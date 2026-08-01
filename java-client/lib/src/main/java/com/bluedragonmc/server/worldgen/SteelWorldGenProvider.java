package com.bluedragonmc.server.worldgen;

import net.minestom.server.instance.generator.Generator;

public class SteelWorldGenProvider {

    private static final Object lock = new Object();
    private static boolean libraryLoaded = false;

    public static void loadNativeLibrary() {
        synchronized (lock) {
            if (!libraryLoaded) {
                NativeLoader.loadLibraryFromJar("/native/libsteel_provider.so", "libsteel_provider.so");
                libraryLoaded = true;
            }
        }
    }

    public static Generator getGenerator(long seed) {
        loadNativeLibrary();
        return new SteelWorldGenerator(seed);
    }
}
