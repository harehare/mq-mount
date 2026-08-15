mod document;
#[cfg(feature = "mount")]
mod fs;
mod inode;

#[cfg(feature = "mount")]
mod app {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use clap::Parser;

    use crate::fs::MqFs;

    /// FUSE-mount one or more Markdown files as a virtual filesystem: each file
    /// gets a top-level directory named after it, headings become
    /// subdirectories, and each section's body becomes a `content.md` file.
    #[derive(Parser, Debug)]
    #[command(name = "mq-mount", author, version, about, long_about = None)]
    pub struct Cli {
        /// Markdown files to mount, followed by the mount directory as the
        /// last argument (e.g. `a.md b.md /mnt`)
        #[arg(required = true, num_args = 2..)]
        paths: Vec<PathBuf>,
        /// Mount read-only; all writes are rejected
        #[arg(long)]
        readonly: bool,
        /// Allow other users on the machine to access the mount
        #[arg(long)]
        allow_other: bool,
        /// Enable verbose (debug) logging
        #[arg(short, long)]
        verbose: bool,
    }

    static INTERRUPTED: AtomicBool = AtomicBool::new(false);

    extern "C" fn on_sigint(_signum: libc::c_int) {
        INTERRUPTED.store(true, Ordering::SeqCst);
    }

    fn wait_for_sigint() {
        // Safety: `on_sigint` only touches a static AtomicBool, which is
        // async-signal-safe; no allocation or locking happens in it.
        unsafe {
            libc::signal(libc::SIGINT, on_sigint as libc::sighandler_t);
        }
        while !INTERRUPTED.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    pub fn run() -> miette::Result<()> {
        let cli = Cli::parse();

        let filter = if cli.verbose { "mq_mount=debug" } else { "mq_mount=info" };
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .init();

        let split_at = cli.paths.len() - 1;
        let (files, mountpoint) = cli.paths.split_at(split_at);
        let mountpoint = &mountpoint[0];

        for file in files {
            if !file.is_file() {
                miette::bail!("not a file: {}", file.display());
            }
        }
        if !mountpoint.is_dir() {
            miette::bail!(
                "mountpoint does not exist or is not a directory: {} (create it first)",
                mountpoint.display()
            );
        }

        let source_paths = files
            .iter()
            .map(|f| f.canonicalize().map_err(|e| miette::miette!("failed to resolve {}: {e}", f.display())))
            .collect::<miette::Result<Vec<_>>>()?;
        let filesystem = MqFs::new(source_paths.clone(), cli.readonly)
            .map_err(|e| miette::miette!("failed to read source file(s): {e}"))?;

        let mut mount_options = vec![
            fuser::MountOption::FSName("mq-mount".to_string()),
            fuser::MountOption::Subtype("mqmount".to_string()),
        ];
        if cli.readonly {
            mount_options.push(fuser::MountOption::RO);
        }
        let acl = if cli.allow_other {
            fuser::SessionACL::All
        } else {
            fuser::SessionACL::Owner
        };
        // `Config` is `#[non_exhaustive]`, so it can't be built with struct-literal
        // syntax from outside `fuser` even via `..Default::default()` — mutate a
        // `Default`-constructed value instead.
        let mut config = fuser::Config::default();
        config.mount_options = mount_options;
        config.acl = acl;

        let session = fuser::spawn_mount(filesystem, mountpoint, &config)
            .map_err(|e| miette::miette!("failed to mount: {e}"))?;
        tracing::info!("mounted {} file(s) at {}", source_paths.len(), mountpoint.display());

        wait_for_sigint();

        tracing::info!("unmounting");
        session
            .umount_and_join()
            .map_err(|e| miette::miette!("failed to unmount cleanly: {e}"))?;
        Ok(())
    }
}

#[cfg(feature = "mount")]
fn main() -> miette::Result<()> {
    app::run()
}

#[cfg(not(feature = "mount"))]
fn main() {
    eprintln!(
        "mq-mount was built without the `mount` feature: fuser needs libfuse (Linux) or \
         macFUSE (macOS) present at build time. Install one of those and rebuild with \
         `cargo build --features mount`."
    );
    std::process::exit(1);
}
