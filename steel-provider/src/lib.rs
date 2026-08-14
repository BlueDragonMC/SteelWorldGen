use std::io::Cursor;
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

use rayon::ThreadPoolBuilder;

use steel_core::behavior::init_behaviors;
use steel_core::block_entity::init_block_entities;
use steel_core::chunk::Chunk;
use steel_core::chunk::chunk_request::{ChunkRequestState, ChunkTicketKind};
use steel_core::chunk::section::{ChunkSection, SectionHolder, Sections};
use steel_core::chunk::status::ChunkStatus;
use steel_core::entity::init_entities;
use steel_core::level_data::WorldGenerationSettings;
use steel_core::world::{World, WorldConfig, WorldStorageConfig};
use steel_core::worldgen::{ChunkGeneratorType, OverworldGenerator};
use steel_registry::vanilla_dimension_types;
use steel_registry::{REGISTRY, Registry};
use steel_utils::types::{Difficulty, GameType};
use steel_utils::{ChunkPos, Identifier};
use steel_worldgen::biomes::BiomeSourceKind;

const MIN_Y: i32 = -64;
const HEIGHT: i32 = 384;

/// How long a chunk request may take before [`WorldgenContext::generate_with_structures`]
/// gives up. The first request for an area is slow because the scheduler
/// generates the full dependency pyramid (structure starts out to radius 8).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

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
    world: Arc<World>,
    /// Kept alive so [`Drop`] can drain in-flight chunk-runtime tasks before
    /// the world is dropped (steel-core's generator context asserts the world
    /// is still alive when a task runs).
    runtime: Arc<tokio::runtime::Runtime>,
    /// The dedicated scheduling-driver thread. steel-core requires
    /// `ChunkMap::advance_scheduling` to be driven from a single thread; this
    /// thread advances scheduling continuously while concurrent chunk requests
    /// queue tickets and poll for readiness.
    driver: Option<std::thread::JoinHandle<()>>,
    /// Signals the driver thread to stop on drop.
    driver_stop: Arc<std::sync::atomic::AtomicBool>,
    /// Wake-up signal for the driver thread (see [`DriverSignal`]).
    driver_signal: Arc<DriverSignal>,
}

/// Coordination between the driver thread and concurrent chunk requests.
///
/// The driver advances scheduling on a fixed cadence while any request is in
/// flight, but sleeps long when idle so it burns no CPU between bursts. A
/// request that queues fresh tickets bumps `inflight` and wakes the driver
/// immediately so the first chunk of a burst has no added latency.
struct DriverSignal {
    /// Number of `generate_with_structures` calls currently waiting for their
    /// request to become ready.
    inflight: std::sync::atomic::AtomicUsize,
    /// Paired with `mutex` for the driver's timed idle wait.
    cvar: std::sync::Condvar,
    /// Guards the `inflight == 0` check so no wake-up is lost.
    mutex: Mutex<()>,
}

impl WorldgenContext {
    /// Create a generator for the given world seed.
    #[must_use]
    pub fn new(seed: u64) -> Self {
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

        let generator = Arc::new(ChunkGeneratorType::Overworld(OverworldGenerator::new(
            None,
            BiomeSourceKind::overworld(seed),
            seed,
            &generation_pool,
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
                Arc::clone(&generation_pool),
            ))
            .expect("failed to create world");

        let driver_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let driver_signal = Arc::new(DriverSignal {
            inflight: std::sync::atomic::AtomicUsize::new(0),
            cvar: std::sync::Condvar::new(),
            mutex: Mutex::new(()),
        });
        let driver = {
            let world = Arc::clone(&world);
            let driver_stop = Arc::clone(&driver_stop);
            let signal = Arc::clone(&driver_signal);
            std::thread::Builder::new()
                .name("steelgen-drive".into())
                .spawn(move || {
                    while !driver_stop.load(std::sync::atomic::Ordering::Acquire) {
                        world.chunk_map.advance_scheduling();
                        if signal.inflight.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                            let guard = signal.mutex.lock().unwrap();
                            let _ = signal
                                .cvar
                                .wait_timeout(guard, Duration::from_millis(100));
                        } else {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                    }
                })
                .expect("failed to spawn scheduling driver thread")
        };

        Self {
            world,
            runtime,
            driver: Some(driver),
            driver_stop,
            driver_signal,
        }
    }

    /// Generate a fully decorated chunk at `(chunk_x, chunk_z)`.
    ///
    /// Requests a 3×3 area around the target at `Features` status via SteelMC's
    /// chunk scheduler. Requesting the neighborhood (not just the target) makes
    /// the scheduler run the neighboring chunks' feature passes too, so
    /// decorations that write across chunk borders (trees, structures, ...) are
    /// applied before the target is returned. The scheduler walks the full
    /// generation pyramid — structure starts (out to radius 8), biomes, noise,
    /// surface, carvers, features — on its background thread pool; this call
    /// just polls the request until it is ready.
    ///
    /// The returned [`Chunk`] contains all block states and biomes for the full
    /// overworld column (`y = -64 .. 320`). Read blocks with
    /// [`Chunk::get_block_state`].
    ///
    /// # Panics
    /// Panics if the request is not satisfied within [`REQUEST_TIMEOUT`].
    #[must_use]
    pub fn generate_with_structures(&self, chunk_x: i32, chunk_z: i32) -> Chunk {
        let center = ChunkPos::new(chunk_x, chunk_z);

        // Queue a fresh 3×3 request for the target chunk.
        let request = self.world.chunk_map.request_square(
            center,
            1,
            ChunkStatus::Features,
            ChunkTicketKind::Command,
        );
        // Wake the driver immediately for the fresh request.
        let signal = &self.driver_signal;
        let guard = signal.mutex.lock().unwrap();
        let was_zero = signal.inflight.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0;
        if was_zero {
            signal.cvar.notify_one();
        }
        drop(guard);

        // Wait for readiness. The dedicated driver thread advances scheduling;
        // polling and ticket lifecycle are safe to do concurrently.
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            if request.poll() == ChunkRequestState::Ready {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "chunk generation for ({chunk_x}, {chunk_z}) did not finish within {REQUEST_TIMEOUT:?}"
            );
            std::thread::sleep(Duration::from_millis(1));
        }

        let ready = request
            .ready_chunks()
            .expect("request reported ready but no chunks are available");
        let holder = ready
            .holders
            .iter()
            .find(|holder| holder.get_pos() == center)
            .expect("generated neighborhood must contain the requested chunk");
        let chunk = holder
            .try_chunk(ChunkStatus::Features)
            .expect("requested chunk must be at Features status");

        // Finalize the holder's sections before cloning so we copy compact
        // palettes instead of building-mode 8 KB cubes, and so each clone's
        // recalculate_counts has no 4096-cell scan to redo.
        for section in &chunk.sections().sections {
            let mut guard = section.write();
            guard.states.finalize_building();
            guard.biomes.finalize_building();
        }

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

        self.driver_signal
            .inflight
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

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
}

impl Drop for WorldgenContext {
    fn drop(&mut self) {
        // Stop the dedicated driver, then the background refill loop, and drain
        // in-flight chunk-runtime tasks (scheduling epochs, generation tasks,
        // saves) while the world is still alive. steel-core's generator context
        // panics if a task runs after its world has been dropped.
        self.driver_stop.store(true, std::sync::atomic::Ordering::Release);
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
                guard
                    .biomes
                    .set(qx, qy, qz, translation[id as usize]);
            }
        }
    }
    guard.biomes.finalize_building();
}

/// Serialize a chunk's sections (block states and biomes) into the raw network
/// section byte stream that a client-side `ChunkData.Section` reader consumes.
///
/// Each section is finalized and its counters recounted before writing, so this
/// works whether the chunk came from [`WorldgenContext::generate_with_structures`].
///
/// Biome IDs are normalized to alphabetical key order (see
/// [`biome_translation`]), making the output identical across builds.
#[must_use]
pub fn serialize_chunk_sections(chunk: &Chunk) -> Vec<u8> {
    let translation = biome_translation();
    let mut cursor = Cursor::new(Vec::new());
    for section in &chunk.sections().sections {
        section.write().recalculate_counts();
        remap_biomes(section, translation);
        section.read().write(&mut cursor);
    }
    cursor.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;
    use steel_registry::vanilla_blocks;
    use steel_registry::RegistryExt;
    use steel_utils::BlockPos;

    #[test]
    fn generate_with_structures_returns_terrain() {
        initialize();

        let ctx = WorldgenContext::new(42);
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
                translation[id] as usize,
                expected,
                "biome {} must map to its alphabetical rank",
                biome.key
            );
        }
    }

    #[test]
    fn serialize_normalizes_every_biome_cell_through_translation() {
        initialize();

        let ctx = WorldgenContext::new(42);
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
                        let scrambled = ((qy as u16 + 1) * 17 + (qz as u16 + 1) * 5 + qx as u16) % n;
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
                            after,
                            translation[before[idx] as usize],
                            "serialization must rewrite every biome cell to the alphabetical rank of its key"
                        );
                        idx += 1;
                    }
                }
            }
        }
    }
}
