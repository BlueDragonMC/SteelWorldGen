//! Concurrent chunk requests with retained tickets.
//!
//! Every caller issues its own independent `Features` request; completed
//! requests keep their ticket handles ("retired") until no active request is
//! within [`RETENTION_MARGIN`] of them, so nearby chunks do not unload and get
//! regenerated between related requests. Retaining tickets also keeps the
//! generation neighborhood complete while work is in flight, which is what the
//! old serializing batch worked around.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use steel_core::chunk::chunk_holder::ChunkHolder;
use steel_core::chunk::chunk_map::ChunkMap;
use steel_core::chunk::chunk_request::{
    ChunkRequest, ChunkRequestHandle, ChunkRequestState, ChunkTicketKind,
};
use steel_core::chunk::status::ChunkStatus;
use steel_utils::ChunkPos;

/// Wake-up channel for the scheduling driver. The driver advances scheduling
/// on a fixed cadence while any request is live and sleeps long when idle so it
/// burns no CPU between bursts.
pub(crate) struct DriverSignal {
    /// Number of live requests.
    inflight: AtomicUsize,
    cvar: Condvar,
    mutex: Mutex<()>,
}

impl DriverSignal {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            inflight: AtomicUsize::new(0),
            cvar: Condvar::new(),
            mutex: Mutex::new(()),
        })
    }

    /// True when no request is live, so the driver may sleep long.
    pub(crate) fn idle(&self) -> bool {
        self.inflight.load(Ordering::Acquire) == 0
    }

    /// Timed wait; returns on wake or when `timeout` elapses.
    pub(crate) fn wait(&self, timeout: Duration) {
        let guard = self.mutex.lock().unwrap();
        let _ = self.cvar.wait_timeout(guard, timeout);
    }

    /// Wake the driver (used when the first request goes live).
    pub(crate) fn wake(&self) {
        let guard = self.mutex.lock().unwrap();
        self.cvar.notify_one();
        drop(guard);
    }

    /// Mark one request live; returns true if the driver must wake.
    fn begin(&self) -> bool {
        self.inflight.fetch_add(1, Ordering::Relaxed) == 0
    }

    /// Mark one request finished.
    fn end(&self) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// How long a single chunk request may take before the caller gives up.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How often a caller re-polls its request.
const POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Chebyshev margin applied when deciding whether a retired request still
/// overlaps live work. Generation halos reach radius 10.
const RETENTION_MARGIN: i32 = 11;

/// Inclusive chunk-space bounding box `(min_x, min_z, max_x, max_z)`.
type Region = (i32, i32, i32, i32);

/// A completed request whose ticket handle is retained.
struct Retired {
    /// Never read; kept so its `Drop` (ticket release) runs at the right time.
    #[allow(dead_code)]
    handle: ChunkRequestHandle,
    region: Region,
}

#[derive(Default)]
struct Inner {
    /// `(id, region)` for every request currently in flight.
    active: Vec<(u64, Region)>,
    retired: Vec<Retired>,
}

/// Coordinates concurrent chunk requests, retaining tickets to avoid churn.
pub(crate) struct RequestCoordinator {
    chunk_map: Arc<ChunkMap>,
    driver_signal: Arc<DriverSignal>,
    inner: Mutex<Inner>,
    next_id: AtomicU64,
}

impl RequestCoordinator {
    pub(crate) fn new(chunk_map: Arc<ChunkMap>, driver_signal: Arc<DriverSignal>) -> Arc<Self> {
        Arc::new(Self {
            chunk_map,
            driver_signal,
            inner: Mutex::new(Inner::default()),
            next_id: AtomicU64::new(0),
        })
    }

    /// Request the 3×3 `Features` neighborhood around `center` and block until
    /// it is ready, returning the center's holder.
    pub(crate) fn request(&self, center: ChunkPos) -> Arc<ChunkHolder> {
        let region = region_of(center);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner.lock().unwrap().active.push((id, region));

        let mut positions = Vec::with_capacity(9);
        for dz in -1..=1 {
            for dx in -1..=1 {
                positions.push(ChunkPos::new(center.0.x + dx, center.0.y + dz));
            }
        }
        let handle = self.chunk_map.request_chunks(ChunkRequest {
            status: ChunkStatus::Features,
            positions,
            ticket_kind: ChunkTicketKind::Command,
        });
        if self.driver_signal.begin() {
            self.driver_signal.wake();
        }

        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            match handle.poll() {
                ChunkRequestState::Ready => break,
                ChunkRequestState::Cancelled => panic!("chunk request was cancelled"),
                ChunkRequestState::Pending { .. } => {
                    if Instant::now() >= deadline {
                        self.driver_signal.end();
                        panic!(
                            "chunk generation for {center:?} did not finish within {REQUEST_TIMEOUT:?}"
                        );
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
            }
        }
        self.driver_signal.end();

        let ready = handle
            .ready_chunks()
            .expect("ready request must expose its holders");
        let holder = ready
            .holders
            .into_iter()
            .find(|h| h.get_pos() == center)
            .expect("ready request must contain the requested center");

        let mut inner = self.inner.lock().unwrap();
        inner.active.retain(|(active_id, _)| *active_id != id);
        inner.retired.push(Retired { handle, region });
        prune(&mut inner);

        holder
    }
}

/// Drop retired handles that no active request can still need. While nothing is
/// active, all retired handles are kept so a subsequent nearby request cannot
/// race an in-progress unload.
fn prune(inner: &mut Inner) {
    let Inner { active, retired } = inner;
    if active.is_empty() {
        return;
    }
    retired.retain(|r| active.iter().any(|(_, a)| regions_overlap(*a, r.region)));
}

/// The 3×3 region a request for `center` covers.
fn region_of(center: ChunkPos) -> Region {
    (
        center.0.x - 1,
        center.0.y - 1,
        center.0.x + 1,
        center.0.y + 1,
    )
}

/// Whether the generation halos of two regions can still overlap.
fn regions_overlap(a: Region, b: Region) -> bool {
    let (ax0, az0, ax1, az1) = a;
    let (bx0, bz0, bx1, bz1) = b;
    ax0 - RETENTION_MARGIN <= bx1 + RETENTION_MARGIN
        && bx0 - RETENTION_MARGIN <= ax1 + RETENTION_MARGIN
        && az0 - RETENTION_MARGIN <= bz1 + RETENTION_MARGIN
        && bz0 - RETENTION_MARGIN <= az1 + RETENTION_MARGIN
}
