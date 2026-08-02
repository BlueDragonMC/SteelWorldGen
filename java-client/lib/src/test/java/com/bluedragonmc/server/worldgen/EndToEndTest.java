package com.bluedragonmc.server.worldgen;

import net.minestom.server.MinecraftServer;
import net.minestom.server.coordinate.Pos;
import net.minestom.server.instance.Instance;
import org.junit.jupiter.api.Test;

import java.util.concurrent.atomic.AtomicReference;

import static com.bluedragonmc.server.worldgen.steel_provider.header_h.*;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.fail;

class EndToEndTest {
    @Test void loadChunkFromSteelMC() {
        long testStart = System.nanoTime();

        MinecraftServer.init();
        Instance instance = MinecraftServer.getInstanceManager().createInstanceContainer();

        SteelWorldGenProvider.loadNativeLibrary();
        long initStart = System.nanoTime();
        steel_provider_init();
        long initDuration = System.nanoTime() - initStart;

        instance.setGenerator(SteelWorldGenProvider.getGenerator(42L));

        AtomicReference<Throwable> testError = new AtomicReference<>();

        MinecraftServer.getExceptionManager().setExceptionHandler((error) -> {
            testError.set(error);
            error.printStackTrace();
        });

        int viewDistance = 8;
        int loaded = 0;
        long genStart = System.nanoTime();
        for (int cx = -viewDistance; cx <= viewDistance; cx++) {
            for (int cz = -viewDistance; cz <= viewDistance; cz++) {
                instance.loadChunk(new Pos(cx * 16, 64, cz * 16)).join();
                loaded++;
            }
        }
        long genDuration = System.nanoTime() - genStart;

        if (testError.get() != null) {
            fail("Exception thrown during test");
        }

        long totalDuration = System.nanoTime() - testStart;
        System.out.printf("One-time init: %.2f ms%n", initDuration / 1e6);
        System.out.printf("Chunk generation (%d chunks): %.2f ms%n", loaded, genDuration / 1e6);
        System.out.printf("Average per chunk: %.3f ms%n", (genDuration / 1e6) / loaded);
        System.out.printf("Total e2e test: %.2f ms%n", totalDuration / 1e6);
    }

    @Test void crossBorderTreesSpanChunkBoundary() {
        MinecraftServer.init();
        Instance instance = MinecraftServer.getInstanceManager().createInstanceContainer();

        SteelWorldGenProvider.loadNativeLibrary();
        steel_provider_init();
        instance.setGenerator(SteelWorldGenProvider.getGenerator(42L));

        // Seed 42: a dark_oak trunk straddles z=16 between chunks (6,0) and (6,1)
        // at world x=111, y=74..76, and its canopy crosses z=16 at x=82..84, y=77..79.
        instance.loadChunk(new Pos(5 * 16, 64, 0)).join();
        instance.loadChunk(new Pos(5 * 16, 64, 16)).join();
        instance.loadChunk(new Pos(6 * 16, 64, 0)).join();
        instance.loadChunk(new Pos(6 * 16, 64, 16)).join();

        String leftSide = instance.getBlock(111, 75, 15).key().value();
        String rightSide = instance.getBlock(111, 75, 16).key().value();
        assertEquals("dark_oak_log", leftSide, "trunk block south of border");
        assertEquals("dark_oak_log", rightSide, "trunk block north of border");

        assertTrue(instance.getBlock(83, 77, 15).compare(Block.DARK_OAK_LEAVES), "canopy south of border");
        assertTrue(instance.getBlock(83, 77, 16).compare(Block.DARK_OAK_LEAVES), "canopy north of border");
    }
}
