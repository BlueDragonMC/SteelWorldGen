package com.bluedragonmc.server.worldgen;

import com.bluedragonmc.server.worldgen.steel_provider.ByteBuffer;
import net.minestom.server.MinecraftServer;
import net.minestom.server.coordinate.Point;
import net.minestom.server.instance.block.Block;
import net.minestom.server.instance.generator.GenerationUnit;
import net.minestom.server.instance.generator.Generator;
import net.minestom.server.instance.generator.GeneratorImpl;
import net.minestom.server.network.NetworkBuffer;
import net.minestom.server.network.packet.server.play.ChunkDataPacket;
import net.minestom.server.network.packet.server.play.data.ChunkData;
import net.minestom.server.registry.DynamicRegistry;
import net.minestom.server.registry.RegistryKey;
import net.minestom.server.world.biome.Biome;
import org.jspecify.annotations.NonNull;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.ref.Cleaner;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;

import static com.bluedragonmc.server.worldgen.steel_provider.header_h.*;

class SteelWorldGenerator implements Generator, AutoCloseable {
    private final MemorySegment worldgenContext;
    private final Cleaner.Cleanable cleanable;

    private static final Cleaner CLEANER = Cleaner.create();

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

    /**
     * SteelMC biome registry IDs mapped to Minestom biome registry IDs.
     * Built alongside {@link #BIOMES_BY_STEEL_ID}; see that field for the
     * rationale behind the ordering.
     */
    private static final int[] BIOME_IDS_BY_STEEL_ID = buildBiomeIdMapping();

    private static final NetworkBuffer.Type<ChunkData.Section> SECTION_SERIALIZER =
            ChunkData.Section.networkType(MinecraftServer.getBiomeRegistry().size());

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

    private static int[] buildBiomeIdMapping() {
        DynamicRegistry<Biome> registry = MinecraftServer.getBiomeRegistry();
        RegistryKey<Biome>[] keys = BIOMES_BY_STEEL_ID;
        int[] ids = new int[keys.length];
        for (int i = 0; i < keys.length; i++) {
            ids[i] = registry.getId(keys[i]);
        }
        return ids;
    }

    SteelWorldGenerator(long seed) {
        steel_provider_init();
        MemorySegment ctx = steel_provider_worldgen_ctx_new(seed);
        this.worldgenContext = ctx;
        this.cleanable = CLEANER.register(this, () -> steel_provider_worldgen_ctx_free(ctx));
    }

    @Override
    public void close() {
        this.cleanable.clean();
    }

    private ChunkDataPacket generateChunk(int chunkX, int chunkZ) {
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment raw = steel_provider_generate(arena, worldgenContext, chunkX, chunkZ);
            try {
                long lenLong = ByteBuffer.len(raw);
                MemorySegment ptr = ByteBuffer.ptr(raw);

                int len = Math.toIntExact(lenLong);
                MemorySegment data = ptr.reinterpret(len, arena, null);

                NetworkBuffer nb = NetworkBuffer.wrap(data, 0, len, MinecraftServer.getRegistries());
                nb.read(NetworkBuffer.VAR_INT); // Frame length
                nb.read(NetworkBuffer.VAR_INT); // Packet id
                return ChunkDataPacket.SERIALIZER.read(nb);
            } finally {
                steel_provider_bytebuf_free(raw);
            }
        }
    }

    @Override
    public void generate(@NonNull GenerationUnit unit) {
        if (unit.absoluteStart().chunkX() + 1 != unit.absoluteEnd().chunkX() || unit.absoluteStart().chunkZ() + 1 != unit.absoluteEnd().chunkZ()) {
            throw new IllegalArgumentException("Expected generation unit to be a single chunk");
        }
        ChunkDataPacket p = generateChunk(unit.absoluteStart().chunkX(), unit.absoluteStart().chunkZ());

        ChunkData cd = p.chunkData();

        Point start = unit.absoluteStart();
        final int blockX = start.blockX();
        final int blockZ = start.blockZ();
        final int baseBlockY = start.blockY();

        List<GenerationUnit> chunkSections = null;
        if (unit.modifier() instanceof GeneratorImpl.AreaModifierImpl chunkModifier) {
            chunkSections = chunkModifier.sections();
        }

        NetworkBuffer data = NetworkBuffer.wrap(cd.data(), 0, cd.data().length, MinecraftServer.getRegistries());
        int sectionIndex = 0;
        while (data.readableBytes() > 0) {
            ChunkData.Section section = data.read(SECTION_SERIALIZER);

            if (chunkSections != null && sectionIndex < chunkSections.size()
                    && chunkSections.get(sectionIndex).modifier() instanceof GeneratorImpl.SectionModifierImpl sm) {
                sm.genSection().blocks().copyFrom(section.blockStates());
                sm.genSection().biomes().setAll((x, y, z) -> BIOME_IDS_BY_STEEL_ID[section.biomes().get(x, y, z)]);
            } else {
                int baseY = baseBlockY + sectionIndex * 16;
                section.blockStates().getAllPresent((x, y, z, state) ->
                        unit.modifier().setBlock(
                                x + blockX, baseY + y, z + blockZ,
                                Block.fromStateId(state)
                        )
                );
                section.biomes().getAll((x, y, z, biome) ->
                        unit.modifier().setBiome(
                                x * 4 + blockX, baseY + y * 4, z * 4 + blockZ,
                                BIOMES_BY_STEEL_ID[biome]
                        )
                );
            }
            sectionIndex++;
        }
    }
}
