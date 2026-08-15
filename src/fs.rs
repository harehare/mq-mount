//! `fuser::Filesystem` glue: translates FUSE calls into [`crate::document`]
//! mutations. State lives behind a `Mutex` since almost every trait method
//! takes `&self`.
//!
//! Each mounted file gets its own top-level directory (named after the file,
//! deduplicated like sibling headings) under a synthetic super-root at
//! `ROOT_INO`. The super-root itself is not backed by any document — it only
//! ever lists one directory per mounted file, fixed at mount time — so it is
//! deliberately kept out of `ino_owner` below: every handler's existing
//! "resolve this ino to a document path, or bail" fallback then naturally
//! rejects mkdir/rmdir/rename/create/unlink at or across that level without
//! needing many explicit `ino == ROOT_INO` special cases.

use std::ffi::OsStr;
use std::fs;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use fuser::{
    Errno, FileAttr, FileHandle as FuserFileHandle, FileType, Filesystem, Generation, INodeNo, RenameFlags, ReplyAttr,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request,
};
use rustc_hash::FxHashMap;

use crate::document::{self, Document, FrontMatterKind, MutationError, Section};
use crate::inode::{InodeTable, MountPath, ROOT_INO};

const TTL: Duration = Duration::from_secs(1);

enum Handle {
    Dir,
    Content {
        file: usize,
        path: Vec<String>,
        buf: Vec<u8>,
        dirty: bool,
    },
    FrontMatter {
        file: usize,
        kind: FrontMatterKind,
        buf: Vec<u8>,
        dirty: bool,
    },
    /// A file created under a name we don't recognize — most likely an
    /// editor's temp file staged for an atomic-save rename onto a canonical
    /// name. Never listed, never persisted on its own.
    Scratch {
        buf: Vec<u8>,
    },
}

/// One mounted Markdown file and everything derived from it.
struct FileMount {
    /// Top-level directory name (file stem, extension stripped, deduplicated
    /// against other mounted files' stems).
    slug: String,
    document: Document,
    tree: document::SectionTree,
    inodes: InodeTable,
    source_path: PathBuf,
    last_persisted: String,
}

struct MountState {
    files: Vec<FileMount>,
    /// Slug -> that file's root directory inode, fixed at mount time (the set
    /// of mounted files never changes for the life of the process).
    root_children: Vec<(String, u64)>,
    /// Inode -> index into `files`. Deliberately never contains `ROOT_INO`.
    ino_owner: FxHashMap<u64, usize>,
    /// Shared across every file's `InodeTable` so inode numbers stay globally
    /// unique across the whole mount.
    next_ino: u64,
    handles: FxHashMap<u64, Handle>,
    open_by_name: FxHashMap<(u64, String), u64>,
    pending_renames: FxHashMap<(u64, String), Vec<u8>>,
    next_fh: u64,
    readonly: bool,
    uid: u32,
    gid: u32,
}

impl MountState {
    fn open(source_paths: Vec<PathBuf>, readonly: bool) -> std::io::Result<Self> {
        let mut next_ino = ROOT_INO + 1;
        let mut seen_slugs: FxHashMap<String, u32> = FxHashMap::default();
        let mut files = Vec::with_capacity(source_paths.len());
        for source_path in source_paths {
            let text = fs::read_to_string(&source_path)?;
            let document = Document::parse(&text).map_err(|e| std::io::Error::other(e.to_string()))?;
            let tree = document.tree();
            let mut inodes = InodeTable::new();
            inodes.sync(&mut next_ino, &tree);

            let stem = source_path
                .file_stem()
                .and_then(|s| s.to_str())
                .or_else(|| source_path.file_name().and_then(|s| s.to_str()))
                .unwrap_or("untitled");
            let slug = document::unique_name(&mut seen_slugs, stem);

            files.push(FileMount {
                slug,
                document,
                tree,
                inodes,
                source_path,
                last_persisted: text,
            });
        }

        let root_children: Vec<(String, u64)> =
            files.iter().map(|f| (f.slug.clone(), f.inodes.root_dir_ino())).collect();
        let mut ino_owner = FxHashMap::default();
        for (idx, f) in files.iter().enumerate() {
            for (ino, _) in f.inodes.entries() {
                ino_owner.insert(ino, idx);
            }
        }

        // Safety: getuid/getgid take no arguments and cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Ok(Self {
            files,
            root_children,
            ino_owner,
            next_ino,
            handles: FxHashMap::default(),
            open_by_name: FxHashMap::default(),
            pending_renames: FxHashMap::default(),
            next_fh: 1,
            readonly,
            uid,
            gid,
        })
    }

    fn rebuild(&mut self, file_idx: usize) {
        let MountState {
            files,
            next_ino,
            ino_owner,
            ..
        } = self;
        let file = &mut files[file_idx];
        let old_inos: Vec<u64> = file.inodes.entries().map(|(ino, _)| ino).collect();
        file.tree = file.document.tree();
        file.inodes.sync(next_ino, &file.tree);
        for ino in old_inos {
            ino_owner.remove(&ino);
        }
        for (ino, _) in file.inodes.entries() {
            ino_owner.insert(ino, file_idx);
        }
    }

    fn persist(&mut self, file_idx: usize) -> std::io::Result<()> {
        let file = &mut self.files[file_idx];
        let rendered = file.document.render();
        if rendered != file.last_persisted {
            fs::write(&file.source_path, &rendered)?;
            file.last_persisted = rendered;
        }
        Ok(())
    }

    fn alloc_fh(&mut self) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        fh
    }

    fn ino_exists(&self, ino: u64) -> bool {
        ino == ROOT_INO || self.ino_owner.contains_key(&ino)
    }

    fn dir_path(&self, ino: u64) -> Option<(usize, Vec<String>)> {
        let &file_idx = self.ino_owner.get(&ino)?;
        match self.files[file_idx].inodes.path_for(ino)? {
            MountPath::Dir(p) => Some((file_idx, p.clone())),
            _ => None,
        }
    }

    fn path_for_ino(&self, ino: u64) -> Option<(usize, MountPath)> {
        let &file_idx = self.ino_owner.get(&ino)?;
        let path = self.files[file_idx].inodes.path_for(ino)?.clone();
        Some((file_idx, path))
    }

    fn find_section(&self, file_idx: usize, path: &[String]) -> Option<Section> {
        let refs: Vec<&str> = path.iter().map(String::as_str).collect();
        self.files[file_idx].tree.find(&refs).cloned()
    }

    /// Finds a named child section under `parent`, as used by `rmdir` and the
    /// heading-rename branch of `rename` to resolve the entry being acted on.
    fn find_child_section(&self, file_idx: usize, parent: &[String], name: &str) -> Option<Section> {
        self.find_section(file_idx, parent)
            .and_then(|p| p.children.into_iter().find(|c| c.name == name))
    }

    /// Like `find_section`, but for callers that only need the content range —
    /// avoids deep-cloning the section's whole (possibly large) child subtree.
    fn section_range(&self, file_idx: usize, path: &[String]) -> Option<Range<usize>> {
        let refs: Vec<&str> = path.iter().map(String::as_str).collect();
        self.files[file_idx].tree.find(&refs).map(|s| s.own_content_range.clone())
    }

    fn section_bytes(&self, file_idx: usize, path: &[String]) -> Vec<u8> {
        self.section_range(file_idx, path)
            .map(|r| self.files[file_idx].document.render_range(r).into_bytes())
            .unwrap_or_default()
    }

    /// Splices `text` into a section's content. If the section vanished
    /// concurrently (deleted while a handle was open elsewhere), this is a
    /// silent no-op rather than an error — matches how a local filesystem
    /// treats writes to an unlinked-but-still-open file.
    fn commit_content(&mut self, file_idx: usize, dir_path: &[String], text: &str) -> Result<(), Errno> {
        let Some(range) = self.section_range(file_idx, dir_path) else {
            return Ok(());
        };
        self.files[file_idx]
            .document
            .replace_section_content(range, text)
            .map_err(map_mutation_error)?;
        self.rebuild(file_idx);
        self.persist(file_idx).map_err(|_| Errno::EIO)?;
        Ok(())
    }

    fn commit_frontmatter(&mut self, file_idx: usize, kind: FrontMatterKind, text: &str) -> Result<(), Errno> {
        if let Some((found_kind, idx)) = self.files[file_idx].tree.front_matter
            && found_kind == kind
        {
            self.files[file_idx].document.set_frontmatter(idx, text.to_string());
            self.rebuild(file_idx);
            self.persist(file_idx).map_err(|_| Errno::EIO)?;
        }
        Ok(())
    }

    fn attr(&self, ino: u64, kind: FileType, size: u64) -> FileAttr {
        let now = SystemTime::now();
        let perm = match kind {
            FileType::Directory => 0o755,
            _ if self.readonly => 0o444,
            _ => 0o644,
        };
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind,
            perm,
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    fn attr_for(&self, file_idx: usize, ino: u64, path: &MountPath) -> FileAttr {
        match path {
            MountPath::Dir(_) => self.attr(ino, FileType::Directory, 0),
            MountPath::Content(p) => self.attr(ino, FileType::RegularFile, self.section_bytes(file_idx, p).len() as u64),
            MountPath::FrontMatter(kind) => {
                let size = self.files[file_idx]
                    .tree
                    .front_matter
                    .filter(|(k, _)| k == kind)
                    .map(|(_, idx)| self.files[file_idx].document.frontmatter_value(idx).len())
                    .unwrap_or(0);
                self.attr(ino, FileType::RegularFile, size as u64)
            }
        }
    }
}

fn map_mutation_error(e: MutationError) -> Errno {
    match e {
        MutationError::AlreadyExists(_) => Errno::EEXIST,
        MutationError::NotEmpty => Errno::ENOTEMPTY,
        MutationError::NotADirectory => Errno::ENOTDIR,
        MutationError::Parse(_) => Errno::EINVAL,
    }
}

fn frontmatter_kind_for_name(name: &str) -> Option<FrontMatterKind> {
    [FrontMatterKind::Yaml, FrontMatterKind::Toml]
        .into_iter()
        .find(|k| k.file_name() == name)
}

/// Emits `entries` into a readdir reply starting at `offset`, stopping early
/// if the reply buffer fills up.
fn emit_entries(reply: &mut ReplyDirectory, entries: Vec<(u64, FileType, String)>, offset: u64) {
    for (i, (entry_ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
        if reply.add(INodeNo(entry_ino), (i + 1) as u64, kind, &name) {
            break;
        }
    }
}

pub struct MqFs {
    state: Mutex<MountState>,
}

impl MqFs {
    pub fn new(source_paths: Vec<PathBuf>, readonly: bool) -> std::io::Result<Self> {
        Ok(Self {
            state: Mutex::new(MountState::open(source_paths, readonly)?),
        })
    }
}

impl Filesystem for MqFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let state = self.state.lock().unwrap();
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };

        if parent.0 == ROOT_INO {
            let Some(&(_, ino)) = state.root_children.iter().find(|(slug, _)| slug == name) else {
                reply.error(Errno::ENOENT);
                return;
            };
            reply.entry(&TTL, &state.attr(ino, FileType::Directory, 0), Generation(0));
            return;
        }

        let Some((file_idx, parent_path)) = state.dir_path(parent.0) else {
            reply.error(Errno::ENOTDIR);
            return;
        };

        let child_path = if name == document::CONTENT_FILE {
            MountPath::Content(parent_path)
        } else if parent_path.is_empty()
            && let Some(kind) = frontmatter_kind_for_name(name)
        {
            MountPath::FrontMatter(kind)
        } else if state
            .find_section(file_idx, &parent_path)
            .is_some_and(|s| s.children.iter().any(|c| c.name == name))
        {
            let mut p = parent_path;
            p.push(name.to_string());
            MountPath::Dir(p)
        } else {
            reply.error(Errno::ENOENT);
            return;
        };

        match state.files[file_idx].inodes.ino_for(&child_path) {
            Some(ino) => reply.entry(&TTL, &state.attr_for(file_idx, ino, &child_path), Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FuserFileHandle>, reply: ReplyAttr) {
        let state = self.state.lock().unwrap();
        if let Some(fh) = fh
            && let Some(Handle::Scratch { buf }) = state.handles.get(&fh.0)
        {
            reply.attr(&TTL, &state.attr(ino.0, FileType::RegularFile, buf.len() as u64));
            return;
        }
        if ino.0 == ROOT_INO {
            reply.attr(&TTL, &state.attr(ino.0, FileType::Directory, 0));
            return;
        }
        match state.path_for_ino(ino.0) {
            Some((file_idx, path)) => reply.attr(&TTL, &state.attr_for(file_idx, ino.0, &path)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FuserFileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let mut state = self.state.lock().unwrap();
        if state.readonly {
            reply.error(Errno::EROFS);
            return;
        }
        if let Some(0) = size {
            // O_TRUNC: prefer clearing the live buffer of an already-open handle
            // (write()s that follow will rebuild it); otherwise clear the
            // underlying section directly.
            if let Some(fh) = fh
                && let Some(handle) = state.handles.get_mut(&fh.0)
            {
                match handle {
                    Handle::Content { buf, dirty, .. } | Handle::FrontMatter { buf, dirty, .. } => {
                        buf.clear();
                        *dirty = true;
                    }
                    Handle::Scratch { buf } => buf.clear(),
                    Handle::Dir => {}
                }
            } else if let Some((file_idx, path)) = state.path_for_ino(ino.0) {
                let result = match &path {
                    MountPath::Content(p) => state.commit_content(file_idx, p, ""),
                    MountPath::FrontMatter(kind) => state.commit_frontmatter(file_idx, *kind, ""),
                    MountPath::Dir(_) => Err(Errno::EISDIR),
                };
                if let Err(e) = result {
                    reply.error(e);
                    return;
                }
            }
        }
        if ino.0 == ROOT_INO {
            reply.attr(&TTL, &state.attr(ino.0, FileType::Directory, 0));
            return;
        }
        match state.path_for_ino(ino.0) {
            Some((file_idx, path)) => reply.attr(&TTL, &state.attr_for(file_idx, ino.0, &path)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readlink(&self, _req: &Request, _ino: INodeNo, reply: ReplyData) {
        reply.error(Errno::ENOSYS);
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        let mut state = self.state.lock().unwrap();
        if state.readonly {
            reply.error(Errno::EROFS);
            return;
        }
        if parent.0 == ROOT_INO {
            // No backing source file to create: the top-level layout is fixed
            // at mount time, one directory per mounted file.
            reply.error(Errno::EPERM);
            return;
        }
        let (Some(name), Some((file_idx, parent_path))) = (name.to_str(), state.dir_path(parent.0)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(parent_section) = state.find_section(file_idx, &parent_path) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if let Err(e) = state.files[file_idx].document.insert_heading(&parent_section, name) {
            reply.error(map_mutation_error(e));
            return;
        }
        state.rebuild(file_idx);
        if state.persist(file_idx).is_err() {
            reply.error(Errno::EIO);
            return;
        }
        let mut child_path = parent_path;
        child_path.push(name.to_string());
        let dir_path = MountPath::Dir(child_path);
        match state.files[file_idx].inodes.ino_for(&dir_path) {
            Some(ino) => reply.entry(&TTL, &state.attr_for(file_idx, ino, &dir_path), Generation(0)),
            None => reply.error(Errno::EIO),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let mut state = self.state.lock().unwrap();
        if state.readonly {
            reply.error(Errno::EROFS);
            return;
        }
        if parent.0 == ROOT_INO {
            reply.error(Errno::EPERM);
            return;
        }
        let (Some(name), Some((file_idx, parent_path))) = (name.to_str(), state.dir_path(parent.0)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(section) = state.find_child_section(file_idx, &parent_path, name) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match state.files[file_idx].document.remove_heading(&section) {
            Ok(()) => {
                state.rebuild(file_idx);
                match state.persist(file_idx) {
                    Ok(()) => reply.ok(),
                    Err(_) => reply.error(Errno::EIO),
                }
            }
            Err(e) => reply.error(map_mutation_error(e)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let mut state = self.state.lock().unwrap();
        if state.readonly {
            reply.error(Errno::EROFS);
            return;
        }
        if parent.0 == ROOT_INO {
            reply.error(Errno::EPERM);
            return;
        }
        let (Some(name), Some((file_idx, parent_path))) = (name.to_str(), state.dir_path(parent.0)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let result = if name == document::CONTENT_FILE {
            state.commit_content(file_idx, &parent_path, "")
        } else if parent_path.is_empty()
            && let Some(kind) = frontmatter_kind_for_name(name)
        {
            state.commit_frontmatter(file_idx, kind, "")
        } else {
            Err(Errno::ENOENT)
        };
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let mut state = self.state.lock().unwrap();
        if state.readonly {
            reply.error(Errno::EROFS);
            return;
        }
        let (Some(name), Some(newname)) = (name.to_str(), newname.to_str()) else {
            reply.error(Errno::EINVAL);
            return;
        };
        let Some((dest_file_idx, new_parent_path)) = state.dir_path(newparent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let dest_kind = if newname == document::CONTENT_FILE {
            Some(None)
        } else if new_parent_path.is_empty() {
            frontmatter_kind_for_name(newname).map(Some)
        } else {
            None
        };

        if let Some(front_matter_kind) = dest_kind {
            let key = (parent.0, name.to_string());
            // `clear_source_path` is only set for the "no temp file, this is a
            // direct rename of a live canonical file" fallback — a handle or
            // pending-rename buffer already represents a file with no other
            // backing content to leave behind, so only that case needs the
            // source explicitly cleared afterwards to complete the move.
            let (bytes, clear_source_path) = if let Some(&fh) = state.open_by_name.get(&key) {
                let bytes = match state.handles.get(&fh) {
                    Some(Handle::Scratch { buf })
                    | Some(Handle::Content { buf, .. })
                    | Some(Handle::FrontMatter { buf, .. }) => Some(buf.clone()),
                    _ => None,
                };
                (bytes, None)
            } else if let Some(buf) = state.pending_renames.remove(&key) {
                (Some(buf), None)
            } else if name == document::CONTENT_FILE {
                match state.dir_path(parent.0) {
                    Some((src_file_idx, _)) if src_file_idx != dest_file_idx => {
                        // content.md can't move between two different mounted
                        // files' documents, only within one.
                        reply.error(Errno::EOPNOTSUPP);
                        return;
                    }
                    Some((src_file_idx, src_path)) => {
                        let bytes = state.section_bytes(src_file_idx, &src_path);
                        (Some(bytes), Some(src_path))
                    }
                    None => (None, None),
                }
            } else {
                (None, None)
            };

            let Some(bytes) = bytes else {
                reply.error(Errno::ENOENT);
                return;
            };
            let text = String::from_utf8_lossy(&bytes).into_owned();

            let result = match front_matter_kind {
                None => state.commit_content(dest_file_idx, &new_parent_path, &text),
                Some(kind) => state.commit_frontmatter(dest_file_idx, kind, &text),
            }
            .and_then(|()| match clear_source_path {
                Some(p) => state.commit_content(dest_file_idx, &p, ""),
                None => Ok(()),
            });
            state.open_by_name.remove(&key);
            state.pending_renames.remove(&key);
            match result {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e),
            }
            return;
        }

        // Not a canonical destination: if the source is itself a scratch file
        // (an editor's temp file mid-way through a multi-step atomic-save
        // dance, e.g. `temp1 -> temp2 -> target`), just re-key it rather than
        // treating this as a heading rename. This is file-boundary-agnostic:
        // the bytes are opaque until they land on a canonical name above.
        let src_key = (parent.0, name.to_string());
        if state.open_by_name.contains_key(&src_key) || state.pending_renames.contains_key(&src_key) {
            let new_key = (newparent.0, newname.to_string());
            if let Some(fh) = state.open_by_name.remove(&src_key) {
                state.open_by_name.insert(new_key.clone(), fh);
            }
            if let Some(buf) = state.pending_renames.remove(&src_key) {
                state.pending_renames.insert(new_key, buf);
            }
            reply.ok();
            return;
        }

        if parent != newparent {
            reply.error(Errno::EOPNOTSUPP);
            return;
        }
        // `dir_path(ROOT_INO)` is `None`, so renaming a top-level per-file
        // directory falls through to ENOENT here — the mounted-file layout is
        // fixed at mount time.
        let Some((file_idx, parent_path)) = state.dir_path(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(section) = state.find_child_section(file_idx, &parent_path, name) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match state.files[file_idx].document.rename_heading(&section, newname) {
            Ok(()) => {
                state.rebuild(file_idx);
                match state.persist(file_idx) {
                    Ok(()) => reply.ok(),
                    Err(_) => reply.error(Errno::EIO),
                }
            }
            Err(e) => reply.error(map_mutation_error(e)),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: fuser::OpenFlags, reply: ReplyOpen) {
        let mut state = self.state.lock().unwrap();
        let wants_write = (flags.0 & libc::O_ACCMODE) != libc::O_RDONLY;
        if state.readonly && wants_write {
            reply.error(Errno::EROFS);
            return;
        }
        let Some((file_idx, path)) = state.path_for_ino(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let handle = match &path {
            MountPath::Content(p) => Handle::Content {
                file: file_idx,
                path: p.clone(),
                buf: state.section_bytes(file_idx, p),
                dirty: false,
            },
            MountPath::FrontMatter(kind) => {
                let buf = state.files[file_idx]
                    .tree
                    .front_matter
                    .filter(|(k, _)| k == kind)
                    .map(|(_, idx)| state.files[file_idx].document.frontmatter_value(idx).as_bytes().to_vec())
                    .unwrap_or_default();
                Handle::FrontMatter {
                    file: file_idx,
                    kind: *kind,
                    buf,
                    dirty: false,
                }
            }
            MountPath::Dir(_) => {
                reply.error(Errno::EISDIR);
                return;
            }
        };
        let fh = state.alloc_fh();
        state.handles.insert(fh, handle);
        reply.opened(FuserFileHandle(fh), fuser::FopenFlags::empty());
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        let mut state = self.state.lock().unwrap();
        if state.readonly {
            reply.error(Errno::EROFS);
            return;
        }
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let Some((file_idx, parent_path)) = state.dir_path(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let _ = flags;

        let (handle, path_for_attr) = if name == document::CONTENT_FILE {
            (
                Handle::Content {
                    file: file_idx,
                    path: parent_path.clone(),
                    buf: Vec::new(),
                    dirty: true,
                },
                Some(MountPath::Content(parent_path.clone())),
            )
        } else if parent_path.is_empty()
            && let Some(kind) = frontmatter_kind_for_name(name)
        {
            (
                Handle::FrontMatter {
                    file: file_idx,
                    kind,
                    buf: Vec::new(),
                    dirty: true,
                },
                Some(MountPath::FrontMatter(kind)),
            )
        } else {
            // Not a canonical name: stage it as a scratch buffer, most likely
            // an editor's temp file en route to an atomic-save rename.
            (Handle::Scratch { buf: Vec::new() }, None)
        };

        let fh = state.alloc_fh();
        let key = (parent.0, name.to_string());
        state.open_by_name.insert(key, fh);
        state.handles.insert(fh, handle);

        let ino = path_for_attr
            .as_ref()
            .and_then(|p| state.files[file_idx].inodes.ino_for(p))
            .unwrap_or_else(|| {
                // Scratch files have no place in the document tree; borrow an
                // inode number from the same never-reused counter by piggy-backing
                // on the fh space shifted above the inode range used so far. This
                // is only ever exposed transiently for the lifetime of the handle.
                u64::MAX - fh
            });
        let attr = state.attr(ino, FileType::RegularFile, 0);
        reply.created(
            &TTL,
            &attr,
            Generation(0),
            FuserFileHandle(fh),
            fuser::FopenFlags::empty(),
        );
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuserFileHandle,
        offset: u64,
        size: u32,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let state = self.state.lock().unwrap();
        let buf = match state.handles.get(&fh.0) {
            Some(Handle::Content { buf, .. })
            | Some(Handle::FrontMatter { buf, .. })
            | Some(Handle::Scratch { buf }) => buf,
            _ => {
                reply.error(Errno::EBADF);
                return;
            }
        };
        let offset = offset as usize;
        if offset >= buf.len() {
            reply.data(&[]);
            return;
        }
        let end = (offset + size as usize).min(buf.len());
        reply.data(&buf[offset..end]);
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuserFileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        let mut state = self.state.lock().unwrap();
        if state.readonly {
            reply.error(Errno::EROFS);
            return;
        }
        let offset = offset as usize;
        let buf = match state.handles.get_mut(&fh.0) {
            Some(Handle::Content { buf, dirty, .. }) | Some(Handle::FrontMatter { buf, dirty, .. }) => {
                *dirty = true;
                buf
            }
            Some(Handle::Scratch { buf }) => buf,
            _ => {
                reply.error(Errno::EBADF);
                return;
            }
        };
        if offset + data.len() > buf.len() {
            buf.resize(offset + data.len(), 0);
        }
        buf[offset..offset + data.len()].copy_from_slice(data);
        reply.written(data.len() as u32);
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuserFileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        let mut state = self.state.lock().unwrap();
        commit_handle_if_dirty(&mut state, fh.0);
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuserFileHandle,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let mut state = self.state.lock().unwrap();
        commit_handle_if_dirty(&mut state, fh.0);
        let key = state
            .open_by_name
            .iter()
            .find(|&(_, &v)| v == fh.0)
            .map(|(k, _)| k.clone());
        if let Some(ref k) = key {
            state.open_by_name.remove(k);
        }
        if let Some(Handle::Scratch { buf }) = state.handles.remove(&fh.0) {
            // Keep the bytes around in case a rename() targeting a canonical
            // name arrives after the editor has already closed this handle.
            if let Some(k) = key {
                state.pending_renames.insert(k, buf);
            }
        }
        reply.ok();
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: fuser::OpenFlags, reply: ReplyOpen) {
        let mut state = self.state.lock().unwrap();
        let fh = state.alloc_fh();
        state.handles.insert(fh, Handle::Dir);
        reply.opened(FuserFileHandle(fh), fuser::FopenFlags::empty());
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FuserFileHandle, offset: u64, mut reply: ReplyDirectory) {
        let state = self.state.lock().unwrap();

        if ino.0 == ROOT_INO {
            let mut entries: Vec<(u64, FileType, String)> = vec![
                (ROOT_INO, FileType::Directory, ".".to_string()),
                (ROOT_INO, FileType::Directory, "..".to_string()),
            ];
            for (slug, dir_ino) in &state.root_children {
                entries.push((*dir_ino, FileType::Directory, slug.clone()));
            }
            emit_entries(&mut reply, entries, offset);
            reply.ok();
            return;
        }

        let Some((file_idx, path)) = state.dir_path(ino.0) else {
            reply.error(Errno::ENOTDIR);
            return;
        };
        let Some(section) = state.find_section(file_idx, &path) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let parent_ino = if path.is_empty() {
            ROOT_INO
        } else {
            state.files[file_idx]
                .inodes
                .ino_for(&MountPath::Dir(path[..path.len() - 1].to_vec()))
                .unwrap_or(ROOT_INO)
        };
        let content_ino = state.files[file_idx]
            .inodes
            .ino_for(&MountPath::Content(path.clone()))
            .unwrap_or(ino.0);

        let mut entries: Vec<(u64, FileType, String)> = vec![
            (ino.0, FileType::Directory, ".".to_string()),
            (parent_ino, FileType::Directory, "..".to_string()),
            (content_ino, FileType::RegularFile, document::CONTENT_FILE.to_string()),
        ];
        if path.is_empty()
            && let Some((kind, _)) = state.files[file_idx].tree.front_matter
        {
            let mp = MountPath::FrontMatter(kind);
            if let Some(fm_ino) = state.files[file_idx].inodes.ino_for(&mp) {
                entries.push((fm_ino, FileType::RegularFile, kind.file_name().to_string()));
            }
        }
        for child in &section.children {
            if let Some(child_ino) = state.files[file_idx].inodes.ino_for(&MountPath::Dir({
                let mut p = path.clone();
                p.push(child.name.clone());
                p
            })) {
                entries.push((child_ino, FileType::Directory, child.name.clone()));
            }
        }

        emit_entries(&mut reply, entries, offset);
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuserFileHandle,
        _flags: fuser::OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.state.lock().unwrap().handles.remove(&fh.0);
        reply.ok();
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, _fh: FuserFileHandle, _datasync: bool, reply: ReplyEmpty) {
        reply.ok();
    }

    fn fsyncdir(&self, _req: &Request, _ino: INodeNo, _fh: FuserFileHandle, _datasync: bool, reply: ReplyEmpty) {
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        reply.statfs(0, 0, 0, 0, 0, 512, 255, 0);
    }

    fn access(&self, _req: &Request, ino: INodeNo, _mask: fuser::AccessFlags, reply: ReplyEmpty) {
        let state = self.state.lock().unwrap();
        if state.ino_exists(ino.0) {
            reply.ok();
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn getxattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, _size: u32, reply: ReplyXattr) {
        reply.error(Errno::NO_XATTR);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, _size: u32, reply: ReplyXattr) {
        reply.size(0);
    }

    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::ENOSYS);
    }

    fn destroy(&mut self) {
        let mut state = self.state.lock().unwrap();
        let fhs: Vec<u64> = state.handles.keys().copied().collect();
        for fh in fhs {
            commit_handle_if_dirty(&mut state, fh);
        }
    }
}

fn commit_handle_if_dirty(state: &mut MountState, fh: u64) {
    let (file_idx, front_matter_kind, path, text) = match state.handles.get(&fh) {
        Some(Handle::Content { file, path, dirty: true, buf }) => (
            Some(*file),
            None,
            Some(path.clone()),
            Some(String::from_utf8_lossy(buf).into_owned()),
        ),
        Some(Handle::FrontMatter { file, kind, dirty: true, buf }) => {
            (Some(*file), Some(*kind), None, Some(String::from_utf8_lossy(buf).into_owned()))
        }
        _ => (None, None, None, None),
    };
    let (Some(file_idx), Some(text)) = (file_idx, text) else { return };
    let result = match (path, front_matter_kind) {
        (Some(p), _) => state.commit_content(file_idx, &p, &text),
        (None, Some(kind)) => state.commit_frontmatter(file_idx, kind, &text),
        _ => Ok(()),
    };
    if result.is_ok()
        && let Some(handle) = state.handles.get_mut(&fh)
    {
        match handle {
            Handle::Content { dirty, .. } | Handle::FrontMatter { dirty, .. } => *dirty = false,
            _ => {}
        }
    }
}
