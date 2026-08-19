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
    pub fn create(mountpoint: &Path) -> std::io::Result<Self> {
        fs::create_dir_all(state_dir())?;
        let path = state_file(mountpoint, "pid");
        fs::write(&path, format!("{}\n{}\n", std::process::id(), mountpoint.display()))?;
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
        let guard = PidFileGuard::create(&mountpoint).unwrap();

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
}
