//! Trial runner: prepares each server's working directory, spawns it with the
//! right environment, samples process CPU/RSS (aggregated across the spawned
//! process and its children, so Minestom's steel-provider subprocess counts),
//! and detects the benchmark interval via the host's own markers.

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::util::{self, Sample};

pub const SEED: &str = "8500081009970950196";
pub const START_MARKER: &str = "Preparing spawn area:";

#[derive(Clone, Copy, PartialEq)]
pub enum Profile {
    All,
    Steel,
    Fabric,
    Minestom,
    SteelProvider,
    SteelMinestom,
}

impl Profile {
    pub fn parse(name: &str) -> Result<Profile, String> {
        match name {
            "all" => Ok(Profile::All),
            "steel" => Ok(Profile::Steel),
            "fabric" => Ok(Profile::Fabric),
            "minestom" => Ok(Profile::Minestom),
            "steel-provider" => Ok(Profile::SteelProvider),
            "steel-minestom" => Ok(Profile::SteelMinestom),
            _ => Err(format!("invalid profile: {name}")),
        }
    }
    pub fn uses_fabric(self) -> bool {
        matches!(self, Profile::All | Profile::Fabric)
    }
    pub fn uses_demo(self) -> bool {
        matches!(
            self,
            Profile::All | Profile::Minestom | Profile::SteelMinestom
        )
    }
    pub fn order(self) -> Vec<&'static str> {
        match self {
            Profile::All => vec!["steel", "fabric", "steel-provider", "minestom"],
            Profile::Steel => vec!["steel"],
            Profile::Fabric => vec!["fabric"],
            Profile::Minestom => vec!["minestom"],
            Profile::SteelProvider => vec!["steel-provider"],
            Profile::SteelMinestom => vec!["steel", "minestom"],
        }
    }
}

pub struct Options {
    pub steel: PathBuf,
    pub fabric: PathBuf,
    pub minestom: PathBuf,
    pub steel_provider: PathBuf,
    pub output: PathBuf,
    pub runs: u32,
    pub side: i32,
    pub workers: u32,
    pub sample_ms: u64,
    pub java_min_heap: String,
    pub java_max_heap: String,
    pub cache: PathBuf,
}

pub struct Prepared {
    work: PathBuf,
    command: Vec<String>,
    env: Vec<(String, String)>,
}

pub struct Trial {
    pub value: Value,
    pub samples: Vec<Value>,
}

fn temp_dir(prefix: &str) -> Result<PathBuf, String> {
    let base = std::env::temp_dir();
    for i in 0..10_000 {
        let candidate = base.join(format!("{prefix}-{}-{i}", std::process::id()));
        if fs::create_dir(&candidate).is_ok() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "could not create temp dir under {}",
        base.display()
    ))
}

fn replace_toml_value(text: &str, key: &str, value: &str) -> String {
    text.lines()
        .map(|line| {
            if let Some(eq) = line.find('=') {
                if line[..eq].trim() == key {
                    format!("{}{}", &line[..=eq], value)
                } else {
                    line.to_string()
                }
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn replace_property(text: &str, key: &str, value: &str) -> String {
    let mut out = String::new();
    let mut replaced = false;
    for line in text.lines() {
        if let Some(eq) = line.find('=') {
            if line[..eq].trim() == key {
                out.push_str(&format!("{key}={value}\n"));
                replaced = true;
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    if !replaced {
        out.push_str(&format!("{key}={value}\n"));
    }
    out
}

fn prepare_steel(repo: &Path, port: u32) -> Result<Prepared, String> {
    let work = temp_dir("steel-benchmark-steel")?;
    fs::create_dir_all(work.join("config")).map_err(|e| e.to_string())?;
    for (name, target) in [
        ("config.toml", "config/config.toml"),
        ("worlds.toml", "config/worlds.toml"),
    ] {
        fs::copy(repo.join("package-content").join(name), work.join(target))
            .map_err(|e| e.to_string())?;
    }
    for name in ["groups.toml", "favicon.png"] {
        if let Ok(data) = fs::read(repo.join("package-content").join(name)) {
            let _ = fs::write(work.join("config").join(name), data);
        }
    }

    let config_path = work.join("config/config.toml");
    let config = fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
    let config = replace_toml_value(&config, "server_port", &port.to_string());
    let config = replace_toml_value(&config, "online_mode", "false");
    fs::write(&config_path, config).map_err(|e| e.to_string())?;

    let worlds_path = work.join("config/worlds.toml");
    let worlds = fs::read_to_string(&worlds_path).map_err(|e| e.to_string())?;
    fs::write(
        &worlds_path,
        replace_toml_value(&worlds, "seed", &format!("\"{SEED}\"")),
    )
    .map_err(|e| e.to_string())?;

    Ok(Prepared {
        work,
        command: vec![
            repo.join("target/release/steel")
                .to_string_lossy()
                .into_owned(),
        ],
        env: vec![("PREGEN_SIZE".into(), "{side}".into())],
    })
}

fn copy_recursive(from: &Path, to: &Path) -> Result<(), String> {
    for entry in fs::read_dir(from).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            fs::create_dir_all(&dst).map_err(|e| e.to_string())?;
            copy_recursive(&src, &dst)?;
        } else {
            fs::copy(&src, &dst).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn prepare_fabric(opts: &Options, mods: &[PathBuf; 2], port: u32) -> Result<Prepared, String> {
    let entries: Vec<String> = fs::read_dir(&opts.fabric)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("fabric-server-") && n.ends_with(".jar"))
        .collect();
    if entries.len() != 1 {
        return Err(format!(
            "expected exactly one Fabric launcher in {} (found {})",
            opts.fabric.display(),
            entries.len()
        ));
    }
    let launcher = &entries[0];
    let work = temp_dir("steel-benchmark-fabric")?;

    fs::copy(opts.fabric.join(launcher), work.join(launcher)).map_err(|e| e.to_string())?;
    // The Fabric launcher expects the vanilla game jar at server.jar so trials
    // run offline.
    fs::copy(opts.fabric.join("server.jar"), work.join("server.jar")).map_err(|e| e.to_string())?;
    for dir in ["libraries", "versions"] {
        symlink(opts.fabric.join(dir), work.join(dir)).map_err(|e| e.to_string())?;
    }
    if opts.fabric.join(".fabric").exists() {
        fs::create_dir_all(work.join(".fabric")).map_err(|e| e.to_string())?;
        copy_recursive(&opts.fabric.join(".fabric"), &work.join(".fabric"))?;
    }
    fs::create_dir_all(work.join("mods")).map_err(|e| e.to_string())?;
    for mod_path in mods {
        let name = mod_path
            .file_name()
            .ok_or("mod has no file name")?
            .to_string_lossy()
            .into_owned();
        fs::copy(mod_path, work.join("mods").join(name)).map_err(|e| e.to_string())?;
    }
    fs::write(work.join("eula.txt"), "eula=true\n").map_err(|e| e.to_string())?;

    let mut properties =
        fs::read_to_string(opts.fabric.join("server.properties")).map_err(|e| e.to_string())?;
    for (key, value) in [
        ("level-seed", SEED),
        ("online-mode", "false"),
        ("server-port", &port.to_string()),
        ("pause-when-empty-seconds", "-1"),
        ("sync-chunk-writes", "true"),
        ("generate-structures", "true"),
        ("level-name", "world"),
    ] {
        properties = replace_property(&properties, key, value);
    }
    fs::write(work.join("server.properties"), properties).map_err(|e| e.to_string())?;

    Ok(Prepared {
        command: vec![
            "java".into(),
            format!("-Xms{}", opts.java_min_heap),
            format!("-Xmx{}", opts.java_max_heap),
            "-XX:+UseG1GC".into(),
            "-jar".into(),
            launcher.clone(),
            "nogui".into(),
        ],
        env: vec![],
        work,
    })
}

fn prepare_minestom(opts: &Options) -> Result<Prepared, String> {
    let work = temp_dir("steel-benchmark-minestom")?;
    fs::copy(&opts.minestom, work.join("demo-all.jar")).map_err(|e| e.to_string())?;
    Ok(Prepared {
        work,
        command: vec![
            "java".into(),
            format!("-Xms{}", opts.java_min_heap),
            format!("-Xmx{}", opts.java_max_heap),
            "-XX:+UseG1GC".into(),
            "-jar".into(),
            "demo-all.jar".into(),
        ],
        env: vec![
            ("PREGEN_SIZE".into(), "{side}".into()),
            ("BENCH_SEED".into(), SEED.into()),
        ],
    })
}

fn prepare_steel_provider(opts: &Options) -> Result<Prepared, String> {
    let work = temp_dir("steel-benchmark-provider")?;
    Ok(Prepared {
        work,
        command: vec![opts.steel_provider.to_string_lossy().into_owned()],
        env: vec![
            ("PREGEN_SIZE".into(), "{side}".into()),
            ("BENCH_SEED".into(), SEED.into()),
            ("WORKERS".into(), opts.workers.to_string()),
        ],
    })
}

fn clean_ansi(line: &str) -> String {
    let mut out = String::new();
    let mut esc = false;
    for c in line.chars() {
        if esc {
            if c.is_ascii_alphabetic() {
                esc = false;
            }
            continue;
        }
        if c == '\x1b' {
            esc = true;
        } else if c != '\r' {
            out.push(c);
        }
    }
    out
}

fn parse_done(line: &str) -> Option<(u64, f64, f64)> {
    let rest = line.find("Spawn area prepared: ")?;
    let rest = &line[rest + "Spawn area prepared: ".len()..];
    let chunks_end = rest.find(" chunks in ")?;
    let chunks: u64 = rest[..chunks_end].trim().parse().ok()?;
    let tail = &rest[chunks_end + " chunks in ".len()..];
    let s_pos = tail.find('s')?;
    if !tail[s_pos..].starts_with("s (") {
        return None;
    }
    let secs: f64 = tail[..s_pos].parse().ok()?;
    let after = tail[s_pos + 1..].trim_start();
    let cps_start = after.find('(')? + 1;
    let cps_end = after[cps_start..].find(' ')? + cps_start;
    let cps: f64 = after[cps_start..cps_end].trim().parse().ok()?;
    Some((chunks, secs, cps))
}

fn parse_progress(line: &str) -> Option<(u64, f64)> {
    let start = line.find("Processed: ")? + "Processed: ".len();
    let chunks_end = line[start..].find(" chunks")? + start;
    let chunks: u64 = line[start..chunks_end].trim().parse().ok()?;
    let rpos = line.find("Rate: ")? + "Rate: ".len();
    let cps_end = line[rpos..].find(" cps")? + rpos;
    let cps: f64 = line[rpos..cps_end].trim().parse().ok()?;
    Some((chunks, cps))
}

fn aggregate(pid: u32, ticks: f64) -> Option<Sample> {
    let pids = util::collect_processes(pid);
    util::sample_processes(&pids, ticks)
}

/// Kill a spawned server and everything it has forked.
fn kill_tree(pid: u32) {
    // Descendants first, then the parent, so no child gets reparented and
    // orphaned mid-kill.
    let pids = util::collect_processes(pid);
    for &p in pids.iter().rev() {
        if p != pid {
            unsafe {
                libc::kill(p as i32, libc::SIGKILL);
            }
        }
    }
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

pub fn trial_order(profile: Profile, run: u32) -> Vec<&'static str> {
    let base = profile.order();
    let offset = (run - 1) as usize % base.len();
    base[offset..]
        .iter()
        .chain(base[..offset].iter())
        .copied()
        .collect()
}

pub fn prepare(
    server: &str,
    opts: &Options,
    mods: &[PathBuf; 2],
    port: u32,
) -> Result<Prepared, String> {
    match server {
        "steel" => prepare_steel(&opts.steel, port),
        "fabric" => prepare_fabric(opts, mods, port),
        "minestom" => prepare_minestom(opts),
        "steel-provider" => prepare_steel_provider(opts),
        _ => Err(format!("unknown server {server}")),
    }
}

pub fn run_trial(
    server: &str,
    run: u32,
    opts: &Options,
    prepared: &Prepared,
) -> Result<Trial, String> {
    let raw_dir = opts.output.join("raw");
    fs::create_dir_all(&raw_dir).map_err(|e| e.to_string())?;
    let raw_path = raw_dir.join(format!("{server}-{run}.log"));
    let mut raw = String::new();
    let mut samples: Vec<Value> = Vec::new();
    let is_fabric = server.starts_with("fabric");

    let mut env: Vec<(String, String)> = std::env::vars().collect();
    for (key, value) in &prepared.env {
        env.push((key.clone(), value.replace("{side}", &opts.side.to_string())));
    }

    println!("Starting {server} run {run} in {}", prepared.work.display());
    use std::os::unix::process::CommandExt;
    let mut command = Command::new(&prepared.command[0]);
    command
        .args(&prepared.command[1..])
        .envs(env)
        .current_dir(&prepared.work)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Terminate the child's controlling-terminal association so a tty-aware
    // server (Steel, the Fabric/Minestom JVMs) cannot tcsetattr() our terminal
    // into raw mode via /dev/tty — that clears OPOST, and the kernel would stop
    // translating our own println! newlines into CRLF, making them display as
    // mashed-together lines. setsid() makes the server a new session leader
    // with no controlling tty.
    //
    // SAFETY: pre_exec runs in the forked child before exec, so the call is
    // single-threaded there.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("failed to spawn {server}: {e}"))?;
    let pid = child.id();
    let mut stdin = child.stdin.take();

    let (tx, rx) = mpsc::channel::<String>();
    let stdout: Box<dyn io::Read + Send> = Box::new(child.stdout.take().expect("stdout"));
    let stderr: Box<dyn io::Read + Send> = Box::new(child.stderr.take().expect("stderr"));
    for stream in [stdout, stderr] {
        let tx = tx.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stream);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    drop(tx);

    let ticks = util::clock_ticks();
    let trial_start = Instant::now();
    let mono = |t: Instant| (t - trial_start).as_secs_f64();

    let mut benchmark_start: Option<Instant> = None;
    let mut benchmark_end: Option<Instant> = None;
    let mut start_cpu: Option<f64> = None;
    let mut end_cpu: Option<f64> = None;
    // Cumulative CPU at the last periodic sample. The Minestom benchmark closes
    // its steel-provider child right before printing the end marker, so the end
    // sample there no longer includes that process; the last periodic sample
    // (taken ≤ sample_ms earlier, while the provider was still running) is used
    // as the end edge so its CPU is not lost from the average.
    let mut last_cpu: Option<f64> = None;
    // Cumulative CPU only grows, so the end edge is the larger of the final
    // aggregate and the last periodic sample. Preferring the aggregate alone
    // would drop the Minestom provider child, which is already gone at the end.
    let end_cpu_of =
        |s: &Option<Sample>, last: Option<f64>| match (s.as_ref().map(|x| x.cpu_seconds), last) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    let mut peak_rss: u64 = 0;
    let mut reported_chunks: Option<u64> = None;
    let mut reported_seconds: Option<f64> = None;
    let mut reported_cps: Option<f64> = None;
    let mut chunky_started = false;
    let mut terminated = false;
    let mut last_sample = Instant::now();

    let mut push_sample = |phase: &str, s: Option<Sample>, bsec: Option<f64>| {
        if let Some(s) = s {
            if s.rss_bytes > peak_rss {
                peak_rss = s.rss_bytes;
            }
            samples.push(json!({
                "server": server,
                "run": run,
                "phase": phase,
                "monotonic_seconds": mono(Instant::now()),
                "benchmark_seconds": bsec.map_or(json!(""), |v| json!(v)),
                "cpu_seconds": s.cpu_seconds,
                "rss_bytes": s.rss_bytes,
            }));
        }
    };

    let radius = (((opts.side - 1) / 2) * 16) as i64;
    let sample_dur = Duration::from_millis(opts.sample_ms);

    loop {
        let line = match rx.recv_timeout(Duration::from_millis(2)) {
            Ok(l) => Some(l),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if let Some(line) = line {
            let clean = clean_ansi(&line);
            raw.push_str(&clean);
            raw.push('\n');
            println!("[{server} {run}] {clean}");
            if is_fabric {
                if !chunky_started && clean.contains("Done (") {
                    for cmd in [
                        "chunky quiet 1",
                        "chunky shape square",
                        "chunky center 0 0",
                        &format!("chunky radius {radius}"),
                        "chunky start",
                    ] {
                        if let Some(w) = stdin.as_mut() {
                            let _ = w.write_all(cmd.as_bytes());
                            let _ = w.write_all(b"\n");
                            let _ = w.flush();
                        }
                    }
                    chunky_started = true;
                } else if clean.contains("[Chunky] Task started") && benchmark_start.is_none() {
                    benchmark_start = Some(Instant::now());
                    let s = aggregate(pid, ticks);
                    start_cpu = s.as_ref().map(|x| x.cpu_seconds);
                    push_sample("benchmark", s, Some(0.0));
                } else if clean.contains("[Chunky] Task finished") && benchmark_end.is_none() {
                    let now = Instant::now();
                    benchmark_end = Some(now);
                    let s = aggregate(pid, ticks);
                    end_cpu = end_cpu_of(&s, last_cpu);
                    if let Some((c, cps)) = parse_progress(&clean) {
                        reported_chunks = Some(c);
                        reported_cps = Some(cps);
                    }
                    let wall = benchmark_start.map(|st| (now - st).as_secs_f64());
                    push_sample("benchmark", s, wall);
                    kill_tree(pid);
                    terminated = true;
                }
            } else {
                if benchmark_start.is_none() && clean.contains(START_MARKER) {
                    benchmark_start = Some(Instant::now());
                    let s = aggregate(pid, ticks);
                    start_cpu = s.as_ref().map(|x| x.cpu_seconds);
                    push_sample("benchmark", s, Some(0.0));
                }
                if let Some((chunks, secs, cps)) = parse_done(&clean) {
                    let now = Instant::now();
                    benchmark_end = Some(now);
                    let s = aggregate(pid, ticks);
                    end_cpu = end_cpu_of(&s, last_cpu);
                    reported_chunks = Some(chunks);
                    reported_seconds = Some(secs);
                    reported_cps = Some(cps);
                    let wall = benchmark_start.map(|st| (now - st).as_secs_f64());
                    push_sample("benchmark", s, wall);
                    kill_tree(pid);
                    terminated = true;
                }
            }
        }
        if terminated {
            break;
        }
        if benchmark_start.is_some()
            && benchmark_end.is_none()
            && last_sample.elapsed() >= sample_dur
        {
            let now = Instant::now();
            let wall = benchmark_start.map(|st| (now - st).as_secs_f64());
            let s = aggregate(pid, ticks);
            last_cpu = s.as_ref().map(|x| x.cpu_seconds);
            push_sample("benchmark", s, wall);
            last_sample = now;
        }
    }

    // Reap the child. We SIGKILL the tree at the benchmark marker, so the
    // process is already gone; wait() just reaps it. A genuine crash (own
    // exit, non-zero status, before we terminated it) is still an error.
    let status = child.wait().map_err(|e| e.to_string())?;
    let ok = status.success() || terminated;
    if !ok {
        let _ = fs::remove_dir_all(&prepared.work);
        return Err(format!("{server} run {run} exited with {status}"));
    }
    match (benchmark_start, benchmark_end, start_cpu, end_cpu) {
        (Some(st), Some(en), Some(sc), Some(ec)) => {
            let wall = (en - st).as_secs_f64();
            let cpu = ec - sc;
            let chunks = reported_chunks.unwrap_or((opts.side * opts.side) as u64);
            let _ = fs::write(&raw_path, &raw);
            let trial = Trial {
                value: json!({
                    "server": server,
                    "run": run,
                    "chunks": chunks,
                    "wall_seconds": (wall * 1000.0).round() / 1000.0,
                    "chunks_per_second": (chunks as f64 / wall * 10.0).round() / 10.0,
                    "reported_seconds": reported_seconds,
                    "reported_chunks_per_second": reported_cps,
                    "cpu_seconds": (cpu * 1000.0).round() / 1000.0,
                    "average_cpu_cores": (cpu / wall * 10.0).round() / 10.0,
                    "peak_rss_bytes": peak_rss,
                    "raw_log": format!("raw/{server}-{run}.log"),
                }),
                samples,
            };
            let _ = fs::remove_dir_all(&prepared.work);
            Ok(trial)
        }
        _ => {
            let _ = fs::remove_dir_all(&prepared.work);
            Err(format!(
                "Could not identify benchmark interval for {server} run {run}"
            ))
        }
    }
}
