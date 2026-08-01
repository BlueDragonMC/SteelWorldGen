package com.bluedragonmc.server.worldgen;

import net.minestom.server.MinecraftServer;
import net.minestom.server.coordinate.Pos;
import net.minestom.server.instance.Instance;
import org.junit.jupiter.api.Test;

import java.util.concurrent.atomic.AtomicReference;

import static com.bluedragonmc.server.worldgen.steel_provider.header_h.steel_provider_init;
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
}
