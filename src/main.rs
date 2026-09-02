mod document;
mod fs;
mod inode;
mod vfs;

#[cfg(feature = "mount")]
mod backend;

#[cfg(feature = "mount")]
mod daemon;

#[cfg(feature = "mount")]
mod query_filter;

#[cfg(feature = "mount")]
mod watch;

#[cfg(feature = "mount")]
mod app {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use clap::Parser;

    use crate::fs::MqFs;

    /// Mount one or more Markdown files (or directories of them) as a
    /// virtual filesystem: each file gets a top-level directory named after
    /// it, headings become subdirectories, and each section's body becomes a
    /// `content.md` file.
    #[derive(Parser, Debug)]
    #[command(name = "mq-mount", author, version, about, long_about = None)]
    pub struct Cli {
        /// Markdown files and/or directories to mount, followed by the mount directory as the last argument (e.g. `a.md docs/ /mnt`). Omit when using `--stop`.
        #[arg(num_args = 0..)]
        paths: Vec<PathBuf>,
        /// Allow writes to the source Markdown file(s) (default: read-only)
        #[arg(long)]
        write: bool,
        /// Loosen file permission bits so other local users can read/write the mount (the underlying NFS server has no per-caller ACL to restrict access to the mounting user; no effect on Windows)
        #[arg(long)]
        allow_other: bool,
        /// Auto-mount new .md files added under a mounted directory; also
        /// unmounts a file deleted or renamed away outside the mount
        #[arg(long)]
        watch: bool,
        /// Expose a read-only `_toc.md` at each mounted file's root, listing
        /// its whole heading tree with links to each section's content.md
        #[arg(long)]
        toc: bool,
        /// Only mount .md files under a directory argument whose path
        /// (relative to that argument) matches this glob; repeatable
        #[arg(long = "include", value_name = "GLOB")]
        include: Vec<String>,
        /// Skip .md files under a directory argument whose path (relative to
        /// that argument) matches this glob; repeatable, applied after
        /// --include
        #[arg(long = "exclude", value_name = "GLOB")]
        exclude: Vec<String>,
        /// Only expose sections whose heading matches this mq query (e.g.
        /// `.h1`, `select(contains("TODO"))`); ancestors of a match stay
        /// visible so it remains reachable by path. Always mounts
        /// read-only, regardless of --write
        #[arg(long, value_name = "QUERY")]
        filter: Option<String>,
        /// Before the first write to each source file, save its pre-edit
        /// bytes to a sibling `<file>.orig` (skipped if one already exists)
        #[arg(long)]
        backup: bool,
        /// Run detached from the terminal; the child keeps running once this process exits
        #[arg(short = 'd', long)]
        background: bool,
        /// Stop a running mount (background or foreground) at this mountpoint and exit
        #[arg(long, value_name = "MOUNTPOINT", conflicts_with_all = ["paths", "write", "allow_other", "watch", "toc", "include", "exclude", "filter", "backup", "background", "list"])]
        stop: Option<PathBuf>,
        /// List currently running background mounts and exit
        #[arg(long, conflicts_with_all = ["paths", "write", "allow_other", "watch", "toc", "include", "exclude", "filter", "backup", "background", "stop"])]
        list: bool,
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

        if let Some(mountpoint) = &cli.stop {
            return crate::daemon::stop(mountpoint);
        }

        if cli.list {
            return crate::daemon::list();
        }

        if cli.paths.len() < 2 {
            miette::bail!("expected at least a source and a mount directory (e.g. `mq-mount a.md /mnt`)");
        }

        let split_at = cli.paths.len() - 1;
        let (sources, mountpoint) = cli.paths.split_at(split_at);
        let mountpoint = &mountpoint[0];

        if !mountpoint.is_dir() {
            miette::bail!(
                "mountpoint does not exist or is not a directory: {} (create it first)",
                mountpoint.display()
            );
        }
        // Canonicalized so it matches how `--stop` resolves the same path
        // (e.g. macOS's /var/folders -> /private/var/folders), and used
        // consistently for the pid file and the background re-exec below.
        let mountpoint = &mountpoint
            .canonicalize()
            .map_err(|e| miette::miette!("failed to resolve {}: {e}", mountpoint.display()))?;

        let readonly = effective_readonly(cli.write, cli.filter.is_some());
        if cli.write && cli.filter.is_some() {
            tracing::warn!("--filter always mounts read-only; ignoring --write");
        }
        let heading_filter = cli.filter.as_deref().map(crate::query_filter::compile).transpose()?;

        let include = compile_globs(&cli.include)?;
        let exclude = compile_globs(&cli.exclude)?;

        let entries = collect_entries(sources, &include, &exclude)?;
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

        let watch_roots = watch_roots_for(sources, cli.watch)?;

        if cli.background {
            return crate::daemon::spawn_background(mountpoint);
        }

        let filesystem = Arc::new(
            MqFs::new(entries, readonly, cli.allow_other, cli.toc, cli.backup, heading_filter)
                .map_err(|e| miette::miette!("failed to read source file(s): {e}"))?,
        );
        if !watch_roots.is_empty() {
            crate::watch::spawn_watchers(Arc::clone(&filesystem), watch_roots, include, exclude);
        }
        spawn_background_sync(Arc::clone(&filesystem));

        let _pidfile = crate::daemon::PidFileGuard::create(mountpoint)
            .map_err(|e| miette::miette!("failed to write pid file: {e}"))?;

        #[cfg(unix)]
        return crate::backend::nfs::run(filesystem, mountpoint, file_count, readonly, &name);
        #[cfg(windows)]
        return crate::backend::winfsp::run(filesystem, mountpoint, file_count, readonly, &name);
    }

    /// Coalesces bursts of small writes into one render+disk-write per file
    /// (see [`MqFs::flush`]) and picks up files changed on disk outside the
    /// mount (see [`MqFs::refresh_external`]), on a short fixed interval for
    /// the life of the mount.
    fn spawn_background_sync(fs: Arc<MqFs>) {
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(150));
                fs.flush();
                fs.refresh_external();
            }
        });
    }

    /// Canonical `(directory, base mount-path prefix)` for every directory
    /// source argument, when `--watch` is set; empty (with a warning) if
    /// `--watch` was given but every source is a plain file.
    fn watch_roots_for(sources: &[PathBuf], watch: bool) -> miette::Result<Vec<(PathBuf, Vec<String>)>> {
        if !watch {
            return Ok(Vec::new());
        }
        let roots = sources
            .iter()
            .filter(|s| s.is_dir())
            .map(|s| {
                s.canonicalize()
                    .map(|resolved| (resolved, vec![stem_or_name(s)]))
                    .map_err(|e| miette::miette!("failed to resolve {}: {e}", s.display()))
            })
            .collect::<miette::Result<Vec<_>>>()?;
        if roots.is_empty() {
            tracing::warn!("--watch has no effect: none of the given sources are directories");
        }
        Ok(roots)
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

    /// A `--filter`ed mount is always read-only: writing into a document
    /// whose non-matching sections are hidden from the mount is out of
    /// scope (there'd be no way to splice an edit back in among content the
    /// mount never showed), so `--filter` overrides `--write` rather than
    /// conflicting with it.
    fn effective_readonly(write: bool, filtered: bool) -> bool {
        !write || filtered
    }

    /// Compiles `--include`/`--exclude` globs, matched against a `.md`
    /// file's path relative to the directory argument it was found under.
    pub(crate) fn compile_globs(patterns: &[String]) -> miette::Result<Vec<glob::Pattern>> {
        patterns
            .iter()
            .map(|p| glob::Pattern::new(p).map_err(|e| miette::miette!("invalid glob {p:?}: {e}")))
            .collect()
    }

    /// Whether `rel` (a `.md` file's path relative to the directory argument
    /// it was found under) passes `--include`/`--exclude`: it must match at
    /// least one `include` pattern (if any are given), and none of `exclude`.
    pub(crate) fn glob_allows(rel: &Path, include: &[glob::Pattern], exclude: &[glob::Pattern]) -> bool {
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if !include.is_empty() && !include.iter().any(|p| p.matches(&rel_str)) {
            return false;
        }
        !exclude.iter().any(|p| p.matches(&rel_str))
    }

    fn collect_entries(
        sources: &[PathBuf],
        include: &[glob::Pattern],
        exclude: &[glob::Pattern],
    ) -> miette::Result<Vec<(PathBuf, Vec<String>)>> {
        let mut out = Vec::new();
        for source in sources {
            if source.is_dir() {
                let base_name = stem_or_name(source);
                let mut components = vec![base_name];
                collect_markdown_files(source, source, &mut components, include, exclude, &mut out)?;
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
        root: &Path,
        dir: &Path,
        components: &mut Vec<String>,
        include: &[glob::Pattern],
        exclude: &[glob::Pattern],
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
                collect_markdown_files(root, &path, components, include, exclude, out)?;
                components.pop();
            } else if file_type.is_file() && path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("md")) {
                let rel = path.strip_prefix(root).unwrap_or(&path);
                if !glob_allows(rel, include, exclude) {
                    continue;
                }
                let mut mount_path = components.clone();
                mount_path.push(stem_or_name(&path));
                out.push((path, mount_path));
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn filter_forces_read_only_even_with_write() {
            assert!(effective_readonly(true, true));
            assert!(!effective_readonly(true, false));
            assert!(effective_readonly(false, false));
        }

        #[test]
        fn glob_allows_matches_only_included_patterns() {
            let include = compile_globs(&["guide/*.md".to_string()]).unwrap();
            assert!(glob_allows(Path::new("guide/a.md"), &include, &[]));
            assert!(!glob_allows(Path::new("api/a.md"), &include, &[]));
        }

        #[test]
        fn glob_allows_applies_exclude_after_include() {
            let include = compile_globs(&["**/*.md".to_string()]).unwrap();
            let exclude = compile_globs(&["**/draft-*.md".to_string()]).unwrap();
            assert!(glob_allows(Path::new("guide/a.md"), &include, &exclude));
            assert!(!glob_allows(Path::new("guide/draft-a.md"), &include, &exclude));
        }

        #[test]
        fn glob_allows_with_no_include_patterns_allows_everything_but_excludes() {
            let exclude = compile_globs(&["*.tmp.md".to_string()]).unwrap();
            assert!(glob_allows(Path::new("a.md"), &[], &exclude));
            assert!(!glob_allows(Path::new("a.tmp.md"), &[], &exclude));
        }

        #[test]
        fn collect_entries_applies_include_and_exclude_under_a_directory_argument() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("guide")).unwrap();
            std::fs::create_dir_all(dir.path().join("api")).unwrap();
            std::fs::write(dir.path().join("guide/a.md"), "# A\n").unwrap();
            std::fs::write(dir.path().join("guide/draft-b.md"), "# B\n").unwrap();
            std::fs::write(dir.path().join("api/c.md"), "# C\n").unwrap();

            let include = compile_globs(&["guide/*.md".to_string()]).unwrap();
            let exclude = compile_globs(&["guide/draft-*.md".to_string()]).unwrap();
            let entries = collect_entries(&[dir.path().to_path_buf()], &include, &exclude).unwrap();

            let names: Vec<String> = entries.iter().map(|(p, _)| stem_or_name(p)).collect();
            assert_eq!(names, vec!["a"]);
        }
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
