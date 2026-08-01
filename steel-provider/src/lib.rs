mod c_api;

use std::sync::{Arc, Once, Weak};

use glam::IVec3;
use rayon::ThreadPoolBuilder;
use rustc_hash::FxHashMap;

use steel_core::behavior::init_behaviors;
use steel_core::block_entity::init_block_entities;
use steel_core::chunk::chunk_access::{ChunkAccess, ChunkStatus};
use steel_core::chunk::chunk_generation_task::StaticCache2D;
use steel_core::chunk::chunk_holder::ChunkHolder;
use steel_core::chunk::chunk_pyramid::{GENERATION_PYRAMID};
use steel_core::chunk::chunk_ticket_manager::ChunkTicketLevel;
use steel_core::chunk::level_chunk::{LevelChunk, LevelChunkPromotion};
use steel_core::chunk::proto_chunk::ProtoChunk;
use steel_core::chunk::section::{ChunkSection, Sections};
use steel_core::entity::init_entities;
use steel_core::level_data::WorldGenerationSettings;
use steel_core::world::{World, WorldConfig, WorldStorageConfig};
use steel_core::worldgen::WorldGenRegion;
use steel_core::worldgen::{ChunkGenerator, ChunkGeneratorType, VanillaGenerator};
use steel_registry::vanilla_dimension_types;
use steel_registry::{REGISTRY, Registry, RegistryEntry};
use steel_utils::types::{Difficulty, GameType};
use steel_utils::Identifier;
use steel_utils::ChunkPos;
use steel_worldgen::biomes::BiomeSourceKind;
use steel_worldgen::noise::Beardifier;

const MIN_Y: i32 = -64;
const HEIGHT: i32 = 384;
const SECTION_COUNT: usize = (HEIGHT / 16) as usize;

static INIT: Once = Once::new();

/// Initialize global SteelMC registries and behaviors.
pub fn initialize() {
    INIT.call_once(|| {
        let mut registry = Registry::new_vanilla();
        registry.freeze();
        let _ = REGISTRY.init(registry);
        init_behaviors();
        init_block_entities();
        init_entities();
    });
}

/// Overworld chunk generator ready for use.
///
/// Create one per seed and reuse it for all chunks in that world.
pub struct WorldgenContext {
    generator: Arc<ChunkGeneratorType>,
    world: Arc<World>,
    seed: u64,
}

fn empty_proto_chunk(
    pos: (i32, i32),
    section_count: usize,
    min_y: i32,
    height: i32,
) -> ChunkAccess {
    let sections: Box<[ChunkSection]> = (0..section_count)
        .map(|_| ChunkSection::new_empty())
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let proto = ProtoChunk::new(
        Sections::from_owned(sections),
        ChunkPos::new(pos.0, pos.1),
        min_y,
        height,
        Weak::new(),
    );
    ChunkAccess::Proto(proto)
}

fn chunk_or_panic(
    chunks: &FxHashMap<(i32, i32), ChunkAccess>,
    pos: (i32, i32),
) -> &ChunkAccess {
    match chunks.get(&pos) {
        Some(chunk) => chunk,
        None => panic!("Missing chunk ({}, {})", pos.0, pos.1),
    }
}

fn build_beardifier(
    chunk: &ChunkAccess,
    chunks: &FxHashMap<(i32, i32), ChunkAccess>,
) -> Option<Beardifier> {
    let pos = chunk.pos();
    let chunk_x = pos.0.x;
    let chunk_z = pos.0.y;

    let references = chunk.structure_references();

    let mut source_positions: rustc_hash::FxHashSet<ChunkPos> =
        rustc_hash::FxHashSet::default();
    for source_chunks in references.values() {
        source_positions.extend(source_chunks.iter().copied());
    }
    if source_positions.is_empty() {
        return None;
    }

    let source_chunk_refs: Vec<&ChunkAccess> = source_positions
        .iter()
        .filter_map(|p| chunks.get(&(p.0.x, p.0.y)))
        .collect();
    let mut source_indices: rustc_hash::FxHashMap<ChunkPos, usize> =
        rustc_hash::FxHashMap::default();
    let mut starts_guards = Vec::with_capacity(source_chunk_refs.len());
    for source_chunk in &source_chunk_refs {
        let source_pos = source_chunk.pos();
        source_indices.insert(source_pos, starts_guards.len());
        starts_guards.push(source_chunk.structure_starts());
    }

    let mut starts: Vec<&steel_worldgen::structure::StructureStart> = Vec::new();
    for (structure_id, source_chunks_ref) in references.iter() {
        for &source_pos in source_chunks_ref {
            let Some(&guard_index) = source_indices.get(&source_pos) else {
                continue;
            };
            let guard = &starts_guards[guard_index];
            if let Some(start) = guard.get(structure_id)
                && start.chunk_pos == source_pos
                && start.terrain_adjustment
                    != steel_registry::structure::TerrainAdjustment::None
            {
                starts.push(start);
            }
        }
    }

    if starts.is_empty() {
        return None;
    }

    let beardifier =
        Beardifier::for_structures_in_chunk(starts.iter().copied(), chunk_x, chunk_z);
    (!beardifier.is_empty()).then_some(beardifier)
}

impl WorldgenContext {
    /// Create a generator for the given world seed.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        let thread_pool = Arc::new(
            ThreadPoolBuilder::new()
                .num_threads(1)
                .build()
                .expect("failed to create rayon thread pool"),
        );

        let generator = Arc::new(ChunkGeneratorType::Overworld(VanillaGenerator::new(
            None,
            BiomeSourceKind::overworld(seed),
            seed,
            &thread_pool,
        )));

        let runtime = Arc::new(tokio::runtime::Runtime::new().expect("failed to create Tokio runtime"));

        let dim_type = &vanilla_dimension_types::OVERWORLD;
        let generation_settings = WorldGenerationSettings {
            generator: Identifier::vanilla_static("overworld"),
            config: toml::Value::Table(toml::map::Map::new()),
            dimension_type: dim_type.key.clone(),
            min_y: MIN_Y,
            height: HEIGHT,
        };
        let world = runtime
            .block_on(World::new_with_config(
                runtime.clone(),
                Identifier::vanilla_static("overworld"),
                dim_type,
                seed as i64,
                WorldConfig {
                    storage: WorldStorageConfig::RamOnly,
                    level_data_path: None,
                    generator: generator.clone(),
                    generation_settings,
                    view_distance: 2,
                    simulation_distance: 2,
                    max_chained_neighbor_updates: 1_000_000,
                    compression: None,
                    is_flat: false,
                    sea_level: 63,
                    default_gamemode: GameType::Survival,
                    difficulty: Difficulty::Normal,
                },
                Arc::clone(&thread_pool),
            ))
            .expect("failed to create world");

        // Drop thread_pool and runtime Arc; the World keeps its own Arc
        // clones of both.
        drop(runtime);

        Self {
            generator,
            world,
            seed,
        }
    }

    /// Generate a chunk at `(chunk_x, chunk_z)` using vanilla overworld
    /// generation (biomes → noise → surface → carvers).
    ///
    /// The returned [`ProtoChunk`] contains all block states, biomes,
    /// heightmaps, and structure-start metadata for the full overworld
    /// column (`y = -64 .. 320`).  Read blocks with
    /// [`ProtoChunk::get_block_state`].
    ///
    /// **Structures:** Structure *starts* (markers + bounding boxes) are
    /// generated, but actual structure *blocks* (village houses, etc.)
    /// and feature decoration (trees, ores, etc.) require
    /// [`generate_with_structures`] which generates the neighboring
    /// chunks needed for feature placement.
    ///
    /// **Promotion to [`LevelChunk`]:** Use [`promote`] after generation
    /// to convert the [`ProtoChunk`] into a full [`LevelChunk`].  This
    /// finalises heightmaps, block entity state, and scheduled ticks.
    #[must_use]
    pub fn generate(&self, chunk_x: i32, chunk_z: i32) -> ProtoChunk {
        let pos = ChunkPos::new(chunk_x, chunk_z);

        let sections = Sections::from_owned(
            (0..SECTION_COUNT)
                .map(|_| ChunkSection::new_empty())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );

        let proto = ProtoChunk::new(sections, pos, MIN_Y, HEIGHT, Weak::new());
        let chunk = ChunkAccess::Proto(proto);

        // 1. Biomes
        self.generator.create_biomes(&chunk);
        if let ChunkAccess::Proto(p) = &chunk {
            p.set_status(ChunkStatus::Biomes);
        }

        // 2. Noise terrain + aquifers
        self.generator.fill_from_noise(&chunk, None);
        if let ChunkAccess::Proto(p) = &chunk {
            p.set_status(ChunkStatus::Noise);
        }

        // 3. Structure starts (markers + bounding boxes)
        self.generator.create_structures(&chunk);
        if let ChunkAccess::Proto(p) = &chunk {
            p.set_status(ChunkStatus::StructureStarts);
        }

        // 4. Surface rules (grass, sand, gravel, etc.)
        let neighbor_biomes = |pos: IVec3| -> u16 {
            self.generator
                .noise_biome(pos.x, pos.y, pos.z)
                .id() as u16
        };
        self.generator.build_surface(&chunk, &neighbor_biomes);
        if let ChunkAccess::Proto(p) = &chunk {
            p.set_status(ChunkStatus::Surface);
        }

        // 5. Carvers (caves & canyons)
        self.generator.apply_carvers(&chunk);
        if let ChunkAccess::Proto(p) = &chunk {
            p.set_status(ChunkStatus::Carvers);
        }

        let ChunkAccess::Proto(proto) = chunk else {
            unreachable!("chunk is always proto during generation");
        };
        proto
    }

    /// Generate a chunk with full feature decoration including structure
    /// blocks (village houses, etc.), trees, ores, and other features.
    ///
    /// This generates a 17×17 grid of neighboring chunks through the
    /// structure-starts stage and a 3×3 grid through noise, surface, and
    /// carvers before running feature decoration on the target chunk.
    ///
    /// The returned [`ProtoChunk`] can be promoted with [`promote`].
    ///
    /// # Panics
    /// Panics if the Tokio runtime, world, or chunk holders cannot be
    /// created.
    #[must_use]
    pub fn generate_with_structures(&self, chunk_x: i32, chunk_z: i32) -> ProtoChunk {
        const STRUCTURE_RADIUS: i32 = 8;
        const CARVER_RADIUS: i32 = 1;

        // Collect positions
        let mut starts_positions: rustc_hash::FxHashSet<(i32, i32)> =
            rustc_hash::FxHashSet::default();
        let mut carver_positions: rustc_hash::FxHashSet<(i32, i32)> =
            rustc_hash::FxHashSet::default();
        for dx in -STRUCTURE_RADIUS..=STRUCTURE_RADIUS {
            for dz in -STRUCTURE_RADIUS..=STRUCTURE_RADIUS {
                starts_positions.insert((chunk_x + dx, chunk_z + dz));
            }
        }
        for dx in -CARVER_RADIUS..=CARVER_RADIUS {
            for dz in -CARVER_RADIUS..=CARVER_RADIUS {
                carver_positions.insert((chunk_x + dx, chunk_z + dz));
            }
        }

        // Also need biomes for carver positions + 1 neighbor ring
        let mut biome_positions: rustc_hash::FxHashSet<(i32, i32)> =
            rustc_hash::FxHashSet::default();
        for &(cx, cz) in &carver_positions {
            for dx in -1..=1 {
                for dz in -1..=1 {
                    biome_positions.insert((cx + dx, cz + dz));
                }
            }
        }

        // Feature decoration writes are gated to the step's block-state write
        // radius (1 chunk) around the chunk being decorated, so a chunk
        // receives feature blocks from its own decoration pass *and* from the
        // decoration of its eight direct neighbors. Those neighbor passes
        // write/read up to one chunk beyond their own center, so the holders
        // at distance 2 must already exist at Carvers status (biomes are
        // enough for them; terrain is never emitted from this ring) to accept
        // the cross-border writes without panicking on the dependency gate.
        let ring_positions: rustc_hash::FxHashSet<(i32, i32)> = biome_positions
            .iter()
            .copied()
            .filter(|pos| !carver_positions.contains(pos))
            .collect();

        // Create all proto chunks
        let mut chunks: FxHashMap<(i32, i32), ChunkAccess> =
            FxHashMap::with_capacity_and_hasher(
                starts_positions.len(),
                rustc_hash::FxBuildHasher,
            );
        for &pos in &starts_positions {
            chunks.insert(
                pos,
                empty_proto_chunk(pos, SECTION_COUNT, MIN_Y, HEIGHT),
            );
        }

        // StructureStarts on all 17x17
        for chunk in chunks.values() {
            self.generator.create_structures(chunk);
        }

        // Biomes on needed positions
        for &pos in &biome_positions {
            self.generator
                .create_biomes(chunk_or_panic(&chunks, pos));
        }

        // StructureReferences for carver positions
        for &(target_x, target_z) in &carver_positions {
            let target_block_x = target_x * 16;
            let target_block_z = target_z * 16;
            for source_x in (target_x - STRUCTURE_RADIUS)..=(target_x + STRUCTURE_RADIUS) {
                for source_z in (target_z - STRUCTURE_RADIUS)..=(target_z + STRUCTURE_RADIUS) {
                    let Some(source_chunk) = chunks.get(&(source_x, source_z)) else {
                        continue;
                    };
                    let starts = source_chunk.structure_starts();
                    for (structure_id, start) in starts.iter() {
                        let Some(bb) = start.bounding_box else {
                            continue;
                        };
                        if bb.intersects_xz(
                            target_block_x,
                            target_block_z,
                            target_block_x + 15,
                            target_block_z + 15,
                        ) {
                            chunk_or_panic(&chunks, (target_x, target_z))
                                .structure_references_mut()
                                .entry(structure_id.clone())
                                .or_default()
                                .insert(ChunkPos::new(source_x, source_z));
                        }
                    }
                }
            }
        }

        // Noise on carver positions (with beardifier from references)
        let carver_sorted: Vec<(i32, i32)> = {
            let mut v: Vec<_> = carver_positions.iter().copied().collect();
            v.sort_unstable();
            v
        };
        for &pos in &carver_sorted {
            let chunk = chunk_or_panic(&chunks, pos);
            let beardifier = build_beardifier(chunk, &chunks);
            self.generator
                .fill_from_noise(chunk, beardifier.as_ref());
        }

        // Surface on carver positions
        let min_qy = MIN_Y >> 2;
        let total_quarts_y = SECTION_COUNT * 4;
        for &pos in &carver_sorted {
            let chunk = chunk_or_panic(&chunks, pos);
            let neighbor_biomes = |q: IVec3| -> u16 {
                let cx = q.x >> 2;
                let cz = q.z >> 2;
                let neighbor = chunk_or_panic(&chunks, (cx, cz));
                let sections = neighbor.sections();
                let local_qx = (q.x - cx * 4) as usize;
                let local_qz = (q.z - cz * 4) as usize;
                let qy_clamped =
                    (q.y - min_qy).clamp(0, i32::try_from(total_quarts_y - 1).unwrap_or(i32::MAX))
                        as usize;
                let section_idx = qy_clamped / 4;
                let local_qy = qy_clamped % 4;
                sections.sections[section_idx]
                    .read()
                    .biomes
                    .get(local_qx, local_qy, local_qz)
            };
            self.generator.build_surface(chunk, &neighbor_biomes);
        }

        // Carvers on carver positions
        for &pos in &carver_sorted {
            let chunk = chunk_or_panic(&chunks, pos);
            self.generator.apply_carvers(chunk);
        }

        // Build feature holders (one per starts position; carver subset gets
        // the higher status).
        let all_holder_positions: Vec<(i32, i32)> =
            starts_positions.iter().copied().collect();
        let holders: FxHashMap<(i32, i32), Arc<ChunkHolder>> = all_holder_positions
            .iter()
            .map(|&pos| {
                let holder = Arc::new(ChunkHolder::new(
                    ChunkPos::new(pos.0, pos.1),
                    ChunkTicketLevel::STRONGEST,
                    None,
                    MIN_Y,
                    HEIGHT,
                ));
                let chunk = chunks.remove(&pos).expect("chunk must exist");
                let status = if carver_positions.contains(&pos)
                    || ring_positions.contains(&pos)
                {
                    ChunkStatus::Carvers
                } else {
                    ChunkStatus::StructureStarts
                };
                if let ChunkAccess::Proto(ref proto) = chunk {
                    proto.set_status(status);
                }
                holder.insert_chunk(chunk, status);
                (pos, holder)
            })
            .collect();

        // Prime final heightmaps and decorate the whole write-radius
        // neighborhood. Each decoration pass only writes within 1 chunk of its
        // own center, so decorating the target *and* its eight neighbors with
        // the shared cache makes cross-border feature blocks (tree canopies,
        // etc.) land in the target holder instead of being truncated at chunk
        // borders. Every pass is seeded by its own center, so the result is
        // deterministic regardless of pass order.
        let feature_step = GENERATION_PYRAMID.get_step_to(ChunkStatus::Features);
        let holders_arc =
            Arc::new(holders);
        let cache = Arc::new(StaticCache2D::create(
            chunk_x,
            chunk_z,
            STRUCTURE_RADIUS,
            {
                let holders = Arc::clone(&holders_arc);
                move |x, z| match holders.get(&(x, z)) {
                    Some(holder) => Arc::clone(holder),
                    None => panic!("Missing feature dependency chunk ({x}, {z})"),
                }
            },
        ));

        let decoration_radius = feature_step.block_state_write_radius;
        for dx in -decoration_radius..=decoration_radius {
            for dz in -decoration_radius..=decoration_radius {
                let center_pos = ChunkPos::new(chunk_x + dx, chunk_z + dz);
                let center_holder = Arc::clone(
                    holders_arc
                        .get(&(center_pos.0.x, center_pos.0.y))
                        .expect("feature neighborhood holder must exist"),
                );
                {
                    let center_chunk = center_holder
                        .try_chunk(ChunkStatus::Carvers)
                        .expect("feature neighborhood chunk must be at Carvers");
                    center_chunk.prime_final_heightmaps();
                }

                let region_random = self
                    .generator
                    .create_worldgen_region_random(self.seed as i64, center_pos);
                let mut region = WorldGenRegion::new(
                    &self.world.chunk_map.world_gen_context,
                    feature_step,
                    &cache,
                    center_pos,
                    region_random,
                );
                self.generator.apply_biome_decorations(&mut region);
            }
        }

        // Extract the target chunk from its holder
        let center_holder = Arc::clone(
            holders_arc
                .get(&(chunk_x, chunk_z))
                .expect("center holder must exist"),
        );
        let center_chunk = center_holder
            .try_chunk(ChunkStatus::Empty)
            .expect("center chunk must exist after feature stage");
        let ChunkAccess::Proto(proto) = &*center_chunk else {
            unreachable!("chunk should still be Proto after feature stage");
        };

        // Clone sections out of the holder (take states+biomes palettes,
        // rebuild section metadata from scratch).
        let sections: Vec<ChunkSection> = proto
            .sections
            .sections
            .iter()
            .map(|s| {
                let guard = s.read();
                let states = guard.states.clone();
                let biomes = guard.biomes.clone();
                drop(guard);
                let mut new_section = ChunkSection::new_with_biomes(states, biomes);
                new_section.recalculate_counts();
                new_section
            })
            .collect();

        ProtoChunk::new(
            Sections::from_owned(sections.into_boxed_slice()),
            ChunkPos::new(chunk_x, chunk_z),
            MIN_Y,
            HEIGHT,
            Weak::new(),
        )
    }
}

/// Promote a [`ProtoChunk`] (produced by [`WorldgenContext::generate`]) to a
/// full [`LevelChunk`].
#[must_use]
pub fn promote(proto: ProtoChunk) -> LevelChunkPromotion {
    LevelChunk::from_proto(proto, MIN_Y, HEIGHT, Weak::new())
}

/// Encode a chunk and its light data into raw packet bytes suitable for
/// sending to a Minecraft client.
///
/// The chunk is promoted via [`promote`] first, then serialized into the
/// `CLevelChunkWithLight` packet format.  Light data will be empty (all
/// sections unlit) since the standalone generator does not run the light
/// stage — the client will display the chunk at full darkness.
///
/// `compression` controls whether the packet is compressed (`Some(...)`)
/// or sent uncompressed (`None`).
///
/// # Panics
/// Panics if packet encoding fails (should never happen for a single chunk).
#[must_use]
pub fn encode_chunk_packet(
    chunk: ProtoChunk,
    dimension_has_skylight: bool,
    compression: Option<steel_protocol::packet_traits::CompressionInfo>,
) -> Vec<u8> {
    use steel_protocol::packet_traits::EncodedPacket;
    use steel_protocol::utils::ConnectionProtocol;
    use steel_protocol::packets::game::CLevelChunkWithLight;

    let promoted = promote(chunk);
    let chunk = promoted.chunk;
    let pos = chunk.pos;

    let packet = CLevelChunkWithLight {
        x: pos.0.x,
        z: pos.0.y,
        chunk_data: chunk.extract_chunk_data(),
        light_data: chunk.extract_light_data(dimension_has_skylight),
    };

    let encoded = EncodedPacket::from_bare(packet, compression, ConnectionProtocol::Play)
        .expect("failed to encode chunk packet");
    encoded.encoded_data.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use steel_registry::vanilla_blocks;
    use steel_utils::BlockPos;

    #[test]
    fn generate_chunk_returns_terrain() {
        initialize();

        let ctx = WorldgenContext::new(42);
        let chunk = ctx.generate(0, 0);

        // Above the overworld build limit — should be air
        let top = chunk.get_block_state(BlockPos::new(0, 320, 0));
        assert_eq!(top, vanilla_blocks::AIR.default_state());

        // Scan downward from build limit to find the first non-air block (the surface)
        let mut surface_y = None;
        for y in (0..319).rev() {
            let state = chunk.get_block_state(BlockPos::new(0, y, 0));
            if state != vanilla_blocks::AIR.default_state() {
                surface_y = Some(y);
                break;
            }
        }
        assert!(
            surface_y.is_some(),
            "expected solid terrain somewhere in this chunk"
        );

        // Below min_y — should be air (void)
        let void = chunk.get_block_state(BlockPos::new(0, -65, 0));
        assert_eq!(void, vanilla_blocks::AIR.default_state());
    }

    #[test]
    fn generate_with_structures_returns_terrain() {
        initialize();

        let ctx = WorldgenContext::new(42);
        let chunk = ctx.generate_with_structures(0, 0);

        let mut surface_y = None;
        for y in (0..319).rev() {
            let state = chunk.get_block_state(BlockPos::new(0, y, 0));
            if state != vanilla_blocks::AIR.default_state() {
                surface_y = Some(y);
                break;
            }
        }
        assert!(
            surface_y.is_some(),
            "expected solid terrain somewhere in this chunk"
        );
    }

    #[test]
    fn generate_with_structures_is_repeatable() {
        initialize();

        let ctx = WorldgenContext::new(42);
        let first = ctx.generate_with_structures(0, 0);
        let second = ctx.generate_with_structures(0, 0);

        let mut differing = 0_u64;
        for y in (MIN_Y..MIN_Y + HEIGHT).step_by(1) {
            for z in 0..16 {
                for x in 0..16 {
                    let a = first.get_block_state(BlockPos::new(x, y, z));
                    let b = second.get_block_state(BlockPos::new(x, y, z));
                    if a != b {
                        differing += 1;
                    }
                }
            }
        }
        assert_eq!(
            differing, 0,
            "feature decoration is nondeterministic: {differing} blocks differ between runs"
        );
    }

    #[test]
    fn generate_with_structures_is_order_independent() {
        initialize();

        let positions: Vec<(i32, i32)> = {
            let mut v = Vec::new();
            for cx in -1..=1 {
                for cz in -1..=1 {
                    v.push((cx, cz));
                }
            }
            v
        };

        // Run 1: generate every chunk in forward order, using a fresh context.
        let forward: Vec<ProtoChunk> = {
            let ctx = WorldgenContext::new(42);
            positions
                .iter()
                .map(|&(x, z)| ctx.generate_with_structures(x, z))
                .collect()
        };

        // Run 2: generate every chunk in reverse order, using a fresh context.
        let reverse: Vec<ProtoChunk> = {
            let ctx = WorldgenContext::new(42);
            positions
                .iter()
                .rev()
                .map(|&(x, z)| ctx.generate_with_structures(x, z))
                .collect()
        };

        for (i, &pos) in positions.iter().enumerate() {
            let j = positions.len() - 1 - i;
            assert!(
                chunk_blocks_equal(&forward[i], &reverse[j]),
                "chunk {pos:?} differs when generated before vs after the other chunks"
            );
        }
    }

    fn chunk_blocks_equal(a: &ProtoChunk, b: &ProtoChunk) -> bool {
        for y in MIN_Y..MIN_Y + HEIGHT {
            for z in 0..16 {
                for x in 0..16 {
                    if a.get_block_state(BlockPos::new(x, y, z))
                        != b.get_block_state(BlockPos::new(x, y, z))
                    {
                        return false;
                    }
                }
            }
        }
        true
    }
}
