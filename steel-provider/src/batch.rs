//! Coalesces concurrent per-chunk requests into single held `Features` requests.
//!
//! SteelMC's scheduler panics in `ChunkGenerationTask::new` when a chunk's
//! generation task is scheduled while part of its dependency halo (radius 9-10)
//! is being unloaded by a *different*, earlier-finished concurrent request: the
//! unloading holder sits in `unloading_chunks` and the deferred-revival path
//! lags one epoch behind task scheduling. So a finished request's tickets are
//! only released once the next active batch has moved beyond [`ZOMBIE_MARGIN`] —
//! until then the completed request is kept alive as a "zombie" to back the
//! active batch's halo.
//!
//! The coordinator therefore guarantees at most one live request at a time:
//! concurrent callers join the same batch, and a completed batch's request is
//! only released once it can no longer overlap the active batch's halo.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use steel_core::chunk::chunk_holder::ChunkHolder;
use steel_core::chunk::chunk_map::ChunkMap;
use steel_core::chunk::chunk_request::{
    ChunkRequest, ChunkRequestHandle, ChunkRequestState, ChunkTicketKind,
};
use steel_core::chunk::status::ChunkStatus;
use steel_utils::ChunkPos;

/// How long a single chunk request may take before the caller gives up. Queued
/// behind a large area's dependency pyramid a chunk can wait tens of seconds,
/// so this must be generous.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a fresh batch waits for more callers before submitting. Minestom
/// generates from a shared pool, so concurrent callers arrive together; 2 ms
/// catches that burst while being negligible next to a chunk's ~12 ms cost.
const BATCH_GRACE: Duration = Duration::from_millis(2);

/// Hard cap on how long a batch stays open, so a steady stream of joiners can
/// never starve the first caller.
const BATCH_MAX_OPEN: Duration = Duration::from_millis(25);

/// Max chunks coalesced into one request, bounding the held request and the
/// holders it keeps alive.
const BATCH_MAX_MEMBERS: usize = 128;

/// How often the worker re-polls an in-flight request.
const BATCH_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Chebyshev margin applied to both regions when deciding whether a zombie can
/// be released. Generation halos reach radius 10 and a held request's tickets
/// propagate radius 9, so a zombie is kept until the active region is > 20
/// chunks away.
const ZOMBIE_MARGIN: i32 = 11;

/// Wake-up channel for the scheduling driver. The driver advances scheduling
/// on a fixed cadence while a batch holds a live request and sleeps long when
/// idle so it burns no CPU between bursts.
pub(crate) struct DriverSignal {
    /// Number of batches currently holding a live request.
    inflight: std::sync::atomic::AtomicUsize,
    cvar: Condvar,
    mutex: Mutex<()>,
}

impl DriverSignal {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            inflight: std::sync::atomic::AtomicUsize::new(0),
            cvar: Condvar::new(),
            mutex: Mutex::new(()),
        })
    }

    /// True when no batch holds a live request, so the driver may sleep long.
    pub(crate) fn idle(&self) -> bool {
        self.inflight.load(Ordering::Acquire) == 0
    }

    /// Timed wait; returns on wake or when `timeout` elapses.
    pub(crate) fn wait(&self, timeout: Duration) {
        let guard = self.mutex.lock().unwrap();
        let _ = self.cvar.wait_timeout(guard, timeout);
    }

    /// Wake the driver (used when a batch starts holding a live request).
    pub(crate) fn wake(&self) {
        let guard = self.mutex.lock().unwrap();
        self.cvar.notify_one();
        drop(guard);
    }

    /// Mark one live request started; returns true if the driver must wake.
    fn begin(&self) -> bool {
        self.inflight.fetch_add(1, Ordering::Relaxed) == 0
    }

    fn end(&self) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// One caller waiting for a specific chunk. `holder` is filled by the worker
/// once the batch's request reports ready. All access happens while the
/// coordinator's inner lock is held.
struct Waiter {
    center: ChunkPos,
    holder: OnceLock<Arc<ChunkHolder>>,
}

/// A batch still collecting joiners (not yet submitted).
struct OpenBatch {
    opened: Instant,
    last_join: Instant,
    waiters: Vec<Arc<Waiter>>,
}

/// A submitted batch awaiting readiness for all of its chunks.
struct InFlightBatch {
    waiters: Vec<Arc<Waiter>>,
    handle: ChunkRequestHandle,
    /// Bounding box `(min_x, min_z, max_x, max_z)` over the requested positions.
    region: (i32, i32, i32, i32),
}

/// A completed request deliberately still held (see module docs). Released by
/// the next `submit` once the active region has moved beyond [`ZOMBIE_MARGIN`].
struct Zombie {
    /// Never read; kept so its `Drop` (ticket release) runs at the right time.
    #[allow(dead_code)]
    handle: ChunkRequestHandle,
    region: (i32, i32, i32, i32),
}

struct BatchInner {
    open: Option<OpenBatch>,
    in_flight: Option<InFlightBatch>,
    /// Joiners that arrived while a batch was in flight; they become the next
    /// in-flight batch the moment the current one completes.
    queued: Vec<Arc<Waiter>>,
    zombies: Vec<Zombie>,
    stopped: bool,
}

pub(crate) struct BatchCoordinator {
    inner: Mutex<BatchInner>,
    cvar: Condvar,
    chunk_map: Arc<ChunkMap>,
    driver_signal: Arc<DriverSignal>,
}

impl BatchCoordinator {
    pub(crate) fn new(chunk_map: Arc<ChunkMap>, driver_signal: Arc<DriverSignal>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(BatchInner {
                open: None,
                in_flight: None,
                queued: Vec::new(),
                zombies: Vec::new(),
                stopped: false,
            }),
            cvar: Condvar::new(),
            chunk_map,
            driver_signal,
        })
    }

    /// Spawn the worker thread that submits batches and polls them to completion.
    pub(crate) fn spawn_worker(self: &Arc<Self>) -> std::thread::JoinHandle<()> {
        let batch = Arc::clone(self);
        std::thread::Builder::new()
            .name("steelgen-batch".into())
            .spawn(move || while batch.worker_step() {})
            .expect("failed to spawn batch worker thread")
    }

    /// Register `center` in the current batch and block until it is ready.
    pub(crate) fn request(&self, center: ChunkPos) -> Arc<ChunkHolder> {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let waiter = Arc::new(Waiter {
            center,
            holder: OnceLock::new(),
        });

        let mut guard = self.inner.lock().unwrap();
        if let Some(open) = &mut guard.open {
            open.waiters.push(Arc::clone(&waiter));
            open.last_join = Instant::now();
        } else if guard.in_flight.is_some() {
            guard.queued.push(Arc::clone(&waiter));
        } else {
            guard.open = Some(OpenBatch {
                opened: Instant::now(),
                last_join: Instant::now(),
                waiters: vec![Arc::clone(&waiter)],
            });
        }
        self.cvar.notify_all();

        loop {
            if waiter.holder.get().is_some() {
                break;
            }
            if guard.stopped {
                drop(guard);
                panic!("worldgen context dropped while a chunk request was pending");
            }
            if Instant::now() >= deadline {
                drop(guard);
                panic!("chunk generation for {center:?} did not finish within {REQUEST_TIMEOUT:?}");
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            guard = self.cvar.wait_timeout(guard, remaining).unwrap().0;
        }

        waiter
            .holder
            .get()
            .cloned()
            .expect("waiter marked ready without a holder")
    }

    /// Submit `waiters` as the single live request and prune zombies whose
    /// region no longer overlaps this batch's. `guard` is the caller's held
    /// `inner` lock.
    fn submit(&self, waiters: Vec<Arc<Waiter>>, guard: &mut MutexGuard<'_, BatchInner>) {
        let positions = request_positions(&waiters);
        let mut region = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for pos in &positions {
            region.0 = region.0.min(pos.0.x);
            region.1 = region.1.min(pos.0.y);
            region.2 = region.2.max(pos.0.x);
            region.3 = region.3.max(pos.0.y);
        }

        let handle = self.chunk_map.request_chunks(ChunkRequest {
            status: ChunkStatus::Features,
            positions: positions.into_iter().collect(),
            ticket_kind: ChunkTicketKind::Command,
        });

        if self.driver_signal.begin() {
            self.driver_signal.wake();
        }

        guard.in_flight = Some(InFlightBatch {
            waiters,
            handle,
            region,
        });
        guard
            .zombies
            .retain(|zombie| regions_overlap(region, zombie.region));
    }

    /// Advance the coordinator one step. Returns `false` once stopped.
    fn worker_step(&self) -> bool {
        let mut guard = self.inner.lock().unwrap();
        if guard.stopped {
            return false;
        }

        if let Some(flight) = guard.in_flight.take() {
            if flight.handle.poll() == ChunkRequestState::Ready {
                let ready = flight
                    .handle
                    .ready_chunks()
                    .expect("ready request must expose its holders");
                let mut holders = HashMap::with_capacity(ready.holders.len());
                for holder in ready.holders {
                    holders.insert(holder.get_pos(), holder);
                }
                for waiter in &flight.waiters {
                    if let Some(holder) = holders.get(&waiter.center) {
                        let _ = waiter.holder.set(Arc::clone(holder));
                    }
                }
                guard.zombies.push(Zombie {
                    handle: flight.handle,
                    region: flight.region,
                });
                self.driver_signal.end();
                if !guard.queued.is_empty() {
                    let waiters = std::mem::take(&mut guard.queued);
                    self.submit(waiters, &mut guard);
                }
                self.cvar.notify_all();
            } else {
                guard.in_flight = Some(flight);
                let _ = self.cvar.wait_timeout(guard, BATCH_POLL_INTERVAL);
            }
            return true;
        }

        let submit_open = guard.open.as_ref().is_some_and(|open| {
            let now = Instant::now();
            !open.waiters.is_empty()
                && (now.duration_since(open.last_join) >= BATCH_GRACE
                    || now.duration_since(open.opened) >= BATCH_MAX_OPEN
                    || open.waiters.len() >= BATCH_MAX_MEMBERS)
        });
        if submit_open {
            let waiters = guard.open.take().expect("checked above").waiters;
            self.submit(waiters, &mut guard);
            self.cvar.notify_all();
            return true;
        }

        let idle = guard.open.is_none();
        let timeout = if idle {
            Duration::from_millis(50)
        } else {
            BATCH_POLL_INTERVAL
        };
        let _ = self.cvar.wait_timeout(guard, timeout);
        true
    }

    pub(crate) fn stop(&self) {
        let mut guard = self.inner.lock().unwrap();
        guard.stopped = true;
        self.cvar.notify_all();
    }
}

/// The 3×3 `Features` neighborhood for each waiter. Requesting neighbors (not
/// just the center) makes the scheduler run their feature passes too, so
/// cross-border decorations are applied before the center is returned.
fn request_positions(waiters: &[Arc<Waiter>]) -> HashSet<ChunkPos> {
    let mut positions = HashSet::with_capacity(waiters.len() * 9);
    for waiter in waiters {
        for dz in -1..=1 {
            for dx in -1..=1 {
                positions.insert(ChunkPos::new(
                    waiter.center.0.x + dx,
                    waiter.center.0.y + dz,
                ));
            }
        }
    }
    positions
}

/// Whether the region reachable by `active` (its requested area plus the
/// generation halo of both regions) can still overlap `zombie`.
fn regions_overlap(active: (i32, i32, i32, i32), zombie: (i32, i32, i32, i32)) -> bool {
    let (ax0, az0, ax1, az1) = active;
    let (zx0, zz0, zx1, zz1) = zombie;
    ax0 - ZOMBIE_MARGIN <= zx1 + ZOMBIE_MARGIN
        && zx0 - ZOMBIE_MARGIN <= ax1 + ZOMBIE_MARGIN
        && az0 - ZOMBIE_MARGIN <= zz1 + ZOMBIE_MARGIN
        && zz0 - ZOMBIE_MARGIN <= az1 + ZOMBIE_MARGIN
}
