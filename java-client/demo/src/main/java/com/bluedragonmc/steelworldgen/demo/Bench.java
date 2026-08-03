package com.bluedragonmc.steelworldgen.demo;

import com.bluedragonmc.steelworldgen.SteelWorldGenProvider;
import net.minestom.server.MinecraftServer;
import net.minestom.server.coordinate.Pos;
import net.minestom.server.entity.GameMode;
import net.minestom.server.event.player.AsyncPlayerConfigurationEvent;
import net.minestom.server.instance.Chunk;
import net.minestom.server.instance.Instance;
import net.minestom.server.instance.LightingChunk;
import sun.misc.Signal;

import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.locks.LockSupport;

public class Bench {
    static final long DEFAULT_BENCH_SEED = 8500081009970950196L;

    public static void main(String[] args) {
        if (benchmarkSize() > 0) {
            runBenchmark();
        } else {
            runServer();
        }
    }

    static void runServer() {
        MinecraftServer server = MinecraftServer.init();

        Instance instance = MinecraftServer.getInstanceManager().createInstanceContainer();
        instance.setGenerator(SteelWorldGenProvider.getGenerator(42L));
        instance.setChunkSupplier(LightingChunk::new);

        MinecraftServer.getGlobalEventHandler().addListener(AsyncPlayerConfigurationEvent.class, (event) -> {
            event.setSpawningInstance(instance);
            event.getPlayer().setRespawnPoint(new Pos(0, 64, 0));
            event.getPlayer().setGameMode(GameMode.SPECTATOR);
        });

        server.start("0.0.0.0", 25565);
    }

    /**
     * Benchmark mode driven by the Steel benchmark harness (scripts/benchmarks/run.ts).
     * <p>
     * The harness sets {@code PREGEN_SIZE} to an odd square side length in chunks and
     * watches stdout for the same "Preparing spawn area" / "Spawn area prepared" markers
     * that Steel's own pregeneration logs. We generate the square with Minestom (native
     * SteelMC generation + FFI + Minestom parsing + lighting), print the markers, then
     * stay alive until the harness sends SIGTERM, exiting with status 0 so the harness
     * treats the run as successful.
     */
    static void runBenchmark() {
        long seed = Long.parseLong(System.getenv().getOrDefault("BENCH_SEED", String.valueOf(DEFAULT_BENCH_SEED)));
        int side = benchmarkSize();

        MinecraftServer.init();
        Instance instance = MinecraftServer.getInstanceManager().createInstanceContainer();
        instance.setGenerator(SteelWorldGenProvider.getGenerator(seed));
        instance.setChunkSupplier(LightingChunk::new);

        int half = (side - 1) / 2;
        int total = side * side;

        System.out.printf("Preparing spawn area: %d chunks (%dx%d) around chunk (0, 0)%n", total, side, side);
        System.out.flush();

        long start = System.nanoTime();

        List<CompletableFuture<Chunk>> futures = new ArrayList<>(total);
        for (int cx = -half; cx <= half; cx++) {
            for (int cz = -half; cz <= half; cz++) {
                futures.add(instance.loadChunk(cx, cz));
            }
        }
        List<Chunk> chunks = futures.stream().map(CompletableFuture::join).toList();

        LightingChunk.relight(instance, chunks);

        double elapsedSecs = (System.nanoTime() - start) / 1e9;
        double chunksPerSecond = total / elapsedSecs;
        System.out.printf("Spawn area prepared: %d chunks in %.2fs (%.1f chunks/s)%n",
                total, elapsedSecs, chunksPerSecond);
        System.out.flush();

        Signal.handle(new Signal("TERM"), sig -> System.exit(0));
        LockSupport.park();
    }

    static int benchmarkSize() {
        String value = System.getenv("PREGEN_SIZE");
        if (value == null || value.isBlank()) return 0;
        int side = Integer.parseInt(value);
        if (side == 0) return 0;
        if (side < 0 || side % 2 == 0)
            throw new IllegalArgumentException("PREGEN_SIZE must be 0 or a positive odd integer");
        return side;
    }
}
