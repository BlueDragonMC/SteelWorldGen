//! SteelWorldGen cold-world benchmark suite — own implementation.
//!
//! Orchestrates the whole benchmark: locates/buils the servers, provisions the
//! Fabric baseline, runs the trials (Steel, Fabric, steel-provider, Minestom)
//! with process-CPU/RSS sampling aggregated across each server's process tree,
//! writes machine-readable results, and prints a summary.
//!
//! Licensed under Apache-2.0 — see LICENSE-Apache-2.0 at the repo root.
//!
//! Env overrides (defaults in brackets):
//!   STEEL_REPO   path to a clean SteelMC checkout with target/release/steel
//!                 [defaults to benchmark/.steel, populated by `mise run steel-checkout`]
//!   FABRIC_DIR   Fabric server directory (provisioned if absent) [benchmark/.fabric-server]
//!   PROFILE      all, steel, fabric, steel-provider, minestom, steel-minestom [all]
//!   SIDE         square side in chunks [101]
//!   RUNS         trials per server [3]
//!   WORKERS      concurrent steel-provider callers [32]
//!   OUTPUT       results directory [benchmark/results]
//!   SAMPLE_MS    /proc sampling interval [250]
//!   COOLDOWN     seconds between trials [5]

mod harness;
mod provision;
mod util;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use harness::{Options, Profile};
use provision::{CHUNKY_VERSION, FABRIC_API_VERSION, GAME_VERSION, LOADER_VERSION};
use serde_json::{Value, json};

const DEFAULT_SEED_ENV: &str = "8500081009970950196";

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let repo_root = util::repo_root();
    let steel_repo = resolve_env("STEEL_REPO").unwrap_or_else(find_steel_repo);
    let fabric_dir = resolve_env("FABRIC_DIR")
        .unwrap_or_else(|| repo_root.join("benchmark").join(".fabric-server"));
    let output =
        resolve_env("OUTPUT").unwrap_or_else(|| repo_root.join("benchmark").join("results"));
    let profile = Profile::parse(&env_or("PROFILE", "all"))?;
    let side: i32 = env_or("SIDE", "101")
        .parse()
        .map_err(|_| "SIDE must be an integer".to_string())?;
    if side <= 0 || side % 2 == 0 {
        return Err("SIDE must be a positive odd integer".into());
    }
    let runs: u32 = env_or("RUNS", "3")
        .parse()
        .map_err(|_| "RUNS must be an integer".to_string())?;
    let workers: u32 = env_or("WORKERS", "32")
        .parse()
        .map_err(|_| "WORKERS must be an integer".to_string())?;
    let sample_ms: u64 = env_or("SAMPLE_MS", "250")
        .parse()
        .map_err(|_| "SAMPLE_MS must be an integer".to_string())?;
    let cooldown: u64 = env_or("COOLDOWN", "5").parse().unwrap_or(5);
    if std::env::var("PREGEN_WINDOW_SIZE").is_err() {
        // SAFETY: the process is single-threaded at this point.
        unsafe { std::env::set_var("PREGEN_WINDOW_SIZE", "64") };
    }
    let cache = std::env::temp_dir().join("steel-benchmark-cache");

    let provider_bin = repo_root.join("steel-provider/target/release/bench");
    let demo_jar = repo_root.join("java-client/demo/build/libs/demo-all.jar");

    let opts = Options {
        steel: steel_repo.clone(),
        fabric: fabric_dir.clone(),
        minestom: demo_jar.clone(),
        steel_provider: provider_bin.clone(),
        output: output.clone(),
        runs,
        side,
        workers,
        sample_ms,
        java_min_heap: env_or("JAVA_MIN_HEAP", "512M"),
        java_max_heap: env_or("JAVA_MAX_HEAP", "8G"),
        cache: cache.clone(),
    };

    println!("==> Building steel-provider + demo (release)");
    util::run_inherit(&repo_root, "mise", &["run", "build", "--release"])?;

    if profile.uses_fabric() {
        println!(
            "==> Provisioning Fabric baseline at {}",
            fabric_dir.display()
        );
        provision::provision(&fabric_dir)?;
    }

    for (name, path) in [
        ("Steel binary", steel_repo.join("target/release/steel")),
        ("steel-provider bench binary", provider_bin.clone()),
        ("demo jar", demo_jar.clone()),
    ] {
        if !path.exists() {
            if name == "demo jar" && !profile.uses_demo() {
                continue;
            }
            return Err(format!(
                "Missing {name} at {} — cannot benchmark.",
                path.display()
            ));
        }
    }

    let mut metadata = build_metadata(&opts, &steel_repo)?;
    metadata["minestom_jar_sha512"] = json!(util::file_sha512(&demo_jar).ok());
    metadata["steel_provider_binary_sha512"] = json!(util::file_sha512(&provider_bin).ok());

    let mods = if profile.uses_fabric() {
        provision::benchmark_mods(&opts.cache)?
    } else {
        [PathBuf::new(), PathBuf::new()]
    };

    fs::create_dir_all(&output).map_err(|e| e.to_string())?;
    let mut trials: Vec<Value> = Vec::new();
    let mut all_samples: Vec<Value> = Vec::new();

    for run in 1..=runs {
        let order = harness::trial_order(profile, run);
        let total = order.len();
        for (index, server) in order.into_iter().enumerate() {
            let port = 25700 + run * 2 + index as u32;
            let prepared = harness::prepare(server, &opts, &mods, port)?;
            let trial = harness::run_trial(server, run, &opts, &prepared)?;
            all_samples.extend(trial.samples.iter().cloned());
            trials.push(trial.value.clone());
            write_outputs(&output, &metadata, &trials, &all_samples)?;
            if cooldown > 0 && !(run == runs && index + 1 == total) {
                std::thread::sleep(Duration::from_secs(cooldown));
            }
        }
    }

    println!("Results written to {}", output.display());
    print_summary(&output)?;
    Ok(())
}

fn find_steel_repo() -> PathBuf {
    // The mise `steel-checkout` task clones SteelMC (pinned to the rev in
    // steel-provider/Cargo.toml) into a gitignored dir inside the benchmark.
    util::repo_root().join("benchmark").join(".steel")
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn resolve_env(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn build_metadata(opts: &Options, steel_repo: &Path) -> Result<Value, String> {
    let steel_status =
        util::capture(steel_repo, "git", &["status", "--porcelain"]).unwrap_or_default();
    let steel_diff =
        util::capture_binary(steel_repo, "git", &["diff", "--binary", "HEAD"]).unwrap_or_default();
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let cpu_model = cpuinfo
        .lines()
        .find(|l| l.starts_with("model name"))
        .and_then(|l| l.split_once(':').map(|(_, rest)| rest.trim().to_string()))
        .unwrap_or_else(|| "unknown".into());
    let mem_kb: u64 = meminfo
        .lines()
        .find(|l| l.starts_with("MemTotal:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let uname = util::uname_info();
    let os = uname
        .as_ref()
        .map(|u| format!("{} {} {}", u.sysname, u.release, u.machine))
        .unwrap_or_default();
    let kernel = uname
        .as_ref()
        .map(|u| u.release.clone())
        .unwrap_or_default();

    let metadata = json!({
        "created_at": util::utc_iso(),
        "minecraft": GAME_VERSION,
        "seed": DEFAULT_SEED_ENV,
        "side_chunks": opts.side,
        "target_chunks": opts.side * opts.side,
        "runs_per_server": opts.runs,
        "sample_interval_ms": opts.sample_ms,
        "world_storage": format!("{} (fresh directory for every trial)", std::env::temp_dir().display()),
        "steel_commit": util::capture(steel_repo, "git", &["rev-parse", "HEAD"]).unwrap_or_default(),
        "steel_worktree_dirty": !steel_status.is_empty(),
        "steel_worktree_diff_sha512": util::sha512_hex(&steel_diff),
        "steel_binary_sha512": util::file_sha512(&steel_repo.join("target/release/steel")).unwrap_or_default(),
        "steel_binary_modified_at": util::mtime_iso(&steel_repo.join("target/release/steel")),
        "steel_pregen_window_size": std::env::var("PREGEN_WINDOW_SIZE").unwrap_or_else(|_| "Steel default".into()),
        "steel_provider_workers": opts.workers,
        "fabric_loader": LOADER_VERSION,
        "chunky": CHUNKY_VERSION,
        "fabric_api": FABRIC_API_VERSION,
        "java_min_heap": opts.java_min_heap,
        "java_max_heap": opts.java_max_heap,
        "machine": {
            "os": os,
            "kernel": kernel,
            "cpu": cpu_model,
            "logical_cpus": util::logical_cpus(),
            "memory_bytes": mem_kb * 1024,
            "java": util::capture(&PathBuf::from("."), "java", &["-version"]).unwrap_or_default(),
        },
    });
    Ok(metadata)
}

fn write_outputs(
    output: &Path,
    metadata: &Value,
    trials: &[Value],
    samples: &[Value],
) -> Result<(), String> {
    fs::write(
        output.join("results.json"),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&json!({
                "metadata": metadata,
                "trials": trials,
                "samples": samples,
            }))
            .map_err(|e| e.to_string())?
        ),
    )
    .map_err(|e| e.to_string())
}

fn print_summary(output: &Path) -> Result<(), String> {
    let results: Value = serde_json::from_str(
        &fs::read_to_string(output.join("results.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let metadata = &results["metadata"];
    let trials = results["trials"].as_array().cloned().unwrap_or_default();

    let names = [
        ("steel", "Steel"),
        ("fabric", "Fabric (vanilla baseline)"),
        ("steel-provider", "steel-provider (native)"),
        ("minestom", "Minestom interop"),
    ];
    let mean = |xs: &[f64]| xs.iter().sum::<f64>() / xs.len() as f64;
    let median = |mut xs: Vec<f64>| {
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        xs[xs.len() / 2]
    };

    println!(
        "# Benchmark results — Minecraft {}, seed {}",
        metadata["minecraft"].as_str().unwrap_or("?"),
        metadata["seed"].as_str().unwrap_or("?")
    );
    println!(
        "{}×{} = {} chunks · {} runs per server",
        metadata["side_chunks"].as_i64().unwrap_or(0),
        metadata["side_chunks"].as_i64().unwrap_or(0),
        metadata["target_chunks"].as_i64().unwrap_or(0),
        metadata["runs_per_server"].as_i64().unwrap_or(0),
    );
    println!(
        "Machine: {} · {} logical CPUs · {} GiB RAM",
        metadata["machine"]["cpu"].as_str().unwrap_or("?"),
        metadata["machine"]["logical_cpus"].as_u64().unwrap_or(0),
        metadata["machine"]["memory_bytes"].as_u64().unwrap_or(0) / (1 << 30),
    );
    println!(
        "Steel: {}",
        metadata["steel_commit"].as_str().unwrap_or("?")
    );

    for (server, label) in names {
        let rows: Vec<&Value> = trials
            .iter()
            .filter(|t| t["server"].as_str() == Some(server))
            .collect();
        if rows.is_empty() {
            continue;
        }
        println!("\n## {label}");
        println!("| Run | Wall | Throughput | Avg CPU | Peak RSS |");
        println!("|-----|------|------------|---------|----------|");
        let mut walls = Vec::new();
        let mut cps = Vec::new();
        let mut cpus = Vec::new();
        let mut rss = Vec::new();
        for row in rows {
            let wall = row["wall_seconds"].as_f64().unwrap_or(0.0);
            let throughput = row["chunks_per_second"].as_f64().unwrap_or(0.0);
            let avg_cpu = row["average_cpu_cores"].as_f64().unwrap_or(0.0);
            let peak = row["peak_rss_bytes"].as_u64().unwrap_or(0);
            walls.push(wall);
            cps.push(throughput);
            cpus.push(avg_cpu);
            rss.push(peak);
            println!(
                "| {} | {:.2} s | {:.1} ch/s | {:.1} cores | {:.2} GiB |",
                row["run"].as_i64().unwrap_or(0),
                wall,
                throughput,
                avg_cpu,
                peak as f64 / (1u64 << 30) as f64,
            );
        }
        println!(
            "| **median** | **{:.2} s** | **{:.1} ch/s** | **{:.1} cores** | **{:.2} GiB** |",
            median(walls.clone()),
            median(cps.clone()),
            mean(&cpus),
            median(rss.iter().map(|&v| v as f64).collect::<Vec<f64>>()) / (1u64 << 30) as f64,
        );
    }
    Ok(())
}
