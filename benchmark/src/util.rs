//! Small platform helpers for the benchmark harness: process spawning,
//! checksums, downloads, and aggregated /proc sampling.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha512};

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .to_path_buf()
}

pub fn run_inherit(dir: &Path, program: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .current_dir(dir)
        .status()
        .map_err(|e| format!("failed to run {program}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with {status}"))
    }
}

pub fn capture(dir: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("failed to run {program}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{program} exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Capture raw stdout bytes even when the command succeeds (used for the
/// Steel worktree diff, which must be hashed byte-for-byte).
pub fn capture_binary(dir: &Path, program: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("failed to run {program}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{program} exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha512_hex(data: &[u8]) -> String {
    let mut h = Sha512::new();
    h.update(data);
    hex(&h.finalize())
}

pub fn file_sha512(path: &Path) -> Result<String, String> {
    let data = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(sha512_hex(&data))
}

/// Download a pinned artifact with curl and verify its SHA-512 before the file
/// is kept (curl is a baseline Linux tool; we already rely on java/git).
pub fn download(url: &str, dest: &Path, expected: &str) -> Result<(), String> {
    let tmp = dest.with_extension("tmp");
    for attempt in 1..=3 {
        let status = Command::new("curl")
            .args(["-fsSL", url, "-o"])
            .arg(&tmp)
            .status()
            .map_err(|e| format!("cannot invoke curl: {e}"))?;
        if !status.success() {
            if attempt == 3 {
                return Err(format!("download failed for {url} (curl exited {status})"));
            }
            continue;
        }
        let data = fs::read(&tmp).map_err(|e| format!("cannot read download: {e}"))?;
        let actual = sha512_hex(&data);
        if actual == expected {
            fs::rename(&tmp, dest).map_err(|e| format!("rename failed: {e}"))?;
            return Ok(());
        }
        if attempt == 3 {
            let _ = fs::remove_file(&tmp);
            return Err(format!("checksum mismatch for {url}: got {actual}"));
        }
    }
    Err("download failed".into())
}

/// Kernel tick rate (CLK_TCK), used to convert /proc/<pid>/stat CPU fields.
pub fn clock_ticks() -> f64 {
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks > 0 { ticks as f64 } else { 100.0 }
}

fn format_utc_rfc3339(secs: i64) -> String {
    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::gmtime_r(&t, &mut tm) }.is_null() {
        return String::new();
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

pub fn utc_iso() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| format_utc_rfc3339(d.as_secs() as i64))
        .unwrap_or_default()
}

pub fn mtime_iso(path: &Path) -> String {
    use std::os::unix::fs::MetadataExt;
    match fs::metadata(path) {
        Ok(meta) => format_utc_rfc3339(meta.mtime()),
        Err(_) => String::new(),
    }
}

pub struct UnameInfo {
    pub sysname: String,
    pub release: String,
    pub machine: String,
}

fn cstr(bytes: &[libc::c_char]) -> String {
    let mut end = 0;
    while end < bytes.len() && bytes[end] != 0 {
        end += 1;
    }
    String::from_utf8_lossy(&bytes[..end].iter().map(|&c| c as u8).collect::<Vec<u8>>())
        .into_owned()
}

/// `uname(2)` without shelling out to the `uname` binary.
pub fn uname_info() -> Option<UnameInfo> {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut u) } != 0 {
        return None;
    }
    Some(UnameInfo {
        sysname: cstr(&u.sysname),
        release: cstr(&u.release),
        machine: cstr(&u.machine),
    })
}

/// Number of logical CPUs visible to the process (like `nproc`), honoring CPU
/// affinity.
pub fn logical_cpus() -> u64 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u64)
        .unwrap_or(1)
}

/// utime+stime (fields 14,15) of a process in clock ticks.
fn stat_times(pid: u32) -> Option<(u64, u64)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    if fields.len() < 13 {
        return None;
    }
    Some((fields[11].parse().ok()?, fields[12].parse().ok()?))
}

fn vm_rss_bytes(pid: u32) -> Option<u64> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

fn children_of(pid: u32) -> Vec<u32> {
    // A process may be forked from any thread, and Linux records each of
    // those children under that thread's /proc/<tgid>/task/<tid>/children —
    // not the tgid's own. Scan every thread so nothing is missed.
    let mut out = Vec::new();
    let task_dir = format!("/proc/{pid}/task");
    let Ok(entries) = fs::read_dir(&task_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        if let Ok(data) = fs::read_to_string(entry.path().join("children")) {
            for c in data.split_whitespace() {
                if let Ok(c) = c.parse() {
                    out.push(c);
                }
            }
        }
    }
    out
}

/// The tracked process and all of its live descendants. The Minestom demo runs
/// the steel-provider as a child process, so its CPU/RSS must be included to
/// get a fair reading of the interop path.
pub fn collect_processes(pid: u32) -> Vec<u32> {
    let mut out = vec![pid];
    let mut stack = children_of(pid);
    while let Some(p) = stack.pop() {
        if out.contains(&p) {
            continue;
        }
        out.push(p);
        for kid in children_of(p) {
            stack.push(kid);
        }
    }
    out
}

pub struct Sample {
    pub cpu_seconds: f64,
    pub rss_bytes: u64,
}

/// Aggregated CPU time and RSS across every process in the list (children of
/// the spawned child, plus the child itself). Processes that vanished mid-read
/// are skipped.
pub fn sample_processes(pids: &[u32], ticks: f64) -> Option<Sample> {
    let mut cpu = 0.0f64;
    let mut rss = 0u64;
    let mut any = false;
    for &pid in pids {
        if let Some((u, s)) = stat_times(pid) {
            cpu += (u + s) as f64 / ticks;
            any = true;
        }
        if let Some(r) = vm_rss_bytes(pid) {
            rss += r;
            any = true;
        }
    }
    if any {
        Some(Sample {
            cpu_seconds: cpu,
            rss_bytes: rss,
        })
    } else {
        None
    }
}
