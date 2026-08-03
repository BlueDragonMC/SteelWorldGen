package com.bluedragonmc.steelworldgen;

import java.io.IOException;
import java.io.InputStream;
import java.net.InetSocketAddress;
import java.net.SocketAddress;
import java.net.StandardProtocolFamily;
import java.net.UnixDomainSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.SocketChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.util.concurrent.TimeUnit;

/**
 * Manages a steel-provider process and the socket protocol used to request chunk
 * sections from it.
 *
 * <p>By default the executable is started on a temporary Unix domain socket. An
 * already-running server can instead be supplied by passing its endpoint (a Unix
 * socket path or an {@code IP:port} pair) to the constructor.
 *
 * <p>See PROTOCOL.md for a detailed description of the network protocol.
 */
public final class SteelWorldGenServer implements AutoCloseable {
    /** JAR resource holding the steel-provider executable. */
    public static final String BINARY_RESOURCE = "/native/steel-provider";

    /** Bytes in a request: seed(8) + chunk_x(4) + chunk_z(4). */
    private static final int REQUEST_BYTES = 16;

    private static final long STARTUP_TIMEOUT_MS = 30_000;
    private static final long PROCESS_STOP_TIMEOUT_MS = 5_000;
    /** Minimum time between restarts of a crashed process, to avoid crash-looping. */
    private static final long RESTART_COOLDOWN_MS = 10_000;

    /** True when this instance spawned the steel-provider process and may restart it. */
    private final boolean ownsProcess;
    /** Extracted steel-provider executable, or null when connecting to an external server. */
    private final Path executable;
    /** Where this client connects (a Unix domain socket or TCP address). */
    private final SocketAddress connectAddress;
    /** Unix socket file the owned process listens on, or null for external servers. */
    private final Path socketPath;

    private final ConnectionPool connections;

    private final Object restartLock = new Object();
    private volatile Process process;
    private volatile long lastRestartNanos;
    private volatile boolean closed;

    /**
     * Extracts the embedded steel-provider executable and starts it on a fresh
     * Unix socket in the system temporary directory.
     *
     * @throws IOException if the executable cannot be extracted, started, or reached.
     */
    public SteelWorldGenServer() throws IOException {
        this(null);
    }

    /**
     * When {@code endpoint} is null or blank, behaves like the no-argument constructor:
     * the embedded executable is extracted and started on a fresh Unix socket. Otherwise
     * the endpoint refers to an already-running steel-provider server (a Unix socket path
     * or an {@code IP:port} pair such as {@code 0.0.0.0:4096}) that this instance connects
     * to without starting a process; the caller owns that server's lifecycle.
     *
     * @param endpoint Unix socket path or {@code IP:port} pair of an existing server, or
     *                 null/blank to start an embedded server.
     * @throws IOException if the executable cannot be extracted, started, or reached.
     */
    public SteelWorldGenServer(String endpoint) throws IOException {
        if (endpoint == null || endpoint.isBlank()) {
            this.ownsProcess = true;
            this.socketPath = Files.createTempFile("steel-provider", ".sock");
            this.socketPath.toFile().deleteOnExit();
            this.connectAddress = UnixDomainSocketAddress.of(this.socketPath);
            this.executable = extractExecutable();
            this.process = spawn();
            try {
                waitForSocket();
            } catch (IOException | RuntimeException e) {
                stopProcess(process);
                deleteSocket();
                throw e;
            }
        } else {
            this.ownsProcess = false;
            this.socketPath = null;
            this.executable = null;
            this.connectAddress = resolveEndpoint(endpoint);
            this.process = null;
            waitForSocket();
        }

        int poolSize = Math.max(2, Math.min(Runtime.getRuntime().availableProcessors(), 16));
        this.connections = new ConnectionPool(poolSize, this::connect);

        if (ownsProcess) {
            Runtime.getRuntime().addShutdownHook(new Thread(this::close, "steel-provider-shutdown"));
        }
    }

    /**
     * Requests the serialized chunk sections for the given chunk, blocking until done.
     *
     * @param seed    world seed
     * @param chunkX  chunk X coordinate
     * @param chunkZ  chunk Z coordinate
     * @return the chunk's sections in Minecraft's network serialization format
     * @throws IOException if the request could not be sent or the process failed
     */
    public byte[] requestChunk(long seed, int chunkX, int chunkZ) throws IOException {
        if (closed) {
            throw new IOException("steel-provider server is closed");
        }
        // If the request fails because an owned process died, restart it and retry once.
        for (int attempt = 0; attempt < 2; attempt++) {
            SocketChannel channel = null;
            try {
                channel = connections.borrow();
                byte[] data = requestChunk(channel, seed, chunkX, chunkZ);
                connections.release(channel);
                return data;
            } catch (IOException | RuntimeException e) {
                if (channel != null) {
                    connections.discard(channel);
                }
                if (attempt == 0 && ownsProcess && !isAlive()) {
                    restart();
                    continue;
                }
                throw e;
            }
        }
        throw new AssertionError("unreachable");
    }

    @Override
    public void close() {
        synchronized (restartLock) {
            if (closed) {
                return;
            }
            closed = true;
            connections.closeAll();
            if (ownsProcess) {
                Process current = process;
                if (current != null) {
                    stopProcess(current);
                }
                deleteSocket();
            }
        }
    }

    /**
     * The currently managed steel-provider process, or null when connected to an
     * external server. Package-private for tests.
     */
    Process process() {
        return process;
    }

    private byte[] requestChunk(SocketChannel channel, long seed, int chunkX, int chunkZ) throws IOException {
        ByteBuffer request = ByteBuffer.allocate(REQUEST_BYTES);
        request.putLong(seed);
        request.putInt(chunkX);
        request.putInt(chunkZ);
        request.flip();
        writeFully(channel, request);

        ByteBuffer length = ByteBuffer.allocate(4);
        readFully(channel, length);
        int payloadLength = length.flip().getInt();

        ByteBuffer payload = ByteBuffer.allocate(payloadLength);
        readFully(channel, payload);
        payload.flip();

        int status = payload.getInt();
        if (status != 0) {
            throw new IOException("steel-provider failed: " + readUtf8(payload));
        }

        byte[] data = new byte[payload.remaining()];
        payload.get(data);
        return data;
    }

    /**
     * Restarts a dead owned process. Rate-limited so a binary that crashes on startup
     * doesn't cause an unbounded restart loop. Any in-flight requests keep using their
     * own connections; this just resets the pool for new ones.
     */
    private void restart() throws IOException {
        synchronized (restartLock) {
            if (closed) {
                throw new IOException("steel-provider server is closed");
            }
            if (isAlive()) {
                return; // another thread already brought it back
            }
            long now = System.nanoTime();
            if (now - lastRestartNanos < TimeUnit.MILLISECONDS.toNanos(RESTART_COOLDOWN_MS)) {
                throw new IOException("steel-provider process died too recently to restart");
            }
            lastRestartNanos = now;
            connections.closeAll();

            Process newProcess = spawn();
            this.process = newProcess;
            try {
                waitForSocket();
            } catch (IOException | RuntimeException e) {
                stopProcess(newProcess);
                throw e;
            }
            if (closed) {
                stopProcess(newProcess);
                throw new IOException("steel-provider server is closed");
            }
        }
    }

    private boolean isAlive() {
        Process current = process;
        return current != null && current.isAlive();
    }

    private Process spawn() throws IOException {
        ProcessBuilder builder = new ProcessBuilder(executable.toString(), socketPath.toString());
        builder.redirectErrorStream(true);
        Process newProcess = builder.start();
        drain(newProcess.getInputStream());
        return newProcess;
    }

    private SocketChannel connect() throws IOException {
        SocketChannel channel = connectAddress instanceof UnixDomainSocketAddress
                ? SocketChannel.open(StandardProtocolFamily.UNIX)
                : SocketChannel.open();
        channel.connect(connectAddress);
        return channel;
    }

    private void waitForSocket() throws IOException {
        long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(STARTUP_TIMEOUT_MS);
        while (System.nanoTime() < deadline) {
            if (ownsProcess && !process.isAlive()) {
                throw new IOException("steel-provider process exited during startup");
            }
            try (SocketChannel probe = connect()) {
                return;
            } catch (IOException ignored) {
                // The listener is not accepting connections yet; retry.
            }
            try {
                Thread.sleep(25);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                throw new IOException("Interrupted while waiting for steel-provider socket", e);
            }
        }
        throw new IOException("Timed out waiting for steel-provider at " + connectAddress);
    }

    /**
     * Returns whether the endpoint looks like a TCP {@code host:port} pair (the part after
     * the last {@code :} is a port number) rather than a Unix socket path. This mirrors the
     * detection in the steel-provider binary's {@code parse_endpoint}.
     */
    private static boolean isTcpEndpoint(String endpoint) {
        int idx = endpoint.lastIndexOf(':');
        if (idx <= 0 || idx == endpoint.length() - 1) {
            return false;
        }
        for (int i = idx + 1; i < endpoint.length(); i++) {
            char c = endpoint.charAt(i);
            if (c < '0' || c > '9') {
                return false;
            }
        }
        return true;
    }

    /**
     * Converts a TCP endpoint the server listens on into the address this client connects
     * to. A wildcard bind address ({@code 0.0.0.0} / {@code ::}) is replaced by loopback,
     * since it cannot itself be connected to.
     */
    private static SocketAddress tcpConnectAddress(String endpoint) throws IOException {
        int idx = endpoint.lastIndexOf(':');
        if (idx <= 0 || idx == endpoint.length() - 1) {
            throw new IOException("Invalid TCP endpoint: " + endpoint);
        }
        String host = endpoint.substring(0, idx);
        String portString = endpoint.substring(idx + 1);
        int port;
        try {
            port = Integer.parseInt(portString);
        } catch (NumberFormatException e) {
            throw new IOException("Invalid TCP endpoint: " + endpoint, e);
        }
        if (host.startsWith("[") && host.endsWith("]") && host.length() > 2) {
            host = host.substring(1, host.length() - 1);
        }
        if (host.equals("0.0.0.0")) {
            host = "127.0.0.1";
        } else if (host.equals("::")) {
            host = "::1";
        }
        try {
            return new InetSocketAddress(host, port);
        } catch (IllegalArgumentException e) {
            throw new IOException("Invalid TCP endpoint: " + endpoint, e);
        }
    }

    private static SocketAddress resolveEndpoint(String endpoint) throws IOException {
        if (isTcpEndpoint(endpoint)) {
            return tcpConnectAddress(endpoint);
        }
        return UnixDomainSocketAddress.of(Path.of(endpoint));
    }

    private static Path extractExecutable() throws IOException {
        try (InputStream in = SteelWorldGenServer.class.getResourceAsStream(BINARY_RESOURCE)) {
            if (in == null) {
                throw new IOException("steel-provider executable not found in JAR: " + BINARY_RESOURCE);
            }
            Path temp = Files.createTempFile("steel-provider", "");
            temp.toFile().deleteOnExit();
            Files.copy(in, temp, StandardCopyOption.REPLACE_EXISTING);
            if (!temp.toFile().setExecutable(true)) {
                throw new IOException("Failed to mark steel-provider executable: " + temp);
            }
            return temp;
        }
    }

    private void stopProcess(Process process) {
        process.destroy();
        try {
            if (!process.waitFor(PROCESS_STOP_TIMEOUT_MS, TimeUnit.MILLISECONDS)) {
                process.destroyForcibly();
                process.waitFor();
            }
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            process.destroyForcibly();
        }
    }

    private void deleteSocket() {
        if (socketPath == null) {
            return;
        }
        try {
            Files.deleteIfExists(socketPath);
        } catch (IOException ignored) {
        }
    }

    private static void drain(InputStream in) {
        Thread thread = new Thread(() -> {
            byte[] buffer = new byte[4096];
            try (InputStream stream = in) {
                int read;
                while ((read = stream.read(buffer)) >= 0) {
                    System.err.write(buffer, 0, read);
                }
            } catch (IOException ignored) {
            }
        }, "steel-provider-output");
        thread.setDaemon(true);
        thread.start();
    }

    private static void writeFully(SocketChannel channel, ByteBuffer buffer) throws IOException {
        while (buffer.hasRemaining()) {
            channel.write(buffer);
        }
    }

    private static void readFully(SocketChannel channel, ByteBuffer buffer) throws IOException {
        while (buffer.hasRemaining()) {
            int read = channel.read(buffer);
            if (read < 0) {
                throw new IOException("steel-provider closed the connection unexpectedly");
            }
        }
    }

    private static String readUtf8(ByteBuffer buffer) {
        byte[] bytes = new byte[buffer.remaining()];
        buffer.get(bytes);
        return new String(bytes, StandardCharsets.UTF_8);
    }
}
