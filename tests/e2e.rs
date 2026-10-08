//! End-to-end tests against a real mount: each test starts the built
//! `mq-mount` binary on a temp file, drives the mount with ordinary file
//! operations and shell tools (`grep`, `sed -i`, `mv`, `rm -r`), and checks
//! what lands in the source file.
//!
//! Mounting needs the OS's NFS client (and root on Linux), so these only run
//! when `MQ_MOUNT_E2E=1` is set and are skipped otherwise:
//!
//! ```sh
//! sudo -E env "PATH=$PATH" MQ_MOUNT_E2E=1 cargo test --test e2e -- --test-threads=1
//! ```
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_mq-mount");

const DOC: &str = "\
# Guide

intro

## Install

* step one
* step two

|a|b|
|-|-|
|1|2|

## Usage

old usage

## Remove Me

gone soon
";

fn enabled() -> bool {
    if std::env::var_os("MQ_MOUNT_E2E").is_some_and(|v| v == "1") {
        return true;
    }
    eprintln!("skipping: set MQ_MOUNT_E2E=1 to run end-to-end mount tests");
    false
}

fn is_mounted(mountpoint: &Path) -> bool {
    let (Ok(here), Ok(up)) = (fs::metadata(mountpoint), fs::metadata(mountpoint.parent().unwrap())) else {
        return false;
    };
    here.dev() != up.dev()
}

/// Polls `cond` until it holds, panicking with `what` after a timeout.
fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn run(cmd: &mut Command) -> std::process::Output {
    let output = cmd.output().expect("failed to spawn");
    assert!(
        output.status.success(),
        "{cmd:?} failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// A running foreground mount of `doc.md` at `<tmp>/mnt`. Dropping it stops
/// the process (SIGTERM: flush, then unmount) and, if anything went wrong,
/// force-unmounts so a failed test never leaves a stale mount behind.
struct Mount {
    dir: tempfile::TempDir,
    child: Option<Child>,
}

impl Mount {
    fn start(extra_args: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("doc.md"), DOC).unwrap();
        let mut mount = Self { dir, child: None };
        let child = Command::new(BIN)
            .arg(mount.source())
            .arg(mount.mnt())
            .arg("--no-config")
            .args(extra_args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to start mq-mount");
        mount.child = Some(child);
        mount.wait_mounted();
        mount
    }

    fn source(&self) -> PathBuf {
        self.dir.path().join("doc.md")
    }

    fn mnt(&self) -> PathBuf {
        self.dir.path().join("mnt")
    }

    /// A path inside the mounted file's own directory.
    fn path(&self, rel: &str) -> PathBuf {
        self.mnt().join("doc").join(rel)
    }

    fn read_source(&self) -> String {
        fs::read_to_string(self.source()).unwrap()
    }

    fn wait_mounted(&mut self) {
        let start = Instant::now();
        while !is_mounted(&self.mnt()) {
            if let Some(child) = self.child.as_mut()
                && let Some(status) = child.try_wait().unwrap()
            {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    use std::io::Read;
                    let _ = pipe.read_to_string(&mut stderr);
                }
                panic!("mq-mount exited ({status}) before mounting:\n{stderr}");
            }
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "timed out waiting for the mount"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits until the source file satisfies `cond` (writes are flushed to
    /// disk in the background, within ~150ms).
    fn wait_source(&self, what: &str, cond: impl Fn(&str) -> bool) {
        wait_until(what, || cond(&self.read_source()));
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            let start = Instant::now();
            while child.try_wait().ok().flatten().is_none() && start.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        if is_mounted(&self.mnt()) {
            let _ = Command::new("umount").arg("-f").arg("-l").arg(self.mnt()).status();
        }
    }
}

#[test]
fn reads_sections_with_ls_cat_and_grep() {
    if !enabled() {
        return;
    }
    let m = Mount::start(&[]);

    let mut names: Vec<String> = fs::read_dir(m.path("Guide"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["Install", "Remove Me", "Usage", "content.md"]);

    assert_eq!(fs::read_to_string(m.path("Guide/content.md")).unwrap(), "intro\n");
    assert_eq!(
        fs::read_to_string(m.path("Guide/Usage/content.md")).unwrap(),
        "old usage\n"
    );

    let out = run(Command::new("grep").arg("-rl").arg("step two").arg(m.mnt()));
    let hits = String::from_utf8_lossy(&out.stdout);
    assert!(hits.trim_end().ends_with("doc/Guide/Install/content.md"), "{hits}");
}

#[test]
fn the_default_mount_is_read_only() {
    if !enabled() {
        return;
    }
    let m = Mount::start(&[]);

    assert!(fs::write(m.path("Guide/content.md"), "nope\n").is_err());
    assert!(fs::create_dir(m.path("Guide/New")).is_err());
    assert_eq!(m.read_source(), DOC);
}

#[test]
fn appending_with_a_shell_redirect_reaches_the_source() {
    if !enabled() {
        return;
    }
    let m = Mount::start(&["--write"]);

    run(Command::new("sh").arg("-c").arg(format!(
        "echo 'more usage' >> '{}'",
        m.path("Guide/Usage/content.md").display()
    )));
    m.wait_source("the append to be flushed", |s| s.contains("old usage\nmore usage\n"));
    // Everything outside the edited section is byte-identical.
    let src = m.read_source();
    assert!(src.contains("* step one\n* step two\n\n|a|b|\n|-|-|\n|1|2|\n"), "{src}");
}

#[test]
fn sed_in_place_edits_reach_the_source() {
    if !enabled() {
        return;
    }
    let m = Mount::start(&["--write"]);

    // GNU sed -i writes a temp file next to the target and renames it over.
    run(Command::new("sed")
        .arg("-i")
        .arg("s/old usage/new usage/")
        .arg(m.path("Guide/Usage/content.md")));
    m.wait_source("the sed edit to be flushed", |s| {
        s.contains("new usage") && !s.contains("old usage")
    });
    assert_eq!(
        fs::read_to_string(m.path("Guide/Usage/content.md")).unwrap(),
        "new usage\n"
    );
}

#[test]
fn an_editor_style_atomic_save_reaches_the_source() {
    if !enabled() {
        return;
    }
    let m = Mount::start(&["--write"]);

    let tmp = m.path("Guide/Usage/.content.md.swp");
    fs::write(&tmp, "saved atomically\n").unwrap();
    fs::rename(&tmp, m.path("Guide/Usage/content.md")).unwrap();

    m.wait_source("the atomic save to be flushed", |s| s.contains("saved atomically"));
    assert!(!m.read_source().contains("old usage"));
    assert!(!m.path("Guide/Usage/.content.md.swp").exists());
}

#[test]
fn mkdir_mv_and_rm_r_edit_the_heading_structure() {
    if !enabled() {
        return;
    }
    let m = Mount::start(&["--write"]);

    fs::create_dir(m.path("Guide/FAQ")).unwrap();
    m.wait_source("mkdir to add a heading", |s| s.contains("## FAQ"));

    // Rename in place.
    run(Command::new("mv")
        .arg(m.path("Guide/Usage"))
        .arg(m.path("Guide/How To")));
    m.wait_source("mv to rename the heading", |s| {
        s.contains("## How To") && !s.contains("## Usage")
    });

    // Reparent: FAQ moves under Install and becomes a level-3 heading.
    run(Command::new("mv")
        .arg(m.path("Guide/FAQ"))
        .arg(m.path("Guide/Install/FAQ")));
    m.wait_source("mv to reparent the heading", |s| s.contains("### FAQ"));
    assert!(m.path("Guide/Install/FAQ/content.md").exists());

    run(Command::new("rm").arg("-r").arg(m.path("Guide/Remove Me")));
    m.wait_source("rm -r to delete the section", |s| {
        !s.contains("Remove Me") && !s.contains("gone soon")
    });
}

#[test]
fn toc_and_full_files_are_readable_and_read_only() {
    if !enabled() {
        return;
    }
    let m = Mount::start(&["--write", "--toc", "--full"]);

    let toc = fs::read_to_string(m.path("_toc.md")).unwrap();
    assert!(toc.contains("[Install](<Guide/Install/content.md>)"), "{toc}");
    let root_toc = fs::read_to_string(m.mnt().join("_toc.md")).unwrap();
    assert!(root_toc.contains("## doc"), "{root_toc}");

    let full = fs::read_to_string(m.path("Guide/_full.md")).unwrap();
    assert!(full.starts_with("# Guide\n\nintro\n\n## Install\n"), "{full}");
    assert!(full.contains("## Remove Me\n\ngone soon\n"), "{full}");

    assert!(fs::write(m.path("_toc.md"), "x").is_err());
    assert!(fs::write(m.path("Guide/_full.md"), "x").is_err());
}

#[test]
fn a_background_mount_is_listed_and_flushes_on_stop() {
    if !enabled() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("doc.md");
    let mnt = dir.path().join("mnt");
    fs::write(&source, DOC).unwrap();

    run(Command::new(BIN)
        .arg(&source)
        .arg(&mnt)
        .args(["--no-config", "--write", "-d"]));
    let stop = || {
        let _ = Command::new(BIN).arg("--stop").arg(&mnt).status();
        if is_mounted(&mnt) {
            let _ = Command::new("umount").arg("-f").arg("-l").arg(&mnt).status();
        }
    };
    struct OnDrop<F: FnMut()>(F);
    impl<F: FnMut()> Drop for OnDrop<F> {
        fn drop(&mut self) {
            (self.0)()
        }
    }
    let _cleanup = OnDrop(stop);

    wait_until("the background mount", || is_mounted(&mnt));

    let list = run(Command::new(BIN).args(["--list", "--json"]));
    let list = String::from_utf8_lossy(&list.stdout);
    let canonical = mnt.canonicalize().unwrap();
    assert!(
        list.contains(&format!("\"mountpoint\":\"{}\"", canonical.display())),
        "{list}"
    );
    assert!(
        list.contains(&format!("\"{}\"", source.canonicalize().unwrap().display())),
        "{list}"
    );

    fs::write(mnt.join("doc/Guide/Usage/content.md"), "written before stop\n").unwrap();
    run(Command::new(BIN).arg("--stop").arg(&mnt));

    assert!(!is_mounted(&mnt), "--stop must unmount");
    assert!(fs::read_to_string(&source).unwrap().contains("written before stop"));

    let list = run(Command::new(BIN).args(["--list", "--json"]));
    let list = String::from_utf8_lossy(&list.stdout);
    assert!(!list.contains(&canonical.display().to_string()), "{list}");
}
