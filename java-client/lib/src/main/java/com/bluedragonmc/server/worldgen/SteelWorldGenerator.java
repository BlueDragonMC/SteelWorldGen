package com.bluedragonmc.server.worldgen;

import com.bluedragonmc.server.worldgen.steel_provider.ByteBuffer;
import net.minestom.server.MinecraftServer;
import net.minestom.server.instance.block.Block;
import net.minestom.server.instance.generator.GenerationUnit;
import net.minestom.server.instance.generator.Generator;
import net.minestom.server.network.NetworkBuffer;
import net.minestom.server.network.packet.server.play.ChunkDataPacket;
import net.minestom.server.network.packet.server.play.data.ChunkData;
import net.minestom.server.registry.DynamicRegistry;
import net.minestom.server.registry.RegistryKey;
import net.minestom.server.world.biome.Biome;
import org.jspecify.annotations.NonNull;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;

import static com.bluedragonmc.server.worldgen.steel_provider.header_h.*;

class SteelWorldGenerator implements Generator, AutoCloseable {
    private final MemorySegment worldgenContext;

    /**
     * Maps SteelMC biome registry IDs to Minestom biome registry keys.
     * <p>
     * SteelMC registers the vanilla biomes in alphabetical order of their keys,
     * so the biome IDs in its chunk packets are alphabetically ordered. Minestom's
     * biome registry instead follows vanilla's own (non-alphabetical) ordering.
     * Looking SteelMC's IDs up directly in Minestom's registry therefore yields
     * the wrong biomes. Sorting Minestom's biome keys alphabetically reproduces
     * SteelMC's ID assignment, giving an exact ID-to-key translation.
     */
    private static final RegistryKey<Biome>[] BIOMES_BY_STEEL_ID = buildBiomeMapping();

    @SuppressWarnings("unchecked")
    private static RegistryKey<Biome>[] buildBiomeMapping() {
        DynamicRegistry<Biome> registry = MinecraftServer.getBiomeRegistry();
        List<RegistryKey<Biome>> keys = new ArrayList<>(registry.size());
        for (int id = 0; id < registry.size(); id++) {
            RegistryKey<Biome> key = registry.getKey(id);
            if (key != null) keys.add(key);
        }
        keys.sort(Comparator.comparing(key -> key.key().asString()));
        return keys.toArray(new RegistryKey[0]);
    }

    SteelWorldGenerator(long seed) {
        steel_provider_init();
        this.worldgenContext = steel_provider_worldgen_ctx_new(seed);
    }

    @Override
    public void close() {
        steel_provider_worldgen_ctx_free(this.worldgenContext);
    }

    @Override
    public void generate(@NonNull GenerationUnit unit) {
        if (unit.absoluteStart().chunkX() + 1 != unit.absoluteEnd().chunkX() || unit.absoluteStart().chunkZ() + 1 != unit.absoluteEnd().chunkZ()) {
            throw new IllegalArgumentException("Expected generation unit to be a single chunk");
        }
        byte[] chunkPacketBytes;
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment raw = steel_provider_generate(arena, worldgenContext, unit.absoluteStart().chunkX(), unit.absoluteStart().chunkZ());

            long lenLong = ByteBuffer.len(raw);
            MemorySegment ptr = ByteBuffer.ptr(raw);

            int len = Math.toIntExact(lenLong);
            chunkPacketBytes = new byte[len];
            MemorySegment.copy(ptr, ValueLayout.JAVA_BYTE, 0, chunkPacketBytes, 0, len);
        }

        NetworkBuffer nb = NetworkBuffer.wrap(chunkPacketBytes, 0, chunkPacketBytes.length, MinecraftServer.getRegistries());
        nb.read(NetworkBuffer.VAR_INT); // Frame length
        nb.read(NetworkBuffer.VAR_INT); // Packet id
        ChunkDataPacket p = ChunkDataPacket.SERIALIZER.read(nb);

        ChunkData cd = p.chunkData();

        NetworkBuffer data = NetworkBuffer.wrap(cd.data(), 0, cd.data().length, MinecraftServer.getRegistries());
        NetworkBuffer.Type<ChunkData.Section> networkType = ChunkData.Section.networkType(MinecraftServer.getBiomeRegistry().size());
        int sectionIndex = 0;
        while (data.readableBytes() > 0) {
            int baseY = unit.absoluteStart().blockY() + sectionIndex * 16;
            ChunkData.Section section = data.read(networkType);
            section.blockStates().getAllPresent((x, y, z, state) ->
                    unit.modifier().setBlock(
                            x + unit.absoluteStart().blockX(), baseY + y, z + unit.absoluteStart().blockZ(),
                            Block.fromStateId(state)
                    )
            );
            section.biomes().getAll((x, y, z, biome) ->
                    unit.modifier().setBiome(
                            x * 4 + unit.absoluteStart().blockX(), baseY + y * 4, z * 4 + unit.absoluteStart().blockZ(),
                            BIOMES_BY_STEEL_ID[biome]
                    )
            );
            sectionIndex++;
        }
    }
}
