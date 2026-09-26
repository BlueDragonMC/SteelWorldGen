//! Native world-generation benchmark for steel-provider.
//!
//! Drives [`WorldgenContext`] exactly like the Java client drives the provider
//! over the wire — one concurrent caller per chunk — while generating a
//! `side x side` square of fully generated chunks. It prints the same
//! "Preparing spawn area" / "Spawn area prepared" markers that Steel's own
//! pregeneration logs, so the benchmark harness measures the identical
//! interval for the provider core as for the full servers.
//!
//! Env: `PREGEN_SIZE` (odd square side, default 101), `BENCH_SEED` (default
//! matches the harness), `WORKERS` (concurrent callers, default
//! `available_parallelism`).

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use steel_provider::{Dimension, WorldgenContext, initialize};

const DEFAULT_SEED: &str = "8500081009970950196";

fn main() {
    install_sigterm_handler();

    let side: i32 = env_or("PREGEN_SIZE", "101")
        .parse()
        .expect("PREGEN_SIZE must be an integer");
    assert!(
        side >= 1 && side % 2 == 1,
        "PREGEN_SIZE must be a positive odd integer"
    );
    let seed: u64 = env_or("BENCH_SEED", DEFAULT_SEED)
        .parse()
        .expect("BENCH_SEED must be an integer");
    let workers: usize = env_or("WORKERS", &default_workers().to_string())
        .parse()
        .expect("WORKERS must be an integer");

    initialize();

    let ctx = Arc::new(WorldgenContext::new(seed, Dimension::Overworld));
    let half = side / 2;
    let total = (side * side) as usize;
    let positions: Vec<(i32, i32)> = (0..side)
        .flat_map(|dz| (0..side).map(move |dx| (dx - half, dz - half)))
        .collect();

    println!("Preparing spawn area: {total} chunks ({side}x{side}) around chunk (0, 0)");
    println!("[steel-provider] {total} chunks, {workers} concurrent callers");
    let _ = std::io::stdout().flush();

    let start = Instant::now();
    let work = Arc::new(positions);
    let next = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let ctx = Arc::clone(&ctx);
        let work = Arc::clone(&work);
        let next = Arc::clone(&next);
        handles.push(std::thread::spawn(move || {
            let mut done = 0usize;
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= work.len() {
                    break;
                }
                let (x, z) = work[i];
                let chunk = ctx.generate_with_structures(x, z);
                let data = steel_provider::serialize_chunk_sections(&chunk);
                assert!(
                    !chunk.sections().sections.is_empty() && !data.is_empty(),
                    "chunk ({x},{z}) must generate sections"
                );
                done += 1;
            }
            done
        }));
    }
    let mut done = 0usize;
    for handle in handles {
        done += handle.join().expect("worker thread panicked");
    }
    assert_eq!(done, total, "every requested chunk must be generated");

    let elapsed = start.elapsed().as_secs_f64();
    let chunks_per_second = total as f64 / elapsed;
    println!(
        "Spawn area prepared: {total} chunks in {elapsed:.2}s ({chunks_per_second:.1} chunks/s)"
    );
    let _ = std::io::stdout().flush();

    // Stay alive like a server until the harness sends SIGTERM, unless a caller
    // asked for a one-shot run (used by profiling tools).
    if std::env::var_os("BENCH_EXIT_AFTER").is_some() {
        return;
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn default_workers() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (cpus * 3).max(2)
}

unsafe extern "C" fn handle_sigterm(_signal: std::os::raw::c_int) {
    // Exit cleanly so the harness records a zero exit code (it SIGTERMs the
    // process once it sees the "Spawn area prepared" marker).
    unsafe {
        libc::_exit(0);
    }
}

fn install_sigterm_handler() {
    unsafe {
        libc::signal(
            libc::SIGTERM,
            handle_sigterm as *const () as libc::sighandler_t,
        );
    }
}
