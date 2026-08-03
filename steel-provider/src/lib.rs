mod c_api;

use std::collections::{hash_map::Entry, VecDeque};
use std::io::Cursor;
use std::sync::{Arc, Mutex, Once, RwLock};

use glam::IVec3;
use rayon::prelude::*;
use rayon::ThreadPoolBuilder;
use rustc_hash::{FxHashMap, FxHashSet};

use steel_core::behavior::init_behaviors;
use steel_core::block_entity::init_block_entities;
use steel_core::chunk::chunk_holder::ChunkHolder;
use steel_core::chunk::chunk_ticket_manager::ChunkTicketLevel;
use steel_core::chunk::section::{ChunkSection, Sections};
use steel_core::chunk::status::ChunkStatus;
use steel_core::chunk::Chunk;
use steel_core::entity::init_entities;
use steel_core::level_data::WorldGenerationSettings;
use steel_core::world::{World, WorldConfig, WorldStorageConfig};
use steel_core::worldgen::generator::generation_benchmark_support;
use steel_core::worldgen::{
    ChunkGenerator, ChunkGeneratorType, OverworldGenerator, WorldGenRegion,
};
use steel_registry::vanilla_dimension_types;
use steel_registry::{REGISTRY, Registry, RegistryEntry};
use steel_utils::types::{Difficulty, GameType};
use steel_utils::{ChunkPos, Identifier};
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
    /// Dedicated thread pool used to parallelize the per-call neighborhood chunk
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

        let generator = Arc::new(ChunkGeneratorType::Overworld(OverworldGenerator::new(
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
    /// in place on the given chunk.
    ///
    /// This mirrors the stages a regular SteelMC server runs, but on a bare
    /// [`Chunk`] that is never promoted past Carvers. Structure *references*
    /// (needed only to build the noise beardifier) are skipped along with the
    /// beardifier, matching the non-structure path of the real pipeline.
    fn run_generation_pipeline(&self, chunk: &Chunk, generator: &Arc<ChunkGeneratorType>) {
        // 1. Structure starts (need for structure references in noise).
        generator.create_structures(chunk);
        // 2. Biomes.
        generator.create_biomes(chunk);
        // 3. Noise (beardifier is only required for actual structure blocks).
        generation_benchmark_support::fill_from_noise(generator.as_ref(), chunk, None);
        // 4. Surface.
        let neighbor_biomes = |q: IVec3| generator.noise_biome(q.x, q.y, q.z).id() as u16;
        generation_benchmark_support::build_surface(generator.as_ref(), chunk, &neighbor_biomes);
        // 5. Carvers.
        generation_benchmark_support::apply_carvers(generator.as_ref(), chunk);
    }

    /// Generate a chunk at `(chunk_x, chunk_z)` using vanilla world generation
    /// (structure starts → biomes → noise → surface → carvers).
    ///
    /// The returned [`Chunk`] contains all block states, biomes, and heightmaps
    /// for the full overworld column (`y = -64 .. 320`). Read blocks with
    /// [`Chunk::get_block_state`].
    ///
    /// **Structures:** Structure *starts* are generated, but actual structure
    /// *blocks* (village houses, etc.) and feature decoration require
    /// [`generate_with_structures`], which first generates the neighboring
    /// chunks needed for feature placement.
    #[must_use]
    pub fn generate(&self, chunk_x: i32, chunk_z: i32) -> Chunk {
        let pos = ChunkPos::new(chunk_x, chunk_z);
        let chunk = Chunk::new(
            self.create_empty_sections(),
            pos,
            MIN_Y,
            HEIGHT,
            Arc::downgrade(&self.world),
        );

        self.run_generation_pipeline(&chunk, &self.generator);
        chunk
    }

    /// Generate a chunk with full feature decoration including structure
    /// blocks (village houses, etc.), trees, ores, and other features.
    ///
    /// This uses the world's persistent `ChunkMap` to generate a 7×7 area
    /// around the target chunk, ensuring that feature decorations from
    /// neighboring chunks (trees, lava pools, etc.) properly write across
    /// chunk borders. The holders are retained between calls, so decorations
    /// persist across multiple `generate_with_structures` calls.
    ///
    /// The returned [`Chunk`] contains the decorated chunk's block states and
    /// biomes, taken from the live holder.
    ///
    /// # Panics
    /// Panics if the world's chunk holders or generation pool cannot be created.
    #[must_use]
    pub fn generate_with_structures(&self, chunk_x: i32, chunk_z: i32) -> Chunk {
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
        // reads from neighbors within distance 1. We run passes in a 3×3 area
        // (radius 1) around the target, so we need a 7×7 area (radius 3) of
        // Carvers-status chunks to support all passes' read dependencies.
        const FEATURE_RADIUS: i32 = 3;

        // Get or create holders for the 7×7 neighborhood.
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

                self.generate_chunk_up_to_carvers(pos.0, pos.1, holder.clone(), generator);
            });
        });

        // Now all neighborhood holders are at least at Carvers status.
        // Run feature decoration passes for each chunk in the 3×3 area around the target chunk.
        // Each chunk's decoration pass runs exactly once and writes to itself
        // and its neighbors (radius 1). We track which passes have run to
        // ensure each pass runs only once across all generate_with_structures calls.
        let feature_step = steel_core::chunk::chunk_pyramid::GENERATION_PYRAMID
            .get_step_to(ChunkStatus::Features);

        const PASS_RADIUS: i32 = 1;

        // Build a lookup map for the cache once (avoids O(n) linear search per pass).
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

        // Run all 9 decoration passes for the 3×3 area around the target chunk.
        // Forward order (dx=-1..=1, dz=-1..=1) ensures neighbor passes run before
        // the target chunk's pass, allowing the target to write into neighbors.
        for dx in -PASS_RADIUS..=PASS_RADIUS {
            for dz in -PASS_RADIUS..=PASS_RADIUS {
                let center_pos = ChunkPos::new(chunk_x + dx, chunk_z + dz);
                let pass_key = (center_pos.0.x, center_pos.0.y);

                // Run this chunk's decoration pass if it hasn't run yet
                if passes_run.insert(pass_key) {
                    let center_holder = holders_map
                        .get(&(center_pos.0.x, center_pos.0.y))
                        .map(Arc::clone)
                        .expect("feature neighborhood holder must exist");

                    // Prime final heightmaps for feature placement
                    let center_chunk = center_holder
                        .try_chunk(ChunkStatus::Carvers)
                        .expect("feature neighborhood chunk must be at Carvers");
                    center_chunk.prime_final_heightmaps();

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

        // Extract the target chunk from the live holder (decorations already applied).
        let target_holder = holders_map
            .get(&(chunk_x, chunk_z))
            .map(Arc::clone)
            .expect("target holder must exist");
        let chunk = target_holder
            .try_chunk(ChunkStatus::Carvers)
            .expect("chunk must be at least at Carvers status");

        // Finalize the holder's sections before cloning so we copy compact
        // palettes instead of building-mode 8 KB cubes, and so each clone's
        // recalculate_counts has no 4096-cell scan to redo.
        for section in &chunk.sections.sections {
            let mut guard = section.write();
            guard.states.finalize_building();
            guard.biomes.finalize_building();
        }

        // Clone sections out of the holder into a fresh Chunk.
        let sections: Vec<ChunkSection> = chunk
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

        let result = Chunk::new(
            Sections::from_owned(sections.into_boxed_slice()),
            center,
            MIN_Y,
            HEIGHT,
            Arc::downgrade(&self.world),
        );
        result.prime_final_heightmaps();
        result
    }

    /// Generate a single chunk up to Carvers status (structure → biomes → noise →
    /// surface → carvers). This is used to initialize holders in the persistent map.
    fn generate_chunk_up_to_carvers(
        &self,
        chunk_x: i32,
        chunk_z: i32,
        holder: Arc<ChunkHolder>,
        generator: &Arc<ChunkGeneratorType>,
    ) {
        let pos = ChunkPos::new(chunk_x, chunk_z);
        let chunk = Chunk::new(
            self.create_empty_sections(),
            pos,
            MIN_Y,
            HEIGHT,
            Arc::downgrade(&self.world),
        );

        self.run_generation_pipeline(&chunk, generator);

        // Insert the fully generated chunk at Carvers status.
        holder.insert_chunk(chunk, ChunkStatus::Carvers);
    }
}

/// Serialize a chunk's sections (block states and biomes) into the raw network
/// section byte stream that a client-side `ChunkData.Section` reader consumes.
///
/// Each section is finalized and its counters recounted before writing, so this
/// works whether the chunk came from [`WorldgenContext::generate`] or
/// [`WorldgenContext::generate_with_structures`].
#[must_use]
pub fn serialize_chunk_sections(chunk: &Chunk) -> Vec<u8> {
    let mut cursor = Cursor::new(Vec::new());
    for section in &chunk.sections().sections {
        section.write().recalculate_counts();
        section.read().write(&mut cursor);
    }
    cursor.into_inner()
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
    fn serialized_sections_are_not_empty() {
        initialize();

        let ctx = WorldgenContext::new(42);
        let chunk = ctx.generate_with_structures(0, 0);

        let bytes = serialize_chunk_sections(&chunk);
        assert!(
            !bytes.is_empty(),
            "serialized chunk sections must produce some data"
        );
    }
}