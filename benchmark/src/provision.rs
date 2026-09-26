//! Idempotent Fabric server provisioning and benchmark-mod downloads.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::util::{self, repo_root};

pub const GAME_VERSION: &str = "26.2";
pub const LOADER_VERSION: &str = "0.19.3";
pub const CHUNKY_VERSION: &str = "1.5.3";
pub const FABRIC_API_VERSION: &str = "0.155.2+26.2";

pub mod artifact {
    pub struct Spec {
        pub filename: &'static str,
        pub url: &'static str,
        pub sha512: &'static str,
    }
    pub const INSTALLER: Spec = Spec {
        filename: "fabric-installer-1.1.2.jar",
        url: "https://maven.fabricmc.net/net/fabricmc/fabric-installer/1.1.2/fabric-installer-1.1.2.jar",
        sha512: "3cbe7b69498c44d814fc95e71cd9ce9c12c45a9e1dcf01085cb3e9aaf6fe63134b701065167be1fa6e4c2df19dfa5400bc5b6b1b7fb5e99e334d24dcdca55c20",
    };
    pub const VANILLA_SERVER: Spec = Spec {
        filename: "server.jar",
        url: "https://piston-data.mojang.com/v1/objects/823e2250d24b3ddac457a60c92a6a941943fcd6a/server.jar",
        sha512: "a7d6df49426295102c6ae8fc1453af64d463e4dd8523c870466dd0410d12a998aec464fdb7bbf99889c80fc72aaac26a55bc675d2e32f49ede0e1839ecaf4592",
    };
    pub const CHUNKY: Spec = Spec {
        filename: "Chunky-Fabric-1.5.3.jar",
        url: "https://cdn.modrinth.com/data/fALzjamp/versions/4Eotm6ov/Chunky-Fabric-1.5.3.jar",
        sha512: "b83bfe7b218d0aa6232af977ae741dc1f82b10e50cd12bb759f65cf416b8b62beccb543e587ef0b9670abe03815660f8e091bc6823624d65cf07300571573516",
    };
    pub const FABRIC_API: Spec = Spec {
        filename: "fabric-api-0.155.2+26.2.jar",
        url: "https://cdn.modrinth.com/data/P7dR8mSH/versions/lVXlbH4w/fabric-api-0.155.2%2B26.2.jar",
        sha512: "cc56984378a27c5bcd56374d6ffbb27a45c6bf3355add2ac6be9817ccac5854362249bf9d0147eb271a70fda2716129204e240d53c9aa876a2a7861f4c7f880f",
    };
}

/// Provision a Fabric server directory if it doesn't already exist.
pub fn provision(fdir: &Path) -> Result<(), String> {
    let mut cache = repo_root().join("benchmark");
    cache.push(".fabric-cache");
    fs::create_dir_all(&cache).map_err(|e| e.to_string())?;
    fs::create_dir_all(fdir).map_err(|e| e.to_string())?;

    let installer = cache.join(artifact::INSTALLER.filename);
    if !installer.exists() {
        println!("==> Downloading {}", artifact::INSTALLER.filename);
        util::download(
            artifact::INSTALLER.url,
            &installer,
            artifact::INSTALLER.sha512,
        )?;
    }

    if !fdir.join("fabric-server-launch.jar").exists() {
        println!("==> Installing Fabric loader {LOADER_VERSION} for Minecraft {GAME_VERSION}");
        util::run_inherit(
            fdir,
            "java",
            &[
                "-jar",
                installer.to_str().unwrap(),
                "server",
                "-mcversion",
                GAME_VERSION,
                "-loader",
                LOADER_VERSION,
                "-dir",
                fdir.to_str().unwrap(),
            ],
        )?;
    }

    if !fdir.join("server.jar").exists() {
        println!("==> Downloading vanilla {GAME_VERSION} server.jar");
        util::download(
            artifact::VANILLA_SERVER.url,
            &fdir.join(artifact::VANILLA_SERVER.filename),
            artifact::VANILLA_SERVER.sha512,
        )?;
    }

    let launcher_props = fdir.join("fabric-server-launcher.properties");
    let has_server_jar = launcher_props.is_file()
        && fs::read_to_string(&launcher_props)
            .map(|s| s.contains("serverJar="))
            .unwrap_or(false);
    if !has_server_jar {
        fs::write(&launcher_props, "serverJar=server.jar\n").map_err(|e| e.to_string())?;
    }
    fs::write(fdir.join("eula.txt"), "eula=true\n").map_err(|e| e.to_string())?;

    if !fdir.join("server.properties").exists() {
        fs::write(
            fdir.join("server.properties"),
            "online-mode=false\nlevel-name=world\nlevel-seed=8500081009970950196\nserver-port=25565\nmotd=SteelWorldGen benchmark Fabric baseline\n",
        )
        .map_err(|e| e.to_string())?;
    }

    // One boot caches the game jar under versions/<game> so benchmark trials
    // run offline. Skipped once the cache exists.
    let game_jar = fdir
        .join("versions")
        .join(GAME_VERSION)
        .join(format!("server-{GAME_VERSION}.jar"));
    if !game_jar.exists() {
        println!("==> First boot (caches the vanilla game jar; may take a couple of minutes)");
        first_boot(fdir)?;
    }
    Ok(())
}

fn first_boot(dir: &Path) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let log_path = dir.join(".first-boot.log");
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&log_path)
        .map_err(|e| e.to_string())?;
    let mut command = Command::new("java");
    command
        .args([
            "-Xms512M",
            "-Xmx2G",
            "-jar",
            "fabric-server-launch.jar",
            "nogui",
        ])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(file));
    // Same reasoning as harness.rs run_trial: don't let the JVM touch our
    // controlling terminal.
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
        .map_err(|e| format!("failed to start Fabric first boot: {e}"))?;

    let pid = child.id();
    let deadline = std::time::Instant::now() + Duration::from_secs(240);
    let mut done = false;
    while std::time::Instant::now() < deadline {
        if let Ok(log) = fs::read_to_string(&log_path) {
            if log.contains("Done (") {
                done = true;
                break;
            }
        }
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!(
                "Fabric first boot exited early with {status}; see {}",
                log_path.display()
            ));
        }
        thread::sleep(Duration::from_secs(1));
    }
    if !done {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
        return Err(format!(
            "Fabric first boot did not finish; see {}",
            log_path.display()
        ));
    }
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    // Give it up to 10s to stop cleanly, then force it.
    for _ in 0..20 {
        if let Some(_) = child.try_wait().map_err(|e| e.to_string())? {
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
    let _ = fs::remove_file(&log_path);
    println!(
        "==> Game jar cached at {}",
        dir.join("versions")
            .join(GAME_VERSION)
            .join(format!("server-{GAME_VERSION}.jar"))
            .display()
    );
    Ok(())
}

/// Download the two benchmark mods into a cache dir (no-op on cache hits) and
/// return their local paths, used by every Fabric trial.
pub fn benchmark_mods(cache: &Path) -> Result<[PathBuf; 2], String> {
    fs::create_dir_all(cache).map_err(|e| e.to_string())?;
    let mut paths: Vec<PathBuf> = Vec::with_capacity(2);
    for spec in [&artifact::CHUNKY, &artifact::FABRIC_API] {
        let dest = cache.join(spec.filename);
        if !dest.exists() {
            println!("Downloading {}...", spec.filename);
            util::download(spec.url, &dest, spec.sha512)?;
        }
        paths.push(dest);
    }
    Ok([paths[0].clone(), paths[1].clone()])
}
