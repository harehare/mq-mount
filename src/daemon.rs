//! Background mounts (`-d`/`--background`) and `--stop <mountpoint>`: a
//! small per-mount PID file under a temp state dir lets `--stop` find and
//! signal the right process, and lets a mount clean up after itself via
//! [`PidFileGuard`]'s `Drop` regardless of how it exits (Ctrl-C, `--stop`'s
//! signal, or an externally-triggered unmount).

use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use rustc_hash::FxHasher;

fn state_dir() -> PathBuf {
    std::env::temp_dir().join("mq-mount")
}

fn state_file(mountpoint: &Path, ext: &str) -> PathBuf {
    let mut hasher = FxHasher::default();
    mountpoint.hash(&mut hasher);
    state_dir().join(format!("{:016x}.{ext}", hasher.finish()))
}

fn canonical(mountpoint: &Path) -> PathBuf {
    mountpoint.canonicalize().unwrap_or_else(|_| mountpoint.to_path_buf())
}

pub struct PidFileGuard {
    path: PathBuf,
}

impl PidFileGuard {
    /// Writes `<pid>\n<mountpoint>\n` followed by one mounted source path
    /// per line; only the first two lines are required to read it back.
    pub fn create(mountpoint: &Path, sources: &[PathBuf]) -> std::io::Result<Self> {
        fs::create_dir_all(state_dir())?;
        let path = state_file(mountpoint, "pid");
        let mut text = format!("{}\n{}\n", std::process::id(), mountpoint.display());
        for source in sources {
            text.push_str(&format!("{}\n", source.display()));
        }
        fs::write(&path, text)?;
        Ok(Self { path })
    }
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn spawn_background(mountpoint: &Path) -> miette::Result<()> {
    fs::create_dir_all(state_dir()).map_err(|e| miette::miette!("failed to create state dir: {e}"))?;
    let exe = std::env::current_exe().map_err(|e| miette::miette!("failed to resolve current executable: {e}"))?;
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "-d" && a != "--background")
        .collect();

    let log_path = state_file(mountpoint, "log");
    let stdout = fs::File::create(&log_path).map_err(|e| miette::miette!("failed to create log file: {e}"))?;
    let stderr = stdout
        .try_clone()
        .map_err(|e| miette::miette!("failed to prepare log file: {e}"))?;

    let mut cmd = Command::new(exe);
    cmd.args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    detach(&mut cmd);

    let child = cmd
        .spawn()
        .map_err(|e| miette::miette!("failed to start background process: {e}"))?;
    println!(
        "mounted in background (pid {}); logs at {}",
        child.id(),
        log_path.display()
    );
    println!("stop with: mq-mount --stop {}", mountpoint.display());
    Ok(())
}

#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    cmd.creation_flags(DETACHED_PROCESS);
}

fn read_pid(path: &Path, mountpoint: &Path) -> miette::Result<i32> {
    let text =
        fs::read_to_string(path).map_err(|_| miette::miette!("no running mount found at {}", mountpoint.display()))?;
    text.lines()
        .next()
        .and_then(|l| l.parse().ok())
        .ok_or_else(|| miette::miette!("corrupt pid file: {}", path.display()))
}

/// A running mount, as recorded in its pid file.
#[derive(Debug, PartialEq, Eq)]
struct MountInfo {
    pid: i32,
    mountpoint: PathBuf,
    /// Empty for a pid file written before sources were recorded.
    sources: Vec<PathBuf>,
}

/// Like [`read_pid`], but for `--list`: a corrupt or unreadable pid file is
/// skipped rather than aborting the whole listing.
fn read_mount_info(path: &Path) -> Option<MountInfo> {
    let text = fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let pid = lines.next()?.parse().ok()?;
    let mountpoint = PathBuf::from(lines.next()?);
    let sources = lines.filter(|l| !l.is_empty()).map(PathBuf::from).collect();
    Some(MountInfo {
        pid,
        mountpoint,
        sources,
    })
}

#[cfg(unix)]
fn is_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(windows)]
fn is_alive(pid: i32) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid as u32) {
            Ok(handle) => {
                let _ = CloseHandle(handle);
                true
            }
            Err(_) => false,
        }
    }
}

/// Lists every currently-running background (or foreground) mount, found by
/// scanning the state dir's `*.pid` files. Self-healing: a pid file whose
/// process is no longer alive (e.g. it was `kill -9`'d, skipping
/// `PidFileGuard`'s `Drop`) is removed instead of listed.
pub fn list(json: bool) -> miette::Result<()> {
    let mut mounts = Vec::new();
    if let Ok(entries) = fs::read_dir(state_dir()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("pid") {
                continue;
            }
            let Some(info) = read_mount_info(&path) else {
                continue;
            };
            if is_alive(info.pid) {
                mounts.push(info);
            } else {
                let _ = fs::remove_file(&path);
                let _ = fs::remove_file(path.with_extension("log"));
            }
        }
    }
    mounts.sort_by(|a, b| a.mountpoint.cmp(&b.mountpoint));

    if json {
        println!("{}", mounts_json(&mounts));
        return Ok(());
    }
    if mounts.is_empty() {
        println!("no running mounts");
        return Ok(());
    }
    println!("{:<10} {:<40} SOURCES", "PID", "MOUNTPOINT");
    for info in mounts {
        let sources: Vec<String> = info.sources.iter().map(|s| s.display().to_string()).collect();
        println!(
            "{:<10} {:<40} {}",
            info.pid,
            info.mountpoint.display(),
            sources.join(" ")
        );
    }
    Ok(())
}

/// `--list --json`: an array of `{pid, mountpoint, sources, log}` objects,
/// `log` being the background log file (`null` for a foreground mount).
fn mounts_json(mounts: &[MountInfo]) -> String {
    let items: Vec<String> = mounts
        .iter()
        .map(|m| {
            let sources: Vec<String> = m.sources.iter().map(|s| json_path(s)).collect();
            let log = state_file(&m.mountpoint, "log");
            let log = if log.exists() {
                json_path(&log)
            } else {
                "null".to_string()
            };
            format!(
                "{{\"pid\":{},\"mountpoint\":{},\"sources\":[{}],\"log\":{}}}",
                m.pid,
                json_path(&m.mountpoint),
                sources.join(","),
                log
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

fn json_path(path: &Path) -> String {
    json_string(&path.to_string_lossy())
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(unix)]
pub fn stop(mountpoint: &Path) -> miette::Result<()> {
    let mountpoint = canonical(mountpoint);
    let path = state_file(&mountpoint, "pid");
    let pid = read_pid(&path, &mountpoint)?;

    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return Err(miette::miette!(
            "no running mount found at {} (pid {pid} not alive)",
            mountpoint.display()
        ));
    }

    let start = std::time::Instant::now();
    while path.exists() {
        if start.elapsed() > Duration::from_secs(10) {
            miette::bail!("timed out waiting for the mount at {} to stop", mountpoint.display());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    tracing::info!("stopped mount at {}", mountpoint.display());
    Ok(())
}

#[cfg(windows)]
pub fn stop(mountpoint: &Path) -> miette::Result<()> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    let mountpoint = canonical(mountpoint);
    let path = state_file(&mountpoint, "pid");
    let pid = read_pid(&path, &mountpoint)?;

    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, false, pid as u32)
            .map_err(|e| miette::miette!("failed to open process {pid}: {e}"))?;
        let result = TerminateProcess(handle, 1);
        let _ = CloseHandle(handle);
        result.map_err(|e| miette::miette!("failed to terminate process {pid}: {e}"))?;
    }
    let _ = fs::remove_file(&path);
    tracing::info!("stopped mount at {}", mountpoint.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_file_guard_writes_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mnt");
        let guard = PidFileGuard::create(&mountpoint, &[PathBuf::from("/docs/a.md")]).unwrap();

        let path = state_file(&mountpoint, "pid");
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.starts_with(&std::process::id().to_string()));

        drop(guard);
        assert!(!path.exists());
    }

    #[test]
    fn stop_reports_when_no_mount_is_running() {
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("never-mounted");
        assert!(stop(&mountpoint).is_err());
    }

    #[test]
    fn read_mount_info_parses_the_pid_file_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.pid");
        fs::write(&path, "12345\n/tmp/mnt\n/docs/a.md\n/docs/b.md\n").unwrap();
        assert_eq!(
            read_mount_info(&path),
            Some(MountInfo {
                pid: 12345,
                mountpoint: PathBuf::from("/tmp/mnt"),
                sources: vec![PathBuf::from("/docs/a.md"), PathBuf::from("/docs/b.md")],
            })
        );
    }

    #[test]
    fn read_mount_info_accepts_a_pid_file_without_sources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.pid");
        fs::write(&path, "12345\n/tmp/mnt\n").unwrap();
        let info = read_mount_info(&path).unwrap();
        assert_eq!(info.pid, 12345);
        assert!(info.sources.is_empty());
    }

    #[test]
    fn read_mount_info_is_none_for_a_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.pid");
        fs::write(&path, "not-a-pid\n").unwrap();
        assert_eq!(read_mount_info(&path), None);
    }

    #[test]
    fn mounts_json_escapes_paths_and_lists_sources() {
        let json = mounts_json(&[MountInfo {
            pid: 7,
            mountpoint: PathBuf::from("/tmp/never-mounted \"q\"\\x"),
            sources: vec![PathBuf::from("/a.md")],
        }]);
        assert_eq!(
            json,
            r#"[{"pid":7,"mountpoint":"/tmp/never-mounted \"q\"\\x","sources":["/a.md"],"log":null}]"#
        );
        assert_eq!(mounts_json(&[]), "[]");
    }

    #[test]
    fn is_alive_is_true_for_the_current_process_and_false_for_a_bogus_pid() {
        assert!(is_alive(std::process::id() as i32));
        assert!(!is_alive(i32::MAX));
    }
}
