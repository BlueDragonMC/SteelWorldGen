package com.bluedragonmc.steelworldgen;

import java.io.IOException;
import java.nio.channels.SocketChannel;
import java.util.Set;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.Semaphore;

/**
 * A pool of reusable {@link SocketChannel}s to a single server. It caps the
 * number of concurrently borrowed connections and recycles idle ones instead of
 * opening a fresh socket per request. New connections are created by the supplied
 * factory, so the pool is transport-agnostic (it serves Unix domain sockets and
 * TCP alike).
 */
final class ConnectionPool {
    /** Creates a new connection. */
    @FunctionalInterface
    interface SocketChannelFactory {
        SocketChannel open() throws IOException;
    }

    private final SocketChannelFactory connectionFactory;
    private final Semaphore permits;
    private final BlockingQueue<SocketChannel> idleConnections;
    private final Set<SocketChannel> allConnections;

    ConnectionPool(int maxConnections, SocketChannelFactory connectionFactory) {
        this.connectionFactory = connectionFactory;
        this.permits = new Semaphore(maxConnections);
        this.idleConnections = new ArrayBlockingQueue<>(maxConnections);
        this.allConnections = ConcurrentHashMap.newKeySet();
    }

    /**
     * Returns a connection from the pool, opening a new one if none is idle,
     * blocking until a permit is available.
     *
     * @throws IOException if a new connection could not be opened
     */
    SocketChannel borrow() throws IOException {
        try {
            permits.acquire();
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new IOException("Interrupted while waiting for a steel-provider connection", e);
        }

        try {
            SocketChannel channel = idleConnections.poll();
            if (channel == null || !channel.isOpen()) {
                if (channel != null) {
                    allConnections.remove(channel);
                    closeQuietly(channel);
                }
                channel = connectionFactory.open();
                allConnections.add(channel);
            }
            return channel;
        } catch (IOException e) {
            permits.release();
            throw e;
        }
    }

    /** Returns a working connection to the pool for reuse. */
    void release(SocketChannel channel) {
        if (channel.isOpen() && idleConnections.offer(channel)) {
            // Returned to the pool for reuse.
        } else {
            allConnections.remove(channel);
            closeQuietly(channel);
        }
        permits.release();
    }

    /** Closes a connection that is no longer usable. */
    void discard(SocketChannel channel) {
        allConnections.remove(channel);
        closeQuietly(channel);
        permits.release();
    }

    /** Closes every pooled connection, e.g. when the server restarts or shuts down. */
    void closeAll() {
        for (SocketChannel channel : allConnections) {
            closeQuietly(channel);
        }
        allConnections.clear();
        idleConnections.clear();
    }

    private static void closeQuietly(SocketChannel channel) {
        try {
            channel.close();
        } catch (IOException ignored) {
        }
    }
}
