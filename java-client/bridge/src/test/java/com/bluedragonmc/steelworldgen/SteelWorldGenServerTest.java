package com.bluedragonmc.steelworldgen;

import org.junit.jupiter.api.Test;

import java.io.IOException;
import java.io.InputStream;
import java.net.ServerSocket;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.util.concurrent.TimeUnit;

import static org.junit.jupiter.api.Assertions.assertTrue;

class SteelWorldGenServerTest {
    @Test void connectsToExternalServer() throws Exception {
        Path executable = extractTestBinary();
        int port;
        try (ServerSocket socket = new ServerSocket(0)) {
            port = socket.getLocalPort();
        }

        Process process = new ProcessBuilder(executable.toString(), "127.0.0.1:" + port).start();
        drain(process);
        try {
            try (SteelWorldGenServer server = new SteelWorldGenServer("127.0.0.1:" + port)) {
                byte[] chunk = server.requestChunk(42, 0, 0);
                assertTrue(chunk.length > 0, "chunk sections requested over TCP must not be empty");
            }
        } finally {
            process.destroy();
            process.waitFor();
        }
    }

    @Test void restartsOwnedProcessAfterCrash() throws Exception {
        try (SteelWorldGenServer server = new SteelWorldGenServer()) {
            byte[] before = server.requestChunk(42, 0, 0);
            assertTrue(before.length > 0, "server must generate chunks before the crash");

            Process original = server.process();
            original.destroy();
            assertTrue(original.waitFor(5, TimeUnit.SECONDS), "test process should exit on demand");

            byte[] after = server.requestChunk(42, 0, 0);
            assertTrue(after.length > 0, "request after a crash must succeed via a restarted process");
            assertTrue(server.process().isAlive(), "a replacement process must be running");
        }
    }

    private static Path extractTestBinary() throws IOException {
        try (InputStream in = SteelWorldGenServer.class.getResourceAsStream(SteelWorldGenServer.BINARY_RESOURCE)) {
            if (in == null) {
                throw new IOException("steel-provider executable not found in test classpath");
            }
            Path temp = Files.createTempFile("steel-provider-test", "");
            Files.copy(in, temp, StandardCopyOption.REPLACE_EXISTING);
            temp.toFile().setExecutable(true);
            return temp;
        }
    }

    private static void drain(Process process) {
        Thread thread = new Thread(() -> {
            try (InputStream in = process.getInputStream()) {
                byte[] buffer = new byte[4096];
                while (in.read(buffer) >= 0) {
                    // Discard to keep the process's stdout from blocking.
                }
            } catch (IOException ignored) {
            }
        });
        thread.setDaemon(true);
        thread.start();
    }
}
