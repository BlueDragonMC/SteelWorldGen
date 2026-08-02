mod c_api;

use std::sync::{Arc, Mutex, Once, Weak};

use glam::IVec3;
use rayon::ThreadPoolBuilder;
use rustc_hash::{FxHashMap, FxHashSet};

use steel_core::behavior::init_behaviors;
use steel_core::block_entity::init_block_entities;
use steel_core::chunk::chunk_access::{ChunkAccess, ChunkStatus};
use steel_core::chunk::chunk_holder::ChunkHolder;
use steel_core::chunk::chunk_pyramid::GENERATION_PYRAMID;
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
use steel_utils::ChunkPos;
use steel_utils::Identifier;
use steel_utils::types::{Difficulty, GameType};
use steel_worldgen::biomes::BiomeSourceKind;

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
    /// Persistent chunk holders to allow feature decorations to write across
    /// chunk borders. In SteelMC's normal server path, the ChunkMap persists
    /// holders between chunk generations. We replicate this by storing holders
    /// in the context and reusing them across generate_with_structures calls.
    holders: Mutex<FxHashMap<(i32, i32), Arc<ChunkHolder>>>,
    /// Tracks which chunks' feature decoration passes have been run.
    /// Each chunk's pass runs exactly once and writes to itself and neighbors.
    decoration_passes_run: Mutex<FxHashSet<(i32, i32)>>,
    /// Per-position mutexes to synchronize concurrent generation of the same chunk.
    /// Cleaned up periodically to prevent unbounded growth.
    generation_mutexes: Mutex<FxHashMap<(i32, i32), Arc<Mutex<()>>>>,
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

        let runtime =
            Arc::new(tokio::runtime::Runtime::new().expect("failed to create Tokio runtime"));

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
            holders: Mutex::new(FxHashMap::default()),
            decoration_passes_run: Mutex::new(FxHashSet::default()),
            generation_mutexes: Mutex::new(FxHashMap::default()),
        }
    }

    /// Create a fresh empty sections array for a full-height chunk.
    fn create_empty_sections(&self) -> Sections {
        Sections::from_owned(
            (0..SECTION_COUNT)
                .map(|_| ChunkSection::new_empty())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        )
    }

    /// Run the generation pipeline (biomes → structures → noise → surface → carvers)
    /// on the given chunk access.
    fn run_generation_pipeline(
        &self,
        chunk_access: &mut ChunkAccess,
        generator: &Arc<ChunkGeneratorType>,
    ) {
        // 1. Biomes
        generator.create_biomes(chunk_access);
        if let ChunkAccess::Proto(p) = chunk_access {
            p.set_status(ChunkStatus::Biomes);
        }

        // 2. Structure starts (need for structure references in noise)
        generator.create_structures(chunk_access);
        if let ChunkAccess::Proto(p) = chunk_access {
            p.set_status(ChunkStatus::StructureStarts);
        }

        // 3. Noise (skip beardifier - only needed for structures)
        generator.fill_from_noise(chunk_access, None);
        if let ChunkAccess::Proto(p) = chunk_access {
            p.set_status(ChunkStatus::Noise);
        }

        // 4. Surface
        let neighbor_biomes = |q: IVec3| -> u16 {
            generator.noise_biome(q.x, q.y, q.z).id() as u16
        };
        generator.build_surface(chunk_access, &neighbor_biomes);
        if let ChunkAccess::Proto(p) = chunk_access {
            p.set_status(ChunkStatus::Surface);
        }

        // 5. Carvers
        generator.apply_carvers(chunk_access);
        if let ChunkAccess::Proto(p) = chunk_access {
            p.set_status(ChunkStatus::Carvers);
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

        let sections = self.create_empty_sections();
        let proto = ProtoChunk::new(sections, pos, MIN_Y, HEIGHT, Weak::new());
        let mut chunk = ChunkAccess::Proto(proto);

        // Run the shared generation pipeline
        self.run_generation_pipeline(&mut chunk, &self.generator);

        let ChunkAccess::Proto(proto) = chunk else {
            unreachable!("chunk is always proto during generation");
        };
        proto
    }

    /// Generate a chunk with full feature decoration including structure
    /// blocks (village houses, etc.), trees, ores, and other features.
    ///
    /// This uses the world's persistent `ChunkMap` to generate a 3×3 area
    /// around the target chunk, ensuring that feature decorations from
    /// neighboring chunks (trees, lava pools, etc.) properly write across
    /// chunk borders. The `ChunkMap` retains holders between calls, so
    /// decorations persist across multiple `generate_with_structures` calls.
    ///
    /// The returned [`ProtoChunk`] can be promoted with [`promote`].
///
/// # Panics
    /// Panics if the Tokio runtime, world, or chunk holders cannot be
    /// created.
    #[must_use]
    pub fn generate_with_structures(&self, chunk_x: i32, chunk_z: i32) -> ProtoChunk {
        let center = ChunkPos::new(chunk_x, chunk_z);
        // Feature decoration writes within radius 1, and each decoration pass
        // reads from neighbors within distance 1. We run passes in a 5x5 area
        // (radius 2) around the target, so we need a 7x7 area (radius 3) of
        // Carvers-status chunks to support all passes' read dependencies.
        const FEATURE_RADIUS: i32 = 3;

        // Get or create holders for the 5x5 neighborhood.
        // We need holders at Carvers status for feature decoration to write into them.
        let mut holders_guard = self.holders.lock().unwrap();
        let mut neighborhood_holders = Vec::new();

        for dx in -FEATURE_RADIUS..=FEATURE_RADIUS {
            for dz in -FEATURE_RADIUS..=FEATURE_RADIUS {
                let pos = (chunk_x + dx, chunk_z + dz);
                let holder = holders_guard
                    .entry(pos)
                    .or_insert_with(|| {
                        Arc::new(ChunkHolder::new(
                            ChunkPos::new(pos.0, pos.1),
                            ChunkTicketLevel::STRONGEST,
                            None,
                            MIN_Y,
                            HEIGHT,
                        ))
                    })
                    .clone();
                neighborhood_holders.push((pos, holder));
            }
        }
        drop(holders_guard);

        // Simple eviction to prevent unbounded growth (keep last ~10000 holders)
        if self.holders.lock().unwrap().len() > 10000 {
            self.holders.lock().unwrap().clear();
        }

        // Clean up old generation mutexes periodically
        if self.generation_mutexes.lock().unwrap().len() > 10000 {
            self.generation_mutexes.lock().unwrap().clear();
        }

        // For each holder in the neighborhood, ensure it's generated up to Carvers status.
        // We do this by running the generation pipeline on any that haven't reached Carvers yet.
        // Use per-position mutex to prevent concurrent generation of the same chunk.
        let generator = &self.generator;

        for (pos, holder) in &neighborhood_holders {
            // Skip if already at Carvers
            if holder.try_chunk(ChunkStatus::Carvers).is_some() {
                continue;
            }
            if holder.persisted_status().is_some() {
                continue;
            }

            // Get or create the mutex for this position
            let gen_mutex = {
                let mut mutexes = self.generation_mutexes.lock().unwrap();
                mutexes
                    .entry(*pos)
                    .or_insert_with(|| Arc::new(Mutex::new(())))
                    .clone()
            };

            // Lock the mutex for this position and check/generate atomically
            let _guard = gen_mutex.lock().unwrap();

            // Double-check after acquiring the lock
            if holder.try_chunk(ChunkStatus::Carvers).is_some() {
                continue;
            }
            if holder.persisted_status().is_some() {
                continue;
            }

            // Generate the chunk up to Carvers
            self.generate_chunk_up_to_carvers(pos.0, pos.1, holder.clone(), generator);
        }

        // Now all neighborhood holders are at least at Carvers status.
        // Run feature decoration passes for each chunk in the 3x3 area around the target chunk.
        // Each chunk's decoration pass runs exactly once and writes to itself
        // and its neighbors (radius 1). We track which passes have run to
        // ensure each pass runs only once across all generate_with_structures calls.
        let feature_step = steel_core::chunk::chunk_pyramid::GENERATION_PYRAMID
            .get_step_to(ChunkStatus::Features);

        // Use live holders directly for both reading and writing.
        // The passes run in a fixed order (dx from -1 to 1, dz from -1 to 1),
        // and each pass runs exactly once (tracked by passes_run).
        // Running a 3x3 area of passes ensures the target chunk and its 8 neighbors
        // receive decorations from their neighbors, since each pass writes to radius 1.
        // The PASS_RADIUS of 1 covers the 3x3 area where features actually write.
        const PASS_RADIUS: i32 = 1;

        // Build a lookup map for the StaticCache2D once (avoids O(n) linear search per pass).
        let holders_map: FxHashMap<(i32, i32), Arc<ChunkHolder>> = neighborhood_holders
            .iter()
            .map(|(pos, h)| (*pos, Arc::clone(h)))
            .collect::<FxHashMap<_, _>>();

        let cache = Arc::new(steel_core::chunk::chunk_generation_task::StaticCache2D::create(
            chunk_x,
            chunk_z,
            FEATURE_RADIUS,
            {
                let holders_map = holders_map.clone();
                move |x, z| match holders_map.get(&(x, z)) {
                    Some(holder) => Arc::clone(holder),
                    None => panic!("Missing feature dependency chunk ({x}, {z})"),
                }
            },
        ));

        // Track which chunks' decoration passes have been run.
        // We use a shared set so passes persist across generate_with_structures calls.
        let mut passes_run = self.decoration_passes_run.lock().unwrap();

        // Run all 9 decoration passes for the 3x3 area around the target chunk.
        // This ensures the target chunk and its 8 neighbors run their passes,
        // allowing trees at chunk boundaries to be generated by either chunk's pass.
        // Forward order (dx=-1..=1, dz=-1..=1) ensures neighbor passes run before
        // the target chunk's pass, allowing the target to write into neighbors.
        for dx in -1..=1 {
            for dz in -1..=1 {
                let center_pos = ChunkPos::new(chunk_x + dx, chunk_z + dz);
                let pass_key = (center_pos.0.x, center_pos.0.y);

                // Run this chunk's decoration pass if it hasn't run yet
                if passes_run.insert(pass_key) {
                    // Use the pre-built map for O(1) lookup instead of linear search
                    let center_holder = holders_map
                        .get(&(center_pos.0.x, center_pos.0.y))
                        .map(|h| Arc::clone(h))
                        .expect("feature neighborhood holder must exist");

                    // Prime final heightmaps for feature placement
                    {
                        let center_chunk = center_holder
                            .try_chunk(ChunkStatus::Carvers)
                            .expect("feature neighborhood chunk must be at Carvers");
                        center_chunk.prime_final_heightmaps();
                    }

                    let region_random = generator.create_worldgen_region_random(
                        self.seed as i64,
                        center_pos,
                    );
                    let mut region = WorldGenRegion::new(
                        &self.world.chunk_map.world_gen_context,
                        feature_step,
                        &cache,
                        center_pos,
                        region_random,
                    );
                    generator.apply_biome_decorations(&mut region);
                }
            }
        }

        // Extract the target chunk from the live holder (decorations already applied)
        let target_holder = neighborhood_holders
            .iter()
            .find(|(p, _)| *p == (chunk_x, chunk_z))
            .map(|(_, h)| Arc::clone(h))
            .expect("target holder must exist");

        let chunk_access = target_holder
            .try_chunk(ChunkStatus::Carvers)
            .expect("chunk must be at least at Carvers status");
        let ChunkAccess::Proto(proto) = &*chunk_access else {
            unreachable!("chunk should still be Proto at Carvers status");
        };

        // Clone sections out of the holder
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
            center,
            MIN_Y,
            HEIGHT,
            Weak::new(),
        )
    }

    /// Generate a single chunk up to Carvers status (noise, surface, carvers).
    /// This is used to initialize holders in the persistent map.
    fn generate_chunk_up_to_carvers(
        &self,
        chunk_x: i32,
        chunk_z: i32,
        holder: Arc<ChunkHolder>,
        generator: &Arc<ChunkGeneratorType>,
    ) {
        let pos = ChunkPos::new(chunk_x, chunk_z);

        // Create a fresh proto chunk and run all generation steps on it locally
        let sections = self.create_empty_sections();
        let proto = ProtoChunk::new(sections, pos, MIN_Y, HEIGHT, Weak::new());
        let mut chunk_access = ChunkAccess::Proto(proto);

        // Run the shared generation pipeline
        self.run_generation_pipeline(&mut chunk_access, generator);

        // Insert the fully generated chunk at Carvers status
        holder.insert_chunk(chunk_access, ChunkStatus::Carvers);
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
    use steel_protocol::packets::game::CLevelChunkWithLight;
    use steel_protocol::utils::ConnectionProtocol;

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
    use steel_registry::blocks::block_state_ext::BlockStateExt;
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

    #[test]
    fn debug_leaf_at_minus27_80_16() {
        initialize();

        let ctx = WorldgenContext::new(42);
        let mut tree_map: std::collections::HashMap<(i32, i32, i32), String> =
            std::collections::HashMap::new();
        for cx in -2..=-2 {
            for cz in 0..=0 {
                let chunk = ctx.generate_with_structures(cx, cz);
                for y in MIN_Y..MIN_Y + HEIGHT {
                    for z in 0..16 {
                        for x in 0..16 {
                            let key = chunk
                                .get_block_state(BlockPos::new(x, y, z))
                                .get_block()
                                .key
                                .to_string();
                            if key.contains("log") || key.contains("leaves") {
                                tree_map.insert((cx * 16 + x, y, cz * 16 + z), key);
                            }
                        }
                    }
                }
            }
        }

        for z in 10..=22 {
            for y in (74..=86).rev() {
                let x = -27;
                let key = tree_map
                    .get(&(x, y, z))
                    .cloned()
                    .unwrap_or_else(|| "air".to_string());
                if key != "air" {
                    println!("world ({x}, {y}, {z}) = {key}");
                }
            }
            println!("--- z={z} scan done");
        }

        println!("=== logs in the area ===");
        let mut logs: Vec<_> = tree_map
            .iter()
            .filter(|((x, _, z), k)| {
                (-40..=-16).contains(x) && (0..=24).contains(z) && k.contains("log")
            })
            .map(|((x, y, z), k)| (x, y, z, k.clone()))
            .collect();
        logs.sort();
        for (x, y, z, k) in logs {
            println!("log at world ({x}, {y}, {z}) = {k}");
        }

        println!("=== leaves at x=-27, z 0..=24, y 70..=90 ===");
        for z in 0..=24 {
            let mut col: Vec<String> = Vec::new();
            for y in (70..=90).rev() {
                if let Some(k) = tree_map.get(&(-27, y, z)) {
                    if k.contains("leaves") {
                        col.push(format!("y{y}:{k}"));
                    }
                }
            }
            if !col.is_empty() {
                println!("x=-27 z={z}: {}", col.join(", "));
            }
        }

        println!("=== canopy box x -34..-24, z 10..=20, y 70..=86 ===");
        for z in 10..=20 {
            let mut line: Vec<String> = Vec::new();
            for y in (70..=86).rev() {
                for x in -34..=-24 {
                    if let Some(k) = tree_map.get(&(x, y, z)) {
                        if k.contains("leaves") {
                            line.push(format!("({x},y{y})"));
                        }
                    }
                }
            }
            if !line.is_empty() {
                println!("z={z}: {}", line.join(" "));
            }
        }

        println!("=== canopy map z 8..=24, x -32..=-22, y 74..=88 ===");
        for z in 8..=24 {
            let mut line: Vec<String> = Vec::new();
            for x in -32..=-22 {
                let mut ys: Vec<i32> = Vec::new();
                for y in 74..=88 {
                    if let Some(k) = tree_map.get(&(x, y, z)) {
                        if k.contains("leaves") {
                            ys.push(y);
                        }
                    }
                }
                if !ys.is_empty() {
                    line.push(format!("x{x}:y{}-{}", ys.first().unwrap(), ys.last().unwrap()));
                }
            }
            if !line.is_empty() {
                println!("z={z}: {}", line.join(" "));
            }
        }

        println!("=== full tree: all blocks x -32..=-24, z 12..=22, y 58..=90 ===");
        let mut all: Vec<_> = tree_map
            .iter()
            .filter(|((x, y, z), k)| {
                (-32..=-24).contains(x) && (12..=22).contains(z) && *y >= 58 && (k.contains("log") || k.contains("leaves"))
            })
            .map(|((x, y, z), k)| (*x, *y, *z, k.clone()))
            .collect();
        all.sort_by_key(|(x, y, z, _)| (*z, *x, *y));
        for (x, y, z, k) in all {
            println!("({x}, {y}, {z}) {k}");
        }
    }
}
