mod c_api;

use std::collections::{hash_map::Entry, VecDeque};
use std::sync::{Arc, Mutex, Once, RwLock, Weak};

use glam::IVec3;
use rayon::prelude::*;
use rayon::ThreadPoolBuilder;
use rustc_hash::{FxHashMap, FxHashSet};

use steel_core::behavior::init_behaviors;
use steel_core::block_entity::init_block_entities;
use steel_core::chunk::chunk_access::{ChunkAccess, ChunkStatus};
use steel_core::chunk::chunk_holder::ChunkHolder;
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

/// Number of persistent chunk holders/mutexes kept before eviction. Eviction is
/// gated against in-flight generation (see [`WorldgenContext::eviction_gate`]),
/// so clearing these maps can never strand an entry another call is using.
const EVICTION_THRESHOLD: usize = 10_000;

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
    /// Insertion order of the persistent holders, used for bounded FIFO eviction.
    /// Every entry here corresponds to one `holders` key; eviction pops from the
    /// front (least-recently-created first) until the cache is back under the
    /// threshold instead of clearing the whole map.
    holder_order: Mutex<VecDeque<(i32, i32)>>,
    /// Tracks which chunks' feature decoration passes have been run.
    /// Each chunk's pass runs exactly once and writes to itself and neighbors.
    decoration_passes_run: Mutex<FxHashSet<(i32, i32)>>,
    /// Per-position mutexes to synchronize concurrent generation of the same chunk.
    /// Cleaned up periodically to prevent unbounded growth.
    generation_mutexes: Mutex<FxHashMap<(i32, i32), Arc<Mutex<()>>>>,
    /// Dedicated thread pool used to parallelize the per-call 7x7 chunk
    /// generation. Minestom calls [`WorldgenContext::generate_with_structures`]
    /// concurrently on many virtual threads; funnelling the heavy worldgen work
    /// through this single bounded pool keeps CPU usage predictable while still
    /// parallelizing the many chunk generations inside a single call.
    generation_pool: Arc<rayon::ThreadPool>,
    /// Gates eviction of `holders`/`generation_mutexes` against in-flight
    /// generation. Every call holds a read guard for the duration of its work;
    /// eviction takes the write guard, guaranteeing no other call is using a
    /// holder/mutex at the moment the maps are cleared.
    eviction_gate: RwLock<()>,
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

        let generation_pool = Arc::new(
            ThreadPoolBuilder::new()
                .num_threads(
                    std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(4)
                        .min(16),
                )
                .thread_name(|_| "steelgen".into())
                .build()
                .expect("failed to create rayon generation pool"),
        );

        Self {
            generator,
            world,
            seed,
            holders: Mutex::new(FxHashMap::default()),
            holder_order: Mutex::new(VecDeque::new()),
            decoration_passes_run: Mutex::new(FxHashSet::default()),
            generation_mutexes: Mutex::new(FxHashMap::default()),
            generation_pool,
            eviction_gate: RwLock::new(()),
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
        // Evict stale holders/mutexes before taking the shared generation gate.
        // The write guard excludes concurrent generation, so evicting can never
        // remove an entry another in-flight call is still using. We check only
        // `holders` here: every generation mutex key is a holder key, so the
        // mutex map can never outgrow it. Evict the least-recently-created
        // holders (FIFO) down to half the threshold so the persistent cache
        // survives future lookups instead of being fully cleared (a wholesale
        // clear forces re-generation of every subsequent chunk).
        if self.holders.lock().unwrap().len() > EVICTION_THRESHOLD {
            let _gate = self.eviction_gate.write().unwrap();
            let mut holders = self.holders.lock().unwrap();
            let mut mutexes = self.generation_mutexes.lock().unwrap();
            let mut order = self.holder_order.lock().unwrap();
            while holders.len() > EVICTION_THRESHOLD / 2 {
                let Some(pos) = order.pop_front() else {
                    break;
                };
                if holders.remove(&pos).is_some() {
                    mutexes.remove(&pos);
                }
            }
            // Defensive fallback if the order bookkeeping ever drifts: clearing
            // is still gated, so no in-flight call is affected.
            if holders.len() > EVICTION_THRESHOLD {
                holders.clear();
                mutexes.clear();
                order.clear();
            }
        }

        // Hold the read guard for the whole call so no other call can evict a
        // holder/mutex while this call is still using it.
        let _eviction_gate = self.eviction_gate.read().unwrap();

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
        let mut new_holder_positions = Vec::new();

        for dx in -FEATURE_RADIUS..=FEATURE_RADIUS {
            for dz in -FEATURE_RADIUS..=FEATURE_RADIUS {
                let pos = (chunk_x + dx, chunk_z + dz);
                let holder = match holders_guard.entry(pos) {
                    Entry::Occupied(entry) => entry.get().clone(),
                    Entry::Vacant(entry) => {
                        let holder = Arc::new(ChunkHolder::new(
                            ChunkPos::new(pos.0, pos.1),
                            ChunkTicketLevel::STRONGEST,
                            None,
                            MIN_Y,
                            HEIGHT,
                        ));
                        entry.insert(holder.clone());
                        new_holder_positions.push(pos);
                        holder
                    }
                };
                neighborhood_holders.push((pos, holder));
            }
        }
        drop(holders_guard);

        // Record insertion order after releasing the holders lock so eviction's
        // holders→order lock ordering is never reversed.
        if !new_holder_positions.is_empty() {
            self.holder_order.lock().unwrap().extend(new_holder_positions);
        }

        // For each holder in the neighborhood, ensure it's generated up to Carvers status.
        // We do this by running the generation pipeline on any that haven't reached Carvers yet.
        // Use per-position mutex to prevent concurrent generation of the same chunk.
        // Per-chunk generation inside SteelMC is serial, so generating the 49
        // holders in parallel (each guarded by its position mutex) scales near-
        // linearly across cores.
        let generator = &self.generator;
        let generation_pool = Arc::clone(&self.generation_pool);
        generation_pool.install(|| {
            neighborhood_holders.par_iter().for_each(|(pos, holder)| {
                // Skip if already at Carvers
                if holder.try_chunk(ChunkStatus::Carvers).is_some() {
                    return;
                }
                if holder.persisted_status().is_some() {
                    return;
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
                    return;
                }
                if holder.persisted_status().is_some() {
                    return;
                }

                self.generate_chunk_up_to_carvers(pos.0, pos.1, holder.clone(), generator);
            });
        });

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
        for dx in -PASS_RADIUS..=PASS_RADIUS {
            for dz in -PASS_RADIUS..=PASS_RADIUS {
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
        let target_holder = holders_map
            .get(&(chunk_x, chunk_z))
            .map(|h| Arc::clone(h))
            .expect("target holder must exist");

        let chunk_access = target_holder
            .try_chunk(ChunkStatus::Carvers)
            .expect("chunk must be at least at Carvers status");
        let ChunkAccess::Proto(proto) = &*chunk_access else {
            unreachable!("chunk should still be Proto at Carvers status");
        };

        // Finalize the holder's sections before cloning so we copy compact
        // palettes instead of building-mode 8 KB cubes, and so each clone's
        // recalculate_counts has no 4096-cell scan to redo. Later decoration
        // passes re-enter building mode, so this mainly benefits repeated
        // extractions of an already-decorated chunk.
        for section in &proto.sections.sections {
            let mut guard = section.write();
            guard.states.finalize_building();
            guard.biomes.finalize_building();
        }

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

        // Carry the holder's final heightmaps into the returned proto. The
        // holder's heightmaps are primed before decoration and kept up to date
        // by every decoration write (Carvers-status chunks update final
        // heightmaps), so LevelChunk::from_proto can skip re-scanning the
        // sections for them. If they are absent (e.g. the holder was evicted
        // and regenerated after its pass already ran), from_proto's normal
        // fallback recomputes them, so this is purely an optimization.
        let chunk = ProtoChunk::new(
            Sections::from_owned(sections.into_boxed_slice()),
            center,
            MIN_Y,
            HEIGHT,
            Weak::new(),
        );
        *chunk.heightmaps.write() = proto.heightmaps.read().clone();

        chunk
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
    use steel_core::chunk::heightmap::{HeightmapType, ProtoHeightmaps};
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
    fn extract_light_data_is_empty_when_light_uncomputed() {
        initialize();

        let ctx = WorldgenContext::new(42);
        let chunk = ctx.generate_with_structures(0, 0);
        let promoted = promote(chunk);

        // Light is never computed by the generation pipeline (it stops at
        // Carvers), so the chunk's light sections are all "missing" and
        // extract_light_data must produce all-zero masks and no updates.
        let light = promoted.chunk.extract_light_data(true);
        for mask in [
            &light.sky_y_mask,
            &light.block_y_mask,
            &light.empty_sky_y_mask,
            &light.empty_block_y_mask,
        ] {
            assert!(
                mask.0.iter().all(|word| *word == 0),
                "light mask must have no bits set, got {mask:?}"
            );
        }
        assert!(light.sky_updates.is_empty());
        assert!(light.block_updates.is_empty());
    }

    #[test]
    fn carried_heightmaps_match_fresh_recompute() {
        initialize();

        let ctx = WorldgenContext::new(42);
        let chunk = ctx.generate_with_structures(0, 0);

        // The extracted chunk carries the holder's incrementally-maintained
        // final heightmaps. They must match a fresh scan of the extracted
        // sections; if they diverge, the carried data is stale and rendering
        // would be wrong.
        let carried = chunk.heightmaps.read().clone();
        let mut fresh = ProtoHeightmaps::new();
        fresh.prime_from_sections(
            HeightmapType::final_types(),
            MIN_Y,
            HEIGHT,
            &chunk.sections.sections,
        );

        for hm_type in HeightmapType::final_types() {
            let a = carried.get(*hm_type).map(|h| *h.raw_data());
            let b = fresh.get(*hm_type).map(|h| *h.raw_data());
            assert_eq!(
                a,
                b,
                "carried {hm_type:?} heightmap diverges from a fresh recompute"
            );
        }
    }
}
