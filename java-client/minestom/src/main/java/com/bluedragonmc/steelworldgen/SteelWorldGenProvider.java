package com.bluedragonmc.steelworldgen;

import net.minestom.server.instance.generator.Generator;

public class SteelWorldGenProvider {

    private static final Object lock = new Object();
    private static SteelWorldGenServer server;

    public static void startServer() {
        synchronized (lock) {
            if (server == null) {
                try {
                    server = new SteelWorldGenServer();
                } catch (Exception e) {
                    throw new RuntimeException("Failed to start steel-provider server", e);
                }
            }
        }
    }

    public static Generator getGenerator(long seed) {
        startServer();
        return new SteelWorldGenerator(seed, server);
    }

    public static void closeServer() {
        synchronized (lock) {
            if (server != null) {
                server.close();
                server = null;
            }
        }
    }
}
