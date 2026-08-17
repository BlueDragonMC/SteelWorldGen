use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once, OnceLock};
use std::time::Duration;

use rayon::ThreadPoolBuilder;

use steel_core::behavior::init_behaviors;
use steel_core::block_entity::init_block_entities;
use steel_core::chunk::Chunk;
use steel_core::chunk::section::{ChunkSection, SectionHolder, Sections};
use steel_core::chunk::status::ChunkStatus;
use steel_core::entity::init_entities;
use steel_core::level_data::WorldGenerationSettings;
use steel_core::world::{World, WorldConfig, WorldStorageConfig};
use steel_core::worldgen::{ChunkGeneratorType, EndGenerator, NetherGenerator, OverworldGenerator};
use steel_registry::vanilla_dimension_types;
use steel_registry::{REGISTRY, Registry};
use steel_utils::types::{Difficulty, GameType};
use steel_utils::{ChunkPos, Identifier};
use steel_worldgen::biomes::BiomeSourceKind;

mod batch;
use batch::{BatchCoordinator, DriverSignal};

/// The dimension a [`WorldgenContext`] generates chunks for.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dimension {
    Overworld = 0,
    Nether = 1,
    End = 2,
}

impl Dimension {
    /// Decodes a dimension from its one-byte wire representation.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Overworld),
            1 => Some(Self::Nether),
            2 => Some(Self::End),
            _ => None,
        }
    }

    /// Encodes this dimension to its one-byte wire representation.
    #[must_use]
    pub const fn to_byte(self) -> u8 {
        self as u8
    }
}

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

/// Chunk generator for a single (seed, dimension) pair, ready for use.
///
/// Create one per seed and dimension and reuse it for all chunks in that world.
pub struct WorldgenContext {
    /// Coalesces concurrent per-chunk requests into single held requests.
    /// Declared before `world` so its ticket handle is released while the
    /// world is still alive.
    batch: Arc<BatchCoordinator>,
    /// The batch-coordination worker thread (submit / poll / complete).
    batch_worker: Option<std::thread::JoinHandle<()>>,
    world: Arc<World>,
    /// Kept alive so [`Drop`] can drain in-flight chunk-runtime tasks before
    /// the world is dropped.
    runtime: Arc<tokio::runtime::Runtime>,
    /// Steel-core requires `ChunkMap::advance_scheduling` to be driven from a
    /// single thread; this is that thread.
    driver: Option<std::thread::JoinHandle<()>>,
    /// Signals the driver thread to stop on drop.
    driver_stop: Arc<AtomicBool>,
}

impl WorldgenContext {
    /// Create a generator for the given world seed in the given [`Dimension`].
    #[must_use]
    pub fn new(seed: u64, dimension: Dimension) -> Self {
        let generation_pool = Arc::new(
            ThreadPoolBuilder::new()
                .num_threads(
                    std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(4)
                        .min(16),
                )
                .thread_name(|i| format!("steelgen-{i}"))
                .build()
                .expect("failed to create rayon generation pool"),
        );

        let generator = Arc::new(match dimension {
            Dimension::Overworld => ChunkGeneratorType::Overworld(OverworldGenerator::new(
                None,
                BiomeSourceKind::overworld(seed),
                seed,
                &generation_pool,
            )),
            Dimension::Nether => ChunkGeneratorType::Nether(NetherGenerator::new(
                None,
                BiomeSourceKind::nether(seed),
                seed,
                &generation_pool,
            )),
            Dimension::End => ChunkGeneratorType::End(EndGenerator::new(
                None,
                BiomeSourceKind::end(seed),
                seed,
                &generation_pool,
            )),
        });

        let runtime =
            Arc::new(tokio::runtime::Runtime::new().expect("failed to create Tokio runtime"));

        let (dim_type, generator_name, sea_level) = match dimension {
            Dimension::Overworld => (&vanilla_dimension_types::OVERWORLD, "overworld", 63),
            Dimension::Nether => (&vanilla_dimension_types::THE_NETHER, "the_nether", 32),
            Dimension::End => (&vanilla_dimension_types::THE_END, "the_end", 0),
        };
        let generation_settings = WorldGenerationSettings {
            generator: Identifier::vanilla_static(generator_name),
            config: toml::Value::Table(toml::map::Map::new()),
            dimension_type: dim_type.key.clone(),
            min_y: dim_type.min_y,
            height: dim_type.height,
        };
        let world = runtime
            .block_on(World::new_with_config(
                runtime.clone(),
                Identifier::vanilla_static(generator_name),
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
                    sea_level,
                    default_gamemode: GameType::Survival,
                    difficulty: Difficulty::Normal,
                },
                Arc::clone(&generation_pool),
            ))
            .expect("failed to create world");

        let driver_stop = Arc::new(AtomicBool::new(false));
        let driver_signal = DriverSignal::new();
        let driver = {
            let world = Arc::clone(&world);
            let driver_stop = Arc::clone(&driver_stop);
            let signal = Arc::clone(&driver_signal);
            std::thread::Builder::new()
                .name("steelgen-drive".into())
                .spawn(move || {
                    while !driver_stop.load(Ordering::Acquire) {
                        world.chunk_map.advance_scheduling();
                        if signal.idle() {
                            signal.wait(Duration::from_millis(100));
                        } else {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                    }
                })
                .expect("failed to spawn scheduling driver thread")
        };

        let batch = BatchCoordinator::new(world.chunk_map.clone(), Arc::clone(&driver_signal));
        let batch_worker = batch.spawn_worker();

        Self {
            batch,
            batch_worker: Some(batch_worker),
            world,
            runtime,
            driver: Some(driver),
            driver_stop,
        }
    }

    /// Generate a fully decorated chunk at `(chunk_x, chunk_z)`.
    ///
    /// Requests the 3×3 neighborhood at `Features` status via SteelMC's
    /// scheduler so cross-border decorations (trees, structures, ...) are
    /// applied before the target is returned, then blocks until ready. The
    /// returned [`Chunk`] holds the full dimension column (e.g. `y = -64 .. 320`
    /// for the overworld, `y = 0 .. 256` for the nether and the end); read
    /// blocks with [`Chunk::get_block_state`].
    ///
    /// # Panics
    /// Panics if the request is not satisfied within 60 seconds.
    #[must_use]
    pub fn generate_with_structures(&self, chunk_x: i32, chunk_z: i32) -> Chunk {
        let center = ChunkPos::new(chunk_x, chunk_z);
        let holder = self.batch.request(center);
        let chunk = holder
            .try_chunk(ChunkStatus::Features)
            .expect("requested chunk must be at Features status");

        // Finalize before cloning so we copy compact palettes instead of
        // building-mode 8 KB cubes, and so recalculate_counts has no 4096-cell
        // scan to redo.
        finalize_sections(chunk);

        let min_y = chunk.min_y();
        let height = chunk.height();

        // Clone sections out of the holder into a fresh Chunk.
        let sections: Vec<ChunkSection> = chunk
            .sections()
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

        Chunk::new(
            Sections::from_owned(sections.into_boxed_slice()),
            center,
            min_y,
            height,
            Arc::downgrade(&self.world),
        )
    }
}

impl Drop for WorldgenContext {
    fn drop(&mut self) {
        // Stop the batch worker and driver, then drain in-flight chunk-runtime
        // tasks while the world is still alive (steel-core's generator context
        // panics if a task runs after its world has been dropped).
        self.batch.stop();
        if let Some(worker) = self.batch_worker.take() {
            let _ = worker.join();
        }
        self.driver_stop.store(true, Ordering::Release);
        if let Some(driver) = self.driver.take() {
            let _ = driver.join();
        }
        self.world.chunk_map.stop_generation_refill_loop();
        self.world.chunk_map.task_tracker.close();
        let tracker = &self.world.chunk_map.task_tracker;
        if !tracker.is_empty() {
            self.runtime.block_on(tracker.wait());
        }
    }
}

/// Maps SteelMC's biome registry IDs to a canonical alphabetical ordering of
/// biome keys.
static BIOME_TRANSLATION: OnceLock<Vec<u16>> = OnceLock::new();

/// Returns `translation` where `translation[steel_biome_id]` is the
/// alphabetical rank of that biome's key (0-based, all biomes sorted by
/// `namespace:path`). Requires [`initialize`] to have run.
#[must_use]
pub fn biome_translation() -> &'static [u16] {
    BIOME_TRANSLATION.get_or_init(|| {
        let biomes = REGISTRY.biomes.iter();
        let mut entries: Vec<(String, usize)> = biomes
            .map(|(id, biome)| (biome.key.to_string(), id))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut translation = vec![0u16; entries.len()];
        for (rank, (_, id)) in entries.into_iter().enumerate() {
            translation[id] = rank as u16;
        }
        translation
    })
}

/// Rewrites a section's biome palette so every biome ID is the alphabetical
/// rank of its key (see [`biome_translation`]).
fn remap_biomes(section: &SectionHolder, translation: &[u16]) {
    let mut guard = section.write();
    guard.biomes.finalize_building();
    guard.biomes.enter_building_mode();
    for qy in 0..4 {
        for qz in 0..4 {
            for qx in 0..4 {
                let id = guard.biomes.get(qx, qy, qz);
                guard.biomes.set(qx, qy, qz, translation[id as usize]);
            }
        }
    }
    guard.biomes.finalize_building();
}

/// Finalizes every section of `chunk` (block states and biomes) so palettes are
/// compact rather than building-mode 8 KB cubes. Idempotent — no-op on sections
/// that are already finalized.
fn finalize_sections(chunk: &Chunk) {
    for section in &chunk.sections().sections {
        let mut guard = section.write();
        guard.states.finalize_building();
        guard.biomes.finalize_building();
    }
}

/// Serialize a chunk's sections (block states and biomes) into the raw network
/// section byte stream a client-side `ChunkData.Section` reader consumes.
///
/// Each section is finalized (a no-op when already compact) and its counters
/// recounted before writing. Biome IDs are normalized to alphabetical key order
/// (see [`biome_translation`]), making the output identical across builds.
#[must_use]
pub fn serialize_chunk_sections(chunk: &Chunk) -> Vec<u8> {
    let translation = biome_translation();
    let mut cursor = Cursor::new(Vec::new());
    for section in &chunk.sections().sections {
        {
            let mut guard = section.write();
            guard.states.finalize_building();
            guard.biomes.finalize_building();
            guard.recalculate_counts();
        }
        remap_biomes(section, translation);
        section.read().write(&mut cursor);
    }
    cursor.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;
    use steel_registry::RegistryExt;
    use steel_registry::vanilla_blocks;
    use steel_utils::BlockPos;

    #[test]
    fn generate_with_structures_returns_terrain() {
        initialize();

        let ctx = WorldgenContext::new(42, Dimension::Overworld);
        let chunk = ctx.generate_with_structures(0, 0);

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
    fn generate_with_structures_is_repeatable() {
        initialize();

        let ctx = WorldgenContext::new(42, Dimension::Overworld);
        let first = ctx.generate_with_structures(0, 0);
        let second = ctx.generate_with_structures(0, 0);

        let min_y = first.min_y();
        let height = first.height();
        let mut differing = 0_u64;
        for y in (min_y..min_y + height).step_by(1) {
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

        let ctx = WorldgenContext::new(42, Dimension::Overworld);
        let chunk = ctx.generate_with_structures(0, 0);

        let bytes = serialize_chunk_sections(&chunk);
        assert!(
            !bytes.is_empty(),
            "serialized chunk sections must produce some data"
        );
    }

    #[test]
    fn biome_translation_is_total_and_alphabetical() {
        initialize();

        let translation = biome_translation();
        let len = REGISTRY.biomes.len();
        assert_eq!(translation.len(), len);

        // The translation must be a bijection (each rank used exactly once),
        // so every SteelMC biome ID round-trips to exactly one canonical ID.
        let mut seen = vec![false; len];
        for &rank in translation {
            let rank = rank as usize;
            assert!(rank < len, "rank {rank} out of range for {len} biomes");
            assert!(!seen[rank], "rank {rank} assigned to multiple biome IDs");
            seen[rank] = true;
        }

        // Every SteelMC biome ID must map to the alphabetical rank of its key.
        let mut keys: Vec<String> = REGISTRY
            .biomes
            .iter()
            .map(|(_, biome)| biome.key.to_string())
            .collect();
        keys.sort();
        for (id, biome) in REGISTRY.biomes.iter() {
            let expected = keys
                .binary_search(&biome.key.to_string())
                .expect("every registered biome key must be in the sorted list");
            assert_eq!(
                translation[id] as usize, expected,
                "biome {} must map to its alphabetical rank",
                biome.key
            );
        }
    }

    #[test]
    fn serialize_normalizes_every_biome_cell_through_translation() {
        initialize();

        let ctx = WorldgenContext::new(42, Dimension::Overworld);
        let chunk = ctx.generate_with_structures(0, 0);

        // Replace every biome cell with a scrambled but valid registry ID so the
        // translation is genuinely exercised rather than being an identity (which
        // it is on filesystems where SteelMC happens to register alphabetically).
        let n = REGISTRY.biomes.len() as u16;
        let mut before: Vec<u16> = Vec::new();
        for section in &chunk.sections().sections {
            let mut guard = section.write();
            guard.biomes.enter_building_mode();
            for qy in 0..4 {
                for qz in 0..4 {
                    for qx in 0..4 {
                        let scrambled =
                            ((qy as u16 + 1) * 17 + (qz as u16 + 1) * 5 + qx as u16) % n;
                        before.push(scrambled);
                        guard.biomes.set(qx, qy, qz, scrambled);
                    }
                }
            }
            guard.biomes.finalize_building();
        }

        let _ = serialize_chunk_sections(&chunk);

        let translation = biome_translation();
        let mut idx = 0;
        for section in &chunk.sections().sections {
            let guard = section.read();
            for qy in 0..4 {
                for qz in 0..4 {
                    for qx in 0..4 {
                        let after = guard.biomes.get(qx, qy, qz);
                        assert_eq!(
                            after, translation[before[idx] as usize],
                            "serialization must rewrite every biome cell to the alphabetical rank of its key"
                        );
                        idx += 1;
                    }
                }
            }
        }
    }

    #[test]
    fn concurrent_generation_does_not_race() {
        // Contiguous chunks generated from 8 threads: the pattern that used to
        // panic inside SteelMC's `ChunkGenerationTask::new` (see
        // examples/burst_race.rs).
        initialize();

        let ctx = Arc::new(WorldgenContext::new(42, Dimension::Overworld));
        const SIDE: i32 = 9;
        let half = SIDE / 2;
        let positions: Vec<(i32, i32)> = (0..SIDE)
            .flat_map(|dz| (0..SIDE).map(move |dx| (dx - half, dz - half)))
            .collect();
        let work = Arc::new(positions);
        let next = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let ctx = Arc::clone(&ctx);
            let work = Arc::clone(&work);
            let next = Arc::clone(&next);
            handles.push(std::thread::spawn(move || {
                let mut generated = 0usize;
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= work.len() {
                        break;
                    }
                    let (x, z) = work[i];
                    let chunk = ctx.generate_with_structures(x, z);
                    assert!(
                        !chunk.sections().sections.is_empty(),
                        "chunk ({x},{z}) must have generated sections"
                    );
                    generated += 1;
                }
                generated
            }));
        }

        let mut total = 0usize;
        for handle in handles {
            total += handle.join().expect("worker thread panicked");
        }
        assert_eq!(total, work.len(), "every requested chunk must be generated");
    }

    #[test]
    fn nether_and_end_generate_terrain() {
        initialize();

        for dimension in [Dimension::Nether, Dimension::End] {
            let ctx = WorldgenContext::new(42, dimension);
            let chunk = ctx.generate_with_structures(0, 0);

            // The nether and the end both have min_y 0 and height 256 per the
            // dimension types, so a chunk must hold exactly 16 sections.
            let min_y = chunk.min_y();
            let height = chunk.height();
            assert_eq!(min_y, 0, "{dimension:?} chunks must start at y=0");
            assert_eq!(height, 256, "{dimension:?} chunks must be 256 blocks tall");
            assert_eq!(chunk.sections().sections.len(), 16);

            // Spawn chunk must contain solid terrain somewhere.
            let mut solid_blocks = 0_u64;
            for y in min_y..min_y + height {
                let state = chunk.get_block_state(BlockPos::new(0, y, 0));
                if state != vanilla_blocks::AIR.default_state() {
                    solid_blocks += 1;
                }
            }
            assert!(
                solid_blocks > 0,
                "{dimension:?} chunk (0,0) must contain solid terrain"
            );
        }
    }
}
