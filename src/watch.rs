//! `--watch`: recursively watches each mounted *directory* argument for new
//! `.md` files and mounts them live via [`MqFs::watch_add`], without a
//! restart. Additions only; applies the same dotfile/dot-directory skip and
//! `.md`-only filter as the initial directory scan at mount time.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;

use notify::event::{ModifyKind, RenameMode};
use notify::{Event, EventKind, RecursiveMode, Watcher};

use crate::fs::MqFs;

/// Starts one watcher thread per `(canonical directory, its base mount-path
/// prefix)` pair; each runs for the life of the process.
pub fn spawn_watchers(fs: Arc<MqFs>, roots: Vec<(PathBuf, Vec<String>)>) {
    for (root, base_prefix) in roots {
        let fs = Arc::clone(&fs);
        std::thread::spawn(move || watch_root(fs, root, base_prefix));
    }
}

fn watch_root(fs: Arc<MqFs>, root: PathBuf, base_prefix: Vec<String>) {
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = match notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("failed to start watcher for {}: {e}", root.display());
            return;
        }
    };
    if let Err(e) = watcher.watch(&root, RecursiveMode::Recursive) {
        tracing::warn!("failed to watch {}: {e}", root.display());
        return;
    }
    tracing::info!("watching {} for new .md files", root.display());

    for res in rx {
        let Ok(event) = res else { continue };
        if !is_relevant(&event.kind) {
            continue;
        }
        for path in &event.paths {
            let Some(mount_path) = mount_path_for(&root, &base_prefix, path) else {
                continue;
            };
            let Ok(canonical) = path.canonicalize() else {
                continue;
            };
            fs.watch_add(canonical, mount_path);
        }
    }
}

fn is_relevant(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Modify(ModifyKind::Data(_))
            | EventKind::Modify(ModifyKind::Name(RenameMode::To))
    )
}

/// Derives the mount path a newly-seen `.md` file under `watch_root` should
/// get, or `None` if it's not a mountable `.md` file (extension mismatch, or
/// a dotfile/dot-directory anywhere in its path under the root).
fn mount_path_for(watch_root: &Path, base_prefix: &[String], created: &Path) -> Option<Vec<String>> {
    let rel = created.strip_prefix(watch_root).ok()?;
    if !rel.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("md")) {
        return None;
    }

    let mut components = base_prefix.to_vec();
    let mut parts = rel.components().peekable();
    while let Some(part) = parts.next() {
        let name = part.as_os_str().to_str()?;
        if name.starts_with('.') {
            return None;
        }
        if parts.peek().is_none() {
            components.push(Path::new(name).file_stem()?.to_str()?.to_string());
        } else {
            components.push(name.to_string());
        }
    }
    Some(components)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_a_direct_child_to_a_mount_path() {
        let root = Path::new("/docs");
        let path = root.join("a.md");
        let base = vec!["docs".to_string()];
        assert_eq!(
            mount_path_for(root, &base, &path),
            Some(vec!["docs".to_string(), "a".to_string()])
        );
    }

    #[test]
    fn maps_a_nested_child_through_intermediate_directories() {
        let root = Path::new("/docs");
        let path = root.join("guide/a.md");
        let base = vec!["docs".to_string()];
        assert_eq!(
            mount_path_for(root, &base, &path),
            Some(vec!["docs".to_string(), "guide".to_string(), "a".to_string()])
        );
    }

    #[test]
    fn rejects_non_markdown_files() {
        let root = Path::new("/docs");
        let path = root.join("notes.txt");
        assert_eq!(mount_path_for(root, &[], &path), None);
    }

    #[test]
    fn rejects_dotfiles_and_dot_directories() {
        let root = Path::new("/docs");
        assert_eq!(mount_path_for(root, &[], &root.join(".hidden.md")), None);
        assert_eq!(mount_path_for(root, &[], &root.join(".git/a.md")), None);
    }

    #[test]
    fn rejects_a_path_outside_the_watch_root() {
        let root = Path::new("/docs");
        let path = Path::new("/other/a.md");
        assert_eq!(mount_path_for(root, &[], path), None);
    }
}
