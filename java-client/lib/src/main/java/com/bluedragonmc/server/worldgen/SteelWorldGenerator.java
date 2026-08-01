package com.bluedragonmc.server.worldgen;

import com.bluedragonmc.server.worldgen.steel_provider.ByteBuffer;
import net.minestom.server.MinecraftServer;
import net.minestom.server.instance.block.Block;
import net.minestom.server.instance.generator.GenerationUnit;
import net.minestom.server.instance.generator.Generator;
import net.minestom.server.network.NetworkBuffer;
import net.minestom.server.network.packet.server.play.ChunkDataPacket;
import net.minestom.server.network.packet.server.play.data.ChunkData;
import org.jspecify.annotations.NonNull;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;

import static com.bluedragonmc.server.worldgen.steel_provider.header_h.*;

class SteelWorldGenerator implements Generator, AutoCloseable {
    private final MemorySegment worldgenContext;

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
            section.biomes().getAllPresent((x, y, z, biome) ->
                    unit.modifier().setBiome(
                            x + unit.absoluteStart().blockX(), baseY + y, z + unit.absoluteStart().blockZ(),
                            MinecraftServer.getBiomeRegistry().getKey(biome)
                    )
            );
            sectionIndex++;
        }
    }
}
