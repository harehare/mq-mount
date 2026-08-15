mod document;
#[cfg(feature = "mount")]
mod fs;
mod inode;

#[cfg(feature = "mount")]
mod app {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use clap::Parser;

    use crate::fs::MqFs;

    /// NFS-mount one or more Markdown files as a virtual filesystem: each file
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
        /// Loosen file permission bits so other local users can read/write
        /// the mount (the underlying NFS server has no per-caller ACL to
        /// restrict access to the mounting user)
        #[arg(long)]
        allow_other: bool,
        /// Enable verbose (debug) logging
        #[arg(short, long)]
        verbose: bool,
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
            .map(|f| {
                f.canonicalize()
                    .map_err(|e| miette::miette!("failed to resolve {}: {e}", f.display()))
            })
            .collect::<miette::Result<Vec<_>>>()?;
        let filesystem = MqFs::new(source_paths.clone(), cli.readonly, cli.allow_other)
            .map_err(|e| miette::miette!("failed to read source file(s): {e}"))?;

        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| miette::miette!("failed to start async runtime: {e}"))?
            .block_on(run_mounted(filesystem, mountpoint, source_paths.len(), cli.readonly))
    }

    async fn run_mounted(filesystem: MqFs, mountpoint: &Path, file_count: usize, readonly: bool) -> miette::Result<()> {
        use nfsserve::tcp::{NFSTcp, NFSTcpListener};

        let mut listener = NFSTcpListener::bind("127.0.0.1:0", filesystem)
            .await
            .map_err(|e| miette::miette!("failed to start NFS server: {e}"))?;
        listener.with_export_name("mq-mount");
        let port = listener.get_listen_port();
        let server = tokio::spawn(async move {
            let _ = listener.handle_forever().await;
        });

        mount_nfs(mountpoint, port, readonly)?;
        tracing::info!("mounted {file_count} file(s) at {}", mountpoint.display());

        tokio::signal::ctrl_c().await.ok();

        tracing::info!("unmounting");
        unmount(mountpoint)?;
        server.abort();
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn mount_nfs(mountpoint: &Path, port: u16, readonly: bool) -> miette::Result<()> {
        let mut opts = format!("noac,nolocks,vers=3,tcp,port={port},mountport={port}");
        if readonly {
            opts.push_str(",ro");
        }
        run_mount_command(Command::new("mount_nfs").args([
            "-o",
            &opts,
            "localhost:/mq-mount",
            &mountpoint.to_string_lossy(),
        ]))
    }

    #[cfg(target_os = "linux")]
    fn mount_nfs(mountpoint: &Path, port: u16, readonly: bool) -> miette::Result<()> {
        let mut opts = format!("noac,nolock,vers=3,tcp,port={port},mountport={port}");
        if readonly {
            opts.push_str(",ro");
        }
        run_mount_command(Command::new("mount").args([
            "-t",
            "nfs",
            "-o",
            &opts,
            "localhost:/mq-mount",
            &mountpoint.to_string_lossy(),
        ]))
    }

    fn run_mount_command(cmd: &mut Command) -> miette::Result<()> {
        let status = cmd
            .status()
            .map_err(|e| miette::miette!("failed to run mount command: {e}"))?;
        if !status.success() {
            miette::bail!("failed to mount: mount command exited with {status}");
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn unmount(mountpoint: &Path) -> miette::Result<()> {
        // Plain `umount` requires root for a non-FUSE mount even when owned by
        // the calling user; `diskutil unmount` goes through DiskArbitration and
        // works unprivileged.
        run_mount_command(Command::new("diskutil").args(["unmount", &mountpoint.to_string_lossy()]))
    }

    #[cfg(target_os = "linux")]
    fn unmount(mountpoint: &Path) -> miette::Result<()> {
        run_mount_command(Command::new("umount").arg(mountpoint))
    }
}

#[cfg(feature = "mount")]
fn main() -> miette::Result<()> {
    app::run()
}

#[cfg(not(feature = "mount"))]
fn main() {
    eprintln!("mq-mount was built without the `mount` feature. Rebuild with `cargo build --features mount`.");
    std::process::exit(1);
}
