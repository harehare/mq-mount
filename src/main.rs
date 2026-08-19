mod document;
mod fs;
mod inode;
mod vfs;

#[cfg(feature = "mount")]
mod backend;

#[cfg(feature = "mount")]
mod app {
    use std::fs;
    use std::path::{Path, PathBuf};

    use clap::Parser;

    use crate::fs::MqFs;

    /// Mount one or more Markdown files (or directories of them) as a
    /// virtual filesystem: each file gets a top-level directory named after
    /// it, headings become subdirectories, and each section's body becomes a
    /// `content.md` file.
    #[derive(Parser, Debug)]
    #[command(name = "mq-mount", author, version, about, long_about = None)]
    pub struct Cli {
        /// Markdown files and/or directories to mount, followed by the mount directory as the last argument (e.g. `a.md docs/ /mnt`).
        #[arg(required = true, num_args = 2..)]
        paths: Vec<PathBuf>,
        /// Mount read-only; all writes are rejected
        #[arg(long)]
        readonly: bool,
        /// Loosen file permission bits so other local users can read/write the mount (the underlying NFS server has no per-caller ACL to restrict access to the mounting user; no effect on Windows)
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
        let (sources, mountpoint) = cli.paths.split_at(split_at);
        let mountpoint = &mountpoint[0];

        if !mountpoint.is_dir() {
            miette::bail!(
                "mountpoint does not exist or is not a directory: {} (create it first)",
                mountpoint.display()
            );
        }

        let entries = collect_entries(sources)?;
        let entries = entries
            .into_iter()
            .map(|(path, mount_path)| {
                path.canonicalize()
                    .map(|resolved| (resolved, mount_path))
                    .map_err(|e| miette::miette!("failed to resolve {}: {e}", path.display()))
            })
            .collect::<miette::Result<Vec<_>>>()?;
        let file_count = entries.len();
        let name = mount_name(&entries);
        let filesystem = MqFs::new(entries, cli.readonly, cli.allow_other)
            .map_err(|e| miette::miette!("failed to read source file(s): {e}"))?;

        #[cfg(unix)]
        return crate::backend::nfs::run(filesystem, mountpoint, file_count, cli.readonly, &name);
        #[cfg(windows)]
        return crate::backend::winfsp::run(filesystem, mountpoint, file_count, cli.readonly, &name);
    }

    /// Derives an export/volume name identifying this mount instance (e.g.
    /// `mq-notes`), so multiple concurrently mounted instances are
    /// distinguishable in `mount`/`df` output or the Windows Explorer volume
    /// list instead of all showing up as the same fixed name.
    fn mount_name(entries: &[(PathBuf, Vec<String>)]) -> String {
        let mut top_level: Vec<&str> = Vec::new();
        for (_, mount_path) in entries {
            if let Some(first) = mount_path.first().map(String::as_str)
                && !top_level.contains(&first)
            {
                top_level.push(first);
            }
        }
        let label = match top_level.as_slice() {
            [] => String::new(),
            [only] => sanitize(only),
            [first, rest @ ..] => format!("{}+{}", sanitize(first), rest.len()),
        };
        let label = if label.is_empty() { "mount".to_string() } else { label };
        format!("mq-{}", truncate(&label, 40))
    }

    /// Keeps only ASCII alphanumerics, collapsing every other run of
    /// characters (spaces, punctuation, non-ASCII) into a single `-`, so the
    /// result is safe to use as an NFS export name or WinFSP filesystem/service name.
    fn sanitize(name: &str) -> String {
        let mut out = String::new();
        let mut last_dash = false;
        for c in name.chars() {
            if c.is_ascii_alphanumeric() {
                out.push(c);
                last_dash = false;
            } else if !last_dash && !out.is_empty() {
                out.push('-');
                last_dash = true;
            }
        }
        out.trim_end_matches('-').to_string()
    }

    fn truncate(s: &str, max_chars: usize) -> &str {
        match s.char_indices().nth(max_chars) {
            Some((idx, _)) => &s[..idx],
            None => s,
        }
    }

    fn collect_entries(sources: &[PathBuf]) -> miette::Result<Vec<(PathBuf, Vec<String>)>> {
        let mut out = Vec::new();
        for source in sources {
            if source.is_dir() {
                let base_name = stem_or_name(source);
                let mut components = vec![base_name];
                collect_markdown_files(source, &mut components, &mut out)?;
            } else if source.is_file() {
                out.push((source.clone(), vec![stem_or_name(source)]));
            } else {
                miette::bail!("not a file or directory: {}", source.display());
            }
        }
        if out.is_empty() {
            miette::bail!("no Markdown (.md) files found among the given paths");
        }
        Ok(out)
    }

    fn stem_or_name(path: &Path) -> String {
        path.file_stem()
            .or_else(|| path.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("untitled")
            .to_string()
    }

    fn collect_markdown_files(
        dir: &Path,
        components: &mut Vec<String>,
        out: &mut Vec<(PathBuf, Vec<String>)>,
    ) -> miette::Result<()> {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .map_err(|e| miette::miette!("failed to read directory {}: {e}", dir.display()))?
            .collect::<Result<_, _>>()
            .map_err(|e| miette::miette!("failed to read directory {}: {e}", dir.display()))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);

        for entry in entries {
            let path = entry.path();
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with('.') {
                continue;
            }
            let file_type = entry
                .file_type()
                .map_err(|e| miette::miette!("failed to stat {}: {e}", path.display()))?;
            if file_type.is_dir() {
                components.push(name.to_string());
                collect_markdown_files(&path, components, out)?;
                components.pop();
            } else if file_type.is_file() && path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("md")) {
                let mut mount_path = components.clone();
                mount_path.push(stem_or_name(&path));
                out.push((path, mount_path));
            }
        }
        Ok(())
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
