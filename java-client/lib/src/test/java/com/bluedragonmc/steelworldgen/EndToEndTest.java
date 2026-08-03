package com.bluedragonmc.steelworldgen;

import net.minestom.server.MinecraftServer;
import net.minestom.server.coordinate.Pos;
import net.minestom.server.instance.Instance;
import net.minestom.server.instance.block.Block;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.Test;

import java.io.IOException;
import java.io.InputStream;
import java.net.ServerSocket;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.util.LinkedHashSet;
import java.util.Set;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.fail;

class EndToEndTest {
    @AfterAll
    static void shutdown() {
        SteelWorldGenProvider.closeServer();
    }

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

    @Test void loadChunkFromSteelMC() {
        long testStart = System.nanoTime();

        MinecraftServer.init();
        Instance instance = MinecraftServer.getInstanceManager().createInstanceContainer();

        long initStart = System.nanoTime();
        SteelWorldGenProvider.startServer();
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

    @Test void biomesMapToCorrectVanillaKeys() {
        MinecraftServer.init();
        Instance instance = MinecraftServer.getInstanceManager().createInstanceContainer();

        instance.setGenerator(SteelWorldGenProvider.getGenerator(42L));

        // SteelMC assigns biome IDs alphabetically (badlands=0, ..., plains=40, ...),
        // whereas Minestom's biome registry uses vanilla's own non-alphabetical order
        // (plains=0). If the raw packet IDs were looked up directly against Minestom's
        // registry, every biome would be mis-translated. This test guards that mapping.
        int[][] chunks = {{0, 0}, {1, 0}, {0, 1}, {1, 1}, {4, 0}, {-2, 1}, {3, 3}, {5, 5}, {-5, -3}};
        for (int[] c : chunks) {
            instance.loadChunk(new Pos(c[0] * 16, 64, c[1] * 16)).join();
        }

        Set<String> biomes = new LinkedHashSet<>();
        for (int[] c : chunks) {
            for (int y = -64; y < 320; y += 4) {
                for (int dx = 0; dx < 16; dx += 4) {
                    for (int dz = 0; dz < 16; dz += 4) {
                        var biome = instance.getBiome(c[0] * 16 + dx, y, c[1] * 16 + dz);
                        if (biome != null) biomes.add(biome.key().asString());
                    }
                }
            }
        }

        assertEquals(
                Set.of("minecraft:beach", "minecraft:dark_forest", "minecraft:river", "minecraft:lush_caves"),
                biomes,
                "biomes near spawn (seed 42) must match SteelMC's vanilla biomes, not a misaligned registry lookup"
        );
    }

    @Test void crossBorderTreesSpanChunkBoundary() {
        MinecraftServer.init();
        Instance instance = MinecraftServer.getInstanceManager().createInstanceContainer();

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
