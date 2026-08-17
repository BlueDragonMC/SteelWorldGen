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

    /**
     * Returns an overworld chunk generator for the given seed. Shorthand for
     * {@code getGenerator(seed, Dimension.OVERWORLD)}.
     */
    public static Generator getGenerator(long seed) {
        return getGenerator(seed, Dimension.OVERWORLD);
    }

    /**
     * Returns a chunk generator for the given seed and dimension.
     */
    public static Generator getGenerator(long seed, Dimension dimension) {
        startServer();
        return new SteelWorldGenerator(seed, dimension, server);
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
