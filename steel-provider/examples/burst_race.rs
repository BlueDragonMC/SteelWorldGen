//! Regression harness for the SteelMC scheduler race: flood a world with
//! concurrent 3x3 `Features` requests (like the 65x65 pregen the
//! SteelWorldGen E2E benchmark drives). Before the batch coordinator in
//! `WorldgenContext`, overlapping concurrent requests panicked inside
//! `ChunkGenerationTask::new`; this harness exits with `panicked=0` only
//! when the fix holds. The full panic also races into the unit test
//! `tests::concurrent_generation_does_not_race`.
//!
//! Use a --release build: the race depends on release timings.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use steel_provider::{Dimension, WorldgenContext};

fn main() -> ExitCode {
    steel_provider::initialize();

    let side_arg = std::env::args()
        .nth(1)
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(65);
    if side_arg < 1 || side_arg % 2 == 0 {
        eprintln!("side must be a positive odd number");
        std::process::exit(2);
    }

    let ctx = Arc::new(WorldgenContext::new(0x5deece66d, Dimension::Overworld));
    let half = side_arg / 2;
    let mut positions = Vec::new();
    for z in -half..=half {
        for x in -half..=half {
            positions.push((x, z));
        }
    }
    let work = Arc::new(positions);
    let next = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // Fire the chunk requests from a bounded pool of workers — like the
    // provider's `steelgen-conn` threads — each grabbing the next position.
    let workers = std::env::args()
        .nth(2)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(32);
    let mut handles = Vec::new();
    for _ in 0..workers {
        let ctx = Arc::clone(&ctx);
        let work = Arc::clone(&work);
        let next = Arc::clone(&next);
        handles.push(std::thread::spawn(move || {
            let mut worked = 0usize;
            loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= work.len() {
                    break;
                }
                let (x, z) = work[i];
                let _ = ctx.generate_with_structures(x, z);
                worked += 1;
            }
            worked
        }));
    }

    let t = Instant::now();
    let mut done = 0usize;
    let mut panicked = 0usize;
    for h in handles {
        match h.join() {
            Ok(worked) => done += worked,
            Err(payload) => {
                panicked += 1;
                println!("WORKER THREAD PANICKED: {payload:?}");
            }
        }
    }
    let elapsed = t.elapsed();
    println!(
        "{side_arg}x{side_arg} = {} chunks (workers={workers}): done={done} panicked={panicked} wall={:.2}s ({:.0} ch/s)",
        work.len(),
        elapsed.as_secs_f64(),
        done as f64 / elapsed.as_secs_f64()
    );

    ExitCode::from(if panicked > 0 { 1 } else { 0 })
}
