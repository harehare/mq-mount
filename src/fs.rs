//! [`crate::vfs::MountFs`] glue: translates [`crate::document`] mutations
//! into the OS/protocol-agnostic virtual filesystem contract that mount
//! backends (NFSv3 on Unix, WinFsp on Windows) adapt to their own protocol.
//! State lives behind a `Mutex` since every trait method takes `&self`.
//!
//! Each mounted file gets its own top-level directory (named after the file,
//! deduplicated like sibling headings) under a synthetic super-root at
//! `ROOT_INO`; a mounted directory contributes an intermediate tree of
//! synthetic directories mirroring its own layout. This filesystem has no
//! open/release; writes to `content.md` / frontmatter files are committed
//! straight into the document on every `write()` call, and files created
//! under a name we don't recognize (an editor's atomic-save temp file) are
//! buffered in `scratch` until a `rename()` lands them on a canonical name.

use std::fs;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::document::{self, Document, FrontMatterKind, MutationError, Section};
use crate::inode::{InodeTable, MountPath, ROOT_INO};
use crate::vfs::{DirEntryOwned, FileAttr, FileKind, Ino, MountFs, VfsError};

struct FileMount {
    document: Document,
    tree: document::SectionTree,
    inodes: InodeTable,
    source_path: PathBuf,
    last_persisted: String,
    last_known_mtime: Option<SystemTime>,
}

impl FileMount {
    fn open(source_path: PathBuf, next_ino: &mut Ino) -> std::io::Result<Self> {
        let text = fs::read_to_string(&source_path)?;
        let mtime = fs::metadata(&source_path).ok().and_then(|m| m.modified().ok());
        let document = Document::parse(&text).map_err(|e| std::io::Error::other(e.to_string()))?;
        let tree = document.tree();
        let mut inodes = InodeTable::new();
        inodes.sync(next_ino, &tree);
        Ok(Self {
            document,
            tree,
            inodes,
            source_path,
            last_persisted: text,
            last_known_mtime: mtime,
        })
    }
}

struct SuperDir {
    parent: Ino,
    children: Vec<(String, Ino)>,
    /// Collision-suffix counters for this level's raw names, carried forward
    /// so a later incremental insert (see [`MountState::add_file`]) stays
    /// consistent with siblings created at mount time.
    seen_names: FxHashMap<String, u32>,
}

struct MountState {
    files: Vec<FileMount>,
    super_dirs: FxHashMap<Ino, SuperDir>,
    file_root_parent: FxHashMap<usize, Ino>,
    ino_owner: FxHashMap<Ino, usize>,
    next_ino: Ino,
    scratch: FxHashMap<Ino, Vec<u8>>,
    scratch_by_name: FxHashMap<(Ino, String), Ino>,
    /// Canonicalized source paths already mounted, so a watcher re-notified
    /// about the same file (or a file it opened before mount) is a no-op.
    mounted_paths: FxHashSet<PathBuf>,
    readonly: bool,
    allow_other: bool,
    uid: u32,
    gid: u32,
}

impl MountState {
    fn open(entries: Vec<(PathBuf, Vec<String>)>, readonly: bool, allow_other: bool) -> std::io::Result<Self> {
        let mut next_ino = ROOT_INO + 1;
        let mut files = Vec::with_capacity(entries.len());
        let mut mount_paths = Vec::with_capacity(entries.len());
        let mut mounted_paths = FxHashSet::default();
        for (source_path, mount_path) in entries {
            let file = FileMount::open(source_path.clone(), &mut next_ino)?;
            mounted_paths.insert(source_path);
            mount_paths.push(mount_path);
            files.push(file);
        }

        let mut ino_owner = FxHashMap::default();
        for (idx, f) in files.iter().enumerate() {
            for (ino, _) in f.inodes.entries() {
                ino_owner.insert(ino, idx);
            }
        }

        let mut state = Self {
            files,
            super_dirs: FxHashMap::default(),
            file_root_parent: FxHashMap::default(),
            ino_owner,
            next_ino,
            scratch: FxHashMap::default(),
            scratch_by_name: FxHashMap::default(),
            mounted_paths,
            readonly,
            allow_other,
            uid: 0,
            gid: 0,
        };
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        state.uid = uid;
        state.gid = gid;

        let leaves: Vec<(usize, Vec<String>)> = mount_paths.into_iter().enumerate().collect();
        let (root_children, root_seen) = state.build_super_level(ROOT_INO, leaves);
        state.super_dirs.insert(
            ROOT_INO,
            SuperDir {
                parent: ROOT_INO,
                children: root_children,
                seen_names: root_seen,
            },
        );

        Ok(state)
    }

    /// Adds a single newly-discovered file to an already-running mount,
    /// creating any missing intermediate super-directories along
    /// `mount_path` on demand. Used by the `--watch` file watcher; a no-op
    /// if `source_path` is already mounted.
    fn add_file(&mut self, source_path: PathBuf, mount_path: Vec<String>) -> std::io::Result<()> {
        if mount_path.is_empty() || self.mounted_paths.contains(&source_path) {
            return Ok(());
        }

        let file = FileMount::open(source_path.clone(), &mut self.next_ino)?;
        let file_idx = self.files.len();
        let entry_inos: Vec<Ino> = file.inodes.entries().map(|(ino, _)| ino).collect();
        let root_ino = file.inodes.root_dir_ino();
        self.files.push(file);
        for ino in entry_inos {
            self.ino_owner.insert(ino, file_idx);
        }
        self.mounted_paths.insert(source_path);

        let mut parent_ino = ROOT_INO;
        for component in &mount_path[..mount_path.len() - 1] {
            parent_ino = self.get_or_create_super_dir(parent_ino, component);
        }
        let super_dir = self
            .super_dirs
            .get_mut(&parent_ino)
            .expect("parent super dir must exist");
        let name = document::unique_name(&mut super_dir.seen_names, mount_path.last().unwrap());
        super_dir.children.push((name, root_ino));
        self.file_root_parent.insert(file_idx, parent_ino);
        Ok(())
    }

    /// Finds an existing subdirectory named `raw_name` directly under
    /// `parent_ino`, or creates one. Matches by exact (unsuffixed) name,
    /// which holds as long as `raw_name` didn't collide with a sibling at
    /// mount time — the same rare edge case [`Self::build_super_level`]
    /// resolves with a `-2`-style suffix.
    fn get_or_create_super_dir(&mut self, parent_ino: Ino, raw_name: &str) -> Ino {
        let existing = self.super_dirs.get(&parent_ino).and_then(|sd| {
            sd.children
                .iter()
                .find(|(name, _)| name == raw_name)
                .map(|&(_, ino)| ino)
        });
        if let Some(ino) = existing
            && self.super_dirs.contains_key(&ino)
        {
            return ino;
        }

        let ino = self.alloc_ino();
        self.super_dirs.insert(
            ino,
            SuperDir {
                parent: parent_ino,
                children: Vec::new(),
                seen_names: FxHashMap::default(),
            },
        );
        let parent = self
            .super_dirs
            .get_mut(&parent_ino)
            .expect("parent super dir must exist");
        let name = document::unique_name(&mut parent.seen_names, raw_name);
        parent.children.push((name, ino));
        ino
    }

    fn build_super_level(
        &mut self,
        parent_ino: Ino,
        entries: Vec<(usize, Vec<String>)>,
    ) -> (Vec<(String, Ino)>, FxHashMap<String, u32>) {
        let mut order: Vec<String> = Vec::new();
        let mut buckets: FxHashMap<String, Vec<(usize, Vec<String>)>> = FxHashMap::default();
        for entry in entries {
            let key = entry.1[0].clone();
            if !buckets.contains_key(&key) {
                order.push(key.clone());
            }
            buckets.entry(key).or_default().push(entry);
        }

        let mut seen_names: FxHashMap<String, u32> = FxHashMap::default();
        let mut children = Vec::new();
        for raw_name in order {
            let group = buckets.remove(&raw_name).unwrap();
            let mut subdirs = Vec::new();
            let mut leaves = Vec::new();
            for (idx, mut path) in group {
                if path.len() > 1 {
                    path.remove(0);
                    subdirs.push((idx, path));
                } else {
                    leaves.push(idx);
                }
            }

            if !subdirs.is_empty() {
                let name = document::unique_name(&mut seen_names, &raw_name);
                let ino = self.alloc_ino();
                let (grandchildren, grandchildren_seen) = self.build_super_level(ino, subdirs);
                self.super_dirs.insert(
                    ino,
                    SuperDir {
                        parent: parent_ino,
                        children: grandchildren,
                        seen_names: grandchildren_seen,
                    },
                );
                children.push((name, ino));
            }
            for file_idx in leaves {
                let name = document::unique_name(&mut seen_names, &raw_name);
                let root_ino = self.files[file_idx].inodes.root_dir_ino();
                self.file_root_parent.insert(file_idx, parent_ino);
                children.push((name, root_ino));
            }
        }

        (children, seen_names)
    }

    fn rebuild(&mut self, file_idx: usize) {
        let MountState {
            files,
            next_ino,
            ino_owner,
            ..
        } = self;
        let file = &mut files[file_idx];
        let old_inos: Vec<Ino> = file.inodes.entries().map(|(ino, _)| ino).collect();
        file.tree = file.document.tree();
        file.inodes.sync(next_ino, &file.tree);
        for ino in old_inos {
            ino_owner.remove(&ino);
        }
        for (ino, _) in file.inodes.entries() {
            ino_owner.insert(ino, file_idx);
        }
    }

    /// Refuses (`VfsError::Conflict`) rather than silently overwriting a source file changed on disk since it was last read.
    fn persist(&mut self, file_idx: usize) -> Result<(), VfsError> {
        let file = &mut self.files[file_idx];
        let rendered = file.document.render();
        if rendered == file.last_persisted {
            return Ok(());
        }
        let current_mtime = fs::metadata(&file.source_path).ok().and_then(|m| m.modified().ok());
        if let (Some(known), Some(current)) = (file.last_known_mtime, current_mtime)
            && current > known
        {
            tracing::warn!(
                "{} changed on disk outside the mount since it was last read; refusing to overwrite it",
                file.source_path.display()
            );
            return Err(VfsError::Conflict);
        }
        fs::write(&file.source_path, &rendered).map_err(|_| VfsError::Io)?;
        file.last_persisted = rendered;
        file.last_known_mtime = fs::metadata(&file.source_path).ok().and_then(|m| m.modified().ok());
        Ok(())
    }

    fn rebuild_and_persist(&mut self, file_idx: usize) -> Result<(), VfsError> {
        self.rebuild(file_idx);
        self.persist(file_idx)
    }

    fn alloc_ino(&mut self) -> Ino {
        let ino = self.next_ino;
        self.next_ino += 1;
        ino
    }

    fn dir_path(&self, ino: Ino) -> Option<(usize, Vec<String>)> {
        let &file_idx = self.ino_owner.get(&ino)?;
        match self.files[file_idx].inodes.path_for(ino)? {
            MountPath::Dir(p) => Some((file_idx, p.clone())),
            _ => None,
        }
    }

    fn path_for_ino(&self, ino: Ino) -> Option<(usize, MountPath)> {
        let &file_idx = self.ino_owner.get(&ino)?;
        let path = self.files[file_idx].inodes.path_for(ino)?.clone();
        Some((file_idx, path))
    }

    fn find_section(&self, file_idx: usize, path: &[String]) -> Option<Section> {
        let refs: Vec<&str> = path.iter().map(String::as_str).collect();
        self.files[file_idx].tree.find(&refs).cloned()
    }

    fn find_child_section(&self, file_idx: usize, parent: &[String], name: &str) -> Option<Section> {
        self.find_section(file_idx, parent)
            .and_then(|p| p.children.into_iter().find(|c| c.name == name))
    }

    fn section_range(&self, file_idx: usize, path: &[String]) -> Option<Range<usize>> {
        let refs: Vec<&str> = path.iter().map(String::as_str).collect();
        self.files[file_idx]
            .tree
            .find(&refs)
            .map(|s| s.own_content_range.clone())
    }

    fn section_bytes(&self, file_idx: usize, path: &[String]) -> Vec<u8> {
        self.section_range(file_idx, path)
            .map(|r| self.files[file_idx].document.render_range(r).into_bytes())
            .unwrap_or_default()
    }

    fn frontmatter_bytes(&self, file_idx: usize, kind: FrontMatterKind) -> Vec<u8> {
        self.files[file_idx]
            .tree
            .front_matter
            .filter(|(k, _)| *k == kind)
            .map(|(_, idx)| self.files[file_idx].document.frontmatter_value(idx).as_bytes().to_vec())
            .unwrap_or_default()
    }

    fn commit_content(&mut self, file_idx: usize, dir_path: &[String], text: &str) -> Result<(), VfsError> {
        let Some(range) = self.section_range(file_idx, dir_path) else {
            return Ok(());
        };
        self.files[file_idx]
            .document
            .replace_section_content(range, text)
            .map_err(map_mutation_error)?;
        self.rebuild_and_persist(file_idx)
    }

    fn commit_frontmatter(&mut self, file_idx: usize, kind: FrontMatterKind, text: &str) -> Result<(), VfsError> {
        if let Some((found_kind, idx)) = self.files[file_idx].tree.front_matter
            && found_kind == kind
        {
            self.files[file_idx].document.set_frontmatter(idx, text.to_string());
            self.rebuild_and_persist(file_idx)?;
        }
        Ok(())
    }

    fn canonical_child(parent_path: &[String], name: &str) -> Option<MountPath> {
        if name == document::CONTENT_FILE {
            Some(MountPath::Content(parent_path.to_vec()))
        } else if parent_path.is_empty() {
            frontmatter_kind_for_name(name).map(MountPath::FrontMatter)
        } else {
            None
        }
    }

    fn has_heading_child(&self, file_idx: usize, parent_path: &[String], name: &str) -> bool {
        self.find_section(file_idx, parent_path)
            .is_some_and(|s| s.children.iter().any(|c| c.name == name))
    }

    fn alloc_scratch(&mut self, key: (Ino, String)) -> Ino {
        let ino = self.alloc_ino();
        self.scratch.insert(ino, Vec::new());
        self.scratch_by_name.insert(key, ino);
        ino
    }

    fn bytes_for(&self, file_idx: usize, path: &MountPath) -> Result<Vec<u8>, VfsError> {
        match path {
            MountPath::Content(p) => Ok(self.section_bytes(file_idx, p)),
            MountPath::FrontMatter(kind) => Ok(self.frontmatter_bytes(file_idx, *kind)),
            MountPath::Dir(_) => Err(VfsError::IsDir),
        }
    }

    fn commit(&mut self, file_idx: usize, path: &MountPath, text: &str) -> Result<(), VfsError> {
        match path {
            MountPath::Content(p) => self.commit_content(file_idx, p, text),
            MountPath::FrontMatter(kind) => self.commit_frontmatter(file_idx, *kind, text),
            MountPath::Dir(_) => Err(VfsError::IsDir),
        }
    }

    fn attr(&self, ino: Ino, kind: FileKind, size: u64) -> FileAttr {
        self.attr_at(ino, kind, size, SystemTime::now())
    }

    fn attr_at(&self, ino: Ino, kind: FileKind, size: u64, mtime: SystemTime) -> FileAttr {
        FileAttr { ino, kind, size, mtime }
    }

    fn attr_for(&self, file_idx: usize, ino: Ino, path: &MountPath) -> FileAttr {
        match path {
            MountPath::Dir(_) => self.attr(ino, FileKind::Dir, 0),
            MountPath::Content(p) => self.attr(ino, FileKind::File, self.section_bytes(file_idx, p).len() as u64),
            MountPath::FrontMatter(kind) => self.attr(
                ino,
                FileKind::File,
                self.frontmatter_bytes(file_idx, *kind).len() as u64,
            ),
        }
    }
}

fn map_mutation_error(e: MutationError) -> VfsError {
    match e {
        MutationError::AlreadyExists(_) => VfsError::Exists,
        MutationError::NotEmpty => VfsError::NotEmpty,
        MutationError::NotADirectory => VfsError::NotDir,
        MutationError::Parse(_) => VfsError::Invalid,
        MutationError::WouldCreateCycle | MutationError::TooDeep => VfsError::Invalid,
    }
}

/// What a `rename()` destination name canonically resolves to.
enum RenameDest {
    Content,
    FrontMatter(FrontMatterKind),
}

fn frontmatter_kind_for_name(name: &str) -> Option<FrontMatterKind> {
    [FrontMatterKind::Yaml, FrontMatterKind::Toml]
        .into_iter()
        .find(|k| k.file_name() == name)
}

fn to_text_lossy(buf: Vec<u8>) -> String {
    String::from_utf8(buf).unwrap_or_else(|e| String::from_utf8_lossy(&e.into_bytes()).into_owned())
}

fn splice(buf: &mut Vec<u8>, offset: usize, data: &[u8]) {
    if offset + data.len() > buf.len() {
        buf.resize(offset + data.len(), 0);
    }
    buf[offset..offset + data.len()].copy_from_slice(data);
}

fn paginate(entries: Vec<DirEntryOwned>, start_after: Ino, max_entries: usize) -> (Vec<DirEntryOwned>, bool) {
    let start_index = if start_after == 0 {
        0
    } else {
        match entries.iter().position(|e| e.ino == start_after) {
            Some(i) => i + 1,
            None => entries.len(),
        }
    };
    let max_entries = max_entries.max(1);
    let end = entries.len().saturating_sub(start_index) <= max_entries;
    let page = entries.into_iter().skip(start_index).take(max_entries).collect();
    (page, end)
}

pub struct MqFs {
    state: Mutex<MountState>,
}

impl MqFs {
    pub fn new(entries: Vec<(PathBuf, Vec<String>)>, readonly: bool, allow_other: bool) -> std::io::Result<Self> {
        Ok(Self {
            state: Mutex::new(MountState::open(entries, readonly, allow_other)?),
        })
    }

    /// Adds a file discovered after mount time (used by `--watch`); logs and
    /// swallows errors rather than propagating, since a transient read
    /// failure (e.g. an editor still writing the file) should just be
    /// retried on the next filesystem event, not tear down the mount.
    pub fn watch_add(&self, source_path: PathBuf, mount_path: Vec<String>) {
        let mut state = self.state.lock().unwrap();
        match state.add_file(source_path.clone(), mount_path) {
            Ok(()) => tracing::info!("auto-mounted {}", source_path.display()),
            Err(e) => tracing::warn!("failed to auto-mount {}: {e}", source_path.display()),
        }
    }
}

impl MountFs for MqFs {
    fn root_ino(&self) -> Ino {
        ROOT_INO
    }

    fn readonly(&self) -> bool {
        self.state.lock().unwrap().readonly
    }

    fn allow_other(&self) -> bool {
        self.state.lock().unwrap().allow_other
    }

    fn uid(&self) -> u32 {
        self.state.lock().unwrap().uid
    }

    fn gid(&self) -> u32 {
        self.state.lock().unwrap().gid
    }

    fn lookup(&self, parent: Ino, name: &str) -> Result<Ino, VfsError> {
        let state = self.state.lock().unwrap();
        tracing::debug!("lookup(parent={parent}, name={name:?})");

        if let Some(super_dir) = state.super_dirs.get(&parent) {
            return super_dir
                .children
                .iter()
                .find(|(child_name, _)| child_name == name)
                .map(|&(_, ino)| ino)
                .ok_or(VfsError::NotFound);
        }

        let (file_idx, parent_path) = state.dir_path(parent).ok_or(VfsError::NotDir)?;
        let child_path = if let Some(mp) = MountState::canonical_child(&parent_path, name) {
            mp
        } else if state.has_heading_child(file_idx, &parent_path, name) {
            let mut p = parent_path;
            p.push(name.to_string());
            MountPath::Dir(p)
        } else {
            return Err(VfsError::NotFound);
        };

        state.files[file_idx]
            .inodes
            .ino_for(&child_path)
            .ok_or(VfsError::NotFound)
    }

    fn parent_of(&self, ino: Ino) -> Result<Ino, VfsError> {
        let state = self.state.lock().unwrap();
        if let Some(super_dir) = state.super_dirs.get(&ino) {
            return Ok(super_dir.parent);
        }
        let (file_idx, path) = state.dir_path(ino).ok_or(VfsError::NotDir)?;
        Ok(if path.is_empty() {
            state.file_root_parent.get(&file_idx).copied().unwrap_or(ROOT_INO)
        } else {
            state.files[file_idx]
                .inodes
                .ino_for(&MountPath::Dir(path[..path.len() - 1].to_vec()))
                .unwrap_or(ROOT_INO)
        })
    }

    fn getattr(&self, ino: Ino) -> Result<FileAttr, VfsError> {
        let state = self.state.lock().unwrap();
        if let Some(buf) = state.scratch.get(&ino) {
            tracing::debug!("getattr({ino}): scratch, {} bytes", buf.len());
            return Ok(state.attr(ino, FileKind::File, buf.len() as u64));
        }
        if state.super_dirs.contains_key(&ino) {
            return Ok(state.attr(ino, FileKind::Dir, 0));
        }
        let (file_idx, path) = state.path_for_ino(ino).ok_or_else(|| {
            tracing::debug!("getattr({ino}): no path for this fileid");
            VfsError::NotFound
        })?;
        let attr = state.attr_for(file_idx, ino, &path);
        tracing::debug!("getattr({ino}): path={path:?} size={}", attr.size);
        Ok(attr)
    }

    fn truncate(&self, ino: Ino) -> Result<FileAttr, VfsError> {
        let mut state = self.state.lock().unwrap();
        if state.readonly {
            return Err(VfsError::ReadOnly);
        }
        tracing::debug!("truncate({ino})");
        if let Some(buf) = state.scratch.get_mut(&ino) {
            buf.clear();
        } else if let Some((file_idx, path)) = state.path_for_ino(ino) {
            state.commit(file_idx, &path, "")?;
        }

        if let Some(buf) = state.scratch.get(&ino) {
            return Ok(state.attr(ino, FileKind::File, buf.len() as u64));
        }
        if state.super_dirs.contains_key(&ino) {
            return Ok(state.attr(ino, FileKind::Dir, 0));
        }
        let (file_idx, path) = state.path_for_ino(ino).ok_or(VfsError::NotFound)?;
        Ok(state.attr_for(file_idx, ino, &path))
    }

    fn read(&self, ino: Ino, offset: u64, count: u32) -> Result<(Vec<u8>, bool), VfsError> {
        let state = self.state.lock().unwrap();
        let bytes: std::borrow::Cow<[u8]> = if let Some(buf) = state.scratch.get(&ino) {
            std::borrow::Cow::Borrowed(buf)
        } else {
            match state.path_for_ino(ino) {
                Some((file_idx, path)) => std::borrow::Cow::Owned(state.bytes_for(file_idx, &path)?),
                None => {
                    tracing::debug!("read({ino}): no path for this fileid");
                    return Err(VfsError::NotFound);
                }
            }
        };
        tracing::debug!(
            "read({ino}, offset={offset}, count={count}): {} bytes available",
            bytes.len()
        );
        let offset = offset as usize;
        if offset >= bytes.len() {
            return Ok((Vec::new(), true));
        }
        let end = (offset + count as usize).min(bytes.len());
        Ok((bytes[offset..end].to_vec(), end >= bytes.len()))
    }

    fn write(&self, ino: Ino, offset: u64, data: &[u8]) -> Result<FileAttr, VfsError> {
        let mut state = self.state.lock().unwrap();
        if state.readonly {
            return Err(VfsError::ReadOnly);
        }
        let offset = offset as usize;

        if let Some(buf) = state.scratch.get_mut(&ino) {
            splice(buf, offset, data);
            let len = buf.len() as u64;
            return Ok(state.attr(ino, FileKind::File, len));
        }

        let (file_idx, path) = state.path_for_ino(ino).ok_or(VfsError::NotFound)?;
        let mut buf = state.bytes_for(file_idx, &path)?;
        splice(&mut buf, offset, data);
        state.commit(file_idx, &path, &to_text_lossy(buf))?;
        Ok(state.attr_for(file_idx, ino, &path))
    }

    fn create(&self, parent: Ino, name: &str) -> Result<(Ino, FileAttr), VfsError> {
        let mut state = self.state.lock().unwrap();
        if state.readonly {
            return Err(VfsError::ReadOnly);
        }
        let (file_idx, parent_path) = state.dir_path(parent).ok_or(VfsError::NotFound)?;

        if let Some(mp) = MountState::canonical_child(&parent_path, name) {
            let ino = state.files[file_idx].inodes.ino_for(&mp).ok_or(VfsError::Io)?;
            return Ok((ino, state.attr_for(file_idx, ino, &mp)));
        }

        let key = (parent, name.to_string());
        if let Some(&ino) = state.scratch_by_name.get(&key) {
            let len = state.scratch.get(&ino).map(Vec::len).unwrap_or(0) as u64;
            return Ok((ino, state.attr(ino, FileKind::File, len)));
        }
        let ino = state.alloc_scratch(key);
        Ok((ino, state.attr(ino, FileKind::File, 0)))
    }

    fn create_exclusive(&self, parent: Ino, name: &str) -> Result<Ino, VfsError> {
        let mut state = self.state.lock().unwrap();
        if state.readonly {
            return Err(VfsError::ReadOnly);
        }

        let exists = if let Some(super_dir) = state.super_dirs.get(&parent) {
            super_dir.children.iter().any(|(child_name, _)| child_name == name)
        } else {
            let (file_idx, parent_path) = state.dir_path(parent).ok_or(VfsError::NotFound)?;
            MountState::canonical_child(&parent_path, name).is_some()
                || state.has_heading_child(file_idx, &parent_path, name)
                || state.scratch_by_name.contains_key(&(parent, name.to_string()))
        };
        if exists {
            return Err(VfsError::Exists);
        }

        Ok(state.alloc_scratch((parent, name.to_string())))
    }

    fn mkdir(&self, parent: Ino, name: &str) -> Result<(Ino, FileAttr), VfsError> {
        let mut state = self.state.lock().unwrap();
        if state.readonly {
            return Err(VfsError::ReadOnly);
        }
        if state.super_dirs.contains_key(&parent) {
            return Err(VfsError::PermissionDenied);
        }
        let (file_idx, parent_path) = state.dir_path(parent).ok_or(VfsError::NotFound)?;
        let parent_section = state.find_section(file_idx, &parent_path).ok_or(VfsError::NotFound)?;

        state.files[file_idx]
            .document
            .insert_heading(&parent_section, name)
            .map_err(map_mutation_error)?;
        state.rebuild_and_persist(file_idx)?;

        let mut child_path = parent_path;
        child_path.push(name.to_string());
        let mp = MountPath::Dir(child_path);
        let ino = state.files[file_idx].inodes.ino_for(&mp).ok_or(VfsError::Io)?;
        Ok((ino, state.attr_for(file_idx, ino, &mp)))
    }

    fn remove(&self, parent: Ino, name: &str) -> Result<(), VfsError> {
        let mut state = self.state.lock().unwrap();
        if state.readonly {
            return Err(VfsError::ReadOnly);
        }

        let key = (parent, name.to_string());
        if let Some(ino) = state.scratch_by_name.remove(&key) {
            state.scratch.remove(&ino);
            return Ok(());
        }
        if state.super_dirs.contains_key(&parent) {
            return Err(VfsError::PermissionDenied);
        }
        let (file_idx, parent_path) = state.dir_path(parent).ok_or(VfsError::NotFound)?;

        if name == document::CONTENT_FILE {
            return state.commit_content(file_idx, &parent_path, "");
        }
        if parent_path.is_empty()
            && let Some(kind) = frontmatter_kind_for_name(name)
        {
            return state.commit_frontmatter(file_idx, kind, "");
        }
        let section = state
            .find_child_section(file_idx, &parent_path, name)
            .ok_or(VfsError::NotFound)?;
        state.files[file_idx]
            .document
            .remove_heading(&section)
            .map_err(map_mutation_error)?;
        state.rebuild_and_persist(file_idx)
    }

    fn rename(&self, from_parent: Ino, from_name: &str, to_parent: Ino, to_name: &str) -> Result<(), VfsError> {
        let mut state = self.state.lock().unwrap();
        if state.readonly {
            return Err(VfsError::ReadOnly);
        }

        let (dest_file_idx, new_parent_path) = state.dir_path(to_parent).ok_or(VfsError::NotFound)?;

        let dest_kind = match MountState::canonical_child(&new_parent_path, to_name) {
            Some(MountPath::Content(_)) => Some(RenameDest::Content),
            Some(MountPath::FrontMatter(kind)) => Some(RenameDest::FrontMatter(kind)),
            _ => None,
        };

        if let Some(dest_kind) = dest_kind {
            let key = (from_parent, from_name.to_string());
            let (bytes, clear_source_path) = if let Some(ino) = state.scratch_by_name.get(&key).copied() {
                (state.scratch.get(&ino).cloned(), None)
            } else if from_name == document::CONTENT_FILE {
                match state.dir_path(from_parent) {
                    Some((src_file_idx, _)) if src_file_idx != dest_file_idx => {
                        return Err(VfsError::Unsupported);
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

            let bytes = bytes.ok_or(VfsError::NotFound)?;
            let text = to_text_lossy(bytes);

            match dest_kind {
                RenameDest::Content => state.commit_content(dest_file_idx, &new_parent_path, &text)?,
                RenameDest::FrontMatter(kind) => state.commit_frontmatter(dest_file_idx, kind, &text)?,
            }
            if let Some(p) = clear_source_path {
                state.commit_content(dest_file_idx, &p, "")?;
            }
            if let Some(ino) = state.scratch_by_name.remove(&key) {
                state.scratch.remove(&ino);
            }
            return Ok(());
        }

        let src_key = (from_parent, from_name.to_string());
        if let Some(ino) = state.scratch_by_name.remove(&src_key) {
            state.scratch_by_name.insert((to_parent, to_name.to_string()), ino);
            return Ok(());
        }

        if from_parent != to_parent {
            let (src_file_idx, src_parent_path) = state.dir_path(from_parent).ok_or(VfsError::NotFound)?;
            let (dest_file_idx, dest_parent_path) = state.dir_path(to_parent).ok_or(VfsError::NotFound)?;
            if src_file_idx != dest_file_idx {
                // Would mean splicing content between two separate `Document`s.
                return Err(VfsError::Unsupported);
            }
            let section = state
                .find_child_section(src_file_idx, &src_parent_path, from_name)
                .ok_or(VfsError::NotFound)?;
            let new_parent = state
                .find_section(dest_file_idx, &dest_parent_path)
                .ok_or(VfsError::NotFound)?;
            state.files[src_file_idx]
                .document
                .move_heading(&section, &new_parent, to_name)
                .map_err(map_mutation_error)?;
            return state.rebuild_and_persist(src_file_idx);
        }
        let (file_idx, parent_path) = state.dir_path(from_parent).ok_or(VfsError::NotFound)?;
        let section = state
            .find_child_section(file_idx, &parent_path, from_name)
            .ok_or(VfsError::NotFound)?;
        state.files[file_idx]
            .document
            .rename_heading(&section, to_name)
            .map_err(map_mutation_error)?;
        state.rebuild_and_persist(file_idx)
    }

    fn readdir(&self, ino: Ino, start_after: Ino, max_entries: usize) -> Result<(Vec<DirEntryOwned>, bool), VfsError> {
        let state = self.state.lock().unwrap();
        let mut entries: Vec<DirEntryOwned> = Vec::new();
        let now = SystemTime::now();

        if let Some(super_dir) = state.super_dirs.get(&ino) {
            for (child_name, child_ino) in &super_dir.children {
                entries.push(DirEntryOwned {
                    ino: *child_ino,
                    name: child_name.clone(),
                    attr: state.attr_at(*child_ino, FileKind::Dir, 0, now),
                });
            }
            return Ok(paginate(entries, start_after, max_entries));
        }

        let (file_idx, path) = state.dir_path(ino).ok_or(VfsError::NotDir)?;
        let section = state.find_section(file_idx, &path).ok_or(VfsError::NotFound)?;

        let content_ino = state.files[file_idx]
            .inodes
            .ino_for(&MountPath::Content(path.clone()))
            .unwrap_or(ino);
        let content_size = state.section_bytes(file_idx, &path).len() as u64;

        entries.push(DirEntryOwned {
            ino: content_ino,
            name: document::CONTENT_FILE.to_string(),
            attr: state.attr_at(content_ino, FileKind::File, content_size, now),
        });

        if path.is_empty()
            && let Some((kind, _)) = state.files[file_idx].tree.front_matter
            && let Some(fm_ino) = state.files[file_idx].inodes.ino_for(&MountPath::FrontMatter(kind))
        {
            let size = state.frontmatter_bytes(file_idx, kind).len() as u64;
            entries.push(DirEntryOwned {
                ino: fm_ino,
                name: kind.file_name().to_string(),
                attr: state.attr_at(fm_ino, FileKind::File, size, now),
            });
        }

        for child in &section.children {
            let mut p = path.clone();
            p.push(child.name.clone());
            if let Some(child_ino) = state.files[file_idx].inodes.ino_for(&MountPath::Dir(p)) {
                entries.push(DirEntryOwned {
                    ino: child_ino,
                    name: child.name.clone(),
                    attr: state.attr_at(child_ino, FileKind::Dir, 0, now),
                });
            }
        }

        Ok(paginate(entries, start_after, max_entries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mounts `content` as a single file and returns the filesystem, the
    /// tempdir keeping its source path alive, and the fileid of that file's
    /// own top-level directory.
    fn mounted(content: &str) -> (MqFs, tempfile::TempDir, Ino) {
        mounted_with(content, false, false)
    }

    fn mounted_with(content: &str, readonly: bool, allow_other: bool) -> (MqFs, tempfile::TempDir, Ino) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.md");
        std::fs::write(&path, content).unwrap();
        let fs = MqFs::new(vec![(path.clone(), vec!["doc".to_string()])], readonly, allow_other).unwrap();
        let (entries, _) = fs.readdir(fs.root_ino(), 0, 100).unwrap();
        let file_ino = entries[0].ino;
        (fs, dir, file_ino)
    }

    fn lookup_path(fs: &MqFs, mut ino: Ino, path: &[&str]) -> Ino {
        for part in path {
            ino = fs.lookup(ino, part).unwrap();
        }
        ino
    }

    #[test]
    fn write_then_read_round_trips_through_content_md() {
        let (fs, dir, file_ino) = mounted("# Title\n\noriginal body\n");
        let content_ino = lookup_path(&fs, file_ino, &["Title", document::CONTENT_FILE]);

        // `write` only overwrites the byte range it's given, like a real
        // write(2) — a whole-file save (shell redirection, an editor) always
        // truncates first, same as O_TRUNC would.
        fs.truncate(content_ino).unwrap();
        fs.write(content_ino, 0, b"new body\n").unwrap();
        let (bytes, eof) = fs.read(content_ino, 0, 1024).unwrap();
        assert_eq!(bytes, b"new body\n");
        assert!(eof);

        let persisted = std::fs::read_to_string(dir.path().join("doc.md")).unwrap();
        assert!(persisted.contains("new body"), "persisted doc was: {persisted}");
    }

    #[test]
    fn saving_a_multi_paragraph_section_keeps_the_blank_lines_between_paragraphs() {
        let (fs, dir, file_ino) = mounted("# Title\n\npara one\n\npara two\n\npara three\n");
        let content_ino = lookup_path(&fs, file_ino, &["Title", document::CONTENT_FILE]);

        // Editor-style whole-file save: read the body back and write it unchanged.
        let (bytes, _) = fs.read(content_ino, 0, 4096).unwrap();
        fs.truncate(content_ino).unwrap();
        fs.write(content_ino, 0, &bytes).unwrap();

        let persisted = std::fs::read_to_string(dir.path().join("doc.md")).unwrap();
        assert_eq!(persisted, "# Title\n\npara one\n\npara two\n\npara three\n");
    }

    #[test]
    fn mkdir_adds_a_heading_visible_in_readdir_and_on_disk() {
        let (fs, dir, file_ino) = mounted("# Title\n\nbody\n");
        let title_ino = fs.lookup(file_ino, "Title").unwrap();

        let (sub_ino, attr) = fs.mkdir(title_ino, "Sub").unwrap();
        assert_eq!(attr.kind, FileKind::Dir);

        let (entries, _) = fs.readdir(title_ino, 0, 100).unwrap();
        assert!(entries.iter().any(|e| e.ino == sub_ino && e.name == "Sub"));

        let persisted = std::fs::read_to_string(dir.path().join("doc.md")).unwrap();
        assert!(persisted.contains("Sub"), "persisted doc was: {persisted}");
    }

    #[test]
    fn remove_rejects_nonempty_directory() {
        let (fs, _dir, file_ino) = mounted("# A\n\n## B\n\nbody\n");

        let err = fs.remove(file_ino, "A").unwrap_err();
        assert!(matches!(err, VfsError::NotEmpty));
    }

    #[test]
    fn remove_deletes_an_empty_heading() {
        let (fs, _dir, file_ino) = mounted("# A\n\n# B\n");
        fs.remove(file_ino, "A").unwrap();
        assert!(matches!(fs.lookup(file_ino, "A"), Err(VfsError::NotFound)));
        assert!(fs.lookup(file_ino, "B").is_ok());
    }

    #[test]
    fn rename_renames_a_heading_and_persists() {
        let (fs, dir, file_ino) = mounted("# Old\n\nbody\n");
        fs.rename(file_ino, "Old", file_ino, "New").unwrap();

        assert!(matches!(fs.lookup(file_ino, "Old"), Err(VfsError::NotFound)));
        assert!(fs.lookup(file_ino, "New").is_ok());
        let persisted = std::fs::read_to_string(dir.path().join("doc.md")).unwrap();
        assert!(persisted.contains("New"));
    }

    #[test]
    fn rename_reparents_a_heading_to_a_different_parent_and_persists() {
        let (fs, dir, file_ino) = mounted("# A\n\nintro\n\n## B\n\nb body\n\n# C\n\nc body\n");
        let a_ino = fs.lookup(file_ino, "A").unwrap();
        let c_ino = fs.lookup(file_ino, "C").unwrap();

        fs.rename(a_ino, "B", c_ino, "Moved").unwrap();

        assert!(matches!(fs.lookup(a_ino, "B"), Err(VfsError::NotFound)));
        let moved_ino = fs.lookup(c_ino, "Moved").unwrap();
        let content_ino = fs.lookup(moved_ino, document::CONTENT_FILE).unwrap();
        let (bytes, _) = fs.read(content_ino, 0, 1024).unwrap();
        assert_eq!(bytes, b"b body\n");

        let persisted = std::fs::read_to_string(dir.path().join("doc.md")).unwrap();
        assert_eq!(persisted, "# A\n\nintro\n\n# C\n\nc body\n\n## Moved\n\nb body\n");
    }

    #[test]
    fn rename_rejects_reparenting_across_different_mounted_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        std::fs::write(&a, "# A\n\n## Sub\n\nbody\n").unwrap();
        std::fs::write(&b, "# B\n\nbody\n").unwrap();
        let fs = MqFs::new(vec![(a, vec!["a".into()]), (b, vec!["b".into()])], false, false).unwrap();

        let a_root = fs.lookup(ROOT_INO, "a").unwrap();
        let a_heading = fs.lookup(a_root, "A").unwrap();
        let b_root = fs.lookup(ROOT_INO, "b").unwrap();

        let err = fs.rename(a_heading, "Sub", b_root, "Sub").unwrap_err();
        assert!(matches!(err, VfsError::Unsupported));
    }

    #[test]
    fn atomic_save_via_scratch_rename_commits_content() {
        let (fs, dir, file_ino) = mounted("# Title\n\noriginal\n");
        let title_ino = fs.lookup(file_ino, "Title").unwrap();

        let (tmp_ino, _attr) = fs.create(title_ino, ".tmp").unwrap();
        fs.write(tmp_ino, 0, b"saved via temp file\n").unwrap();
        fs.rename(title_ino, ".tmp", title_ino, document::CONTENT_FILE).unwrap();

        let content_ino = fs.lookup(title_ino, document::CONTENT_FILE).unwrap();
        let (bytes, _) = fs.read(content_ino, 0, 1024).unwrap();
        assert_eq!(bytes, b"saved via temp file\n");

        let persisted = std::fs::read_to_string(dir.path().join("doc.md")).unwrap();
        assert!(
            persisted.contains("saved via temp file"),
            "persisted doc was: {persisted}"
        );
    }

    #[test]
    fn readonly_mount_rejects_every_mutation() {
        let (fs, _dir, file_ino) = mounted_with("# Title\n\nbody\n", true, false);
        let title_ino = fs.lookup(file_ino, "Title").unwrap();
        let content_ino = fs.lookup(title_ino, document::CONTENT_FILE).unwrap();

        assert!(matches!(fs.write(content_ino, 0, b"x"), Err(VfsError::ReadOnly)));
        assert!(matches!(fs.create(title_ino, "x"), Err(VfsError::ReadOnly)));
        assert!(matches!(fs.mkdir(title_ino, "Sub"), Err(VfsError::ReadOnly)));
        assert!(matches!(fs.remove(title_ino, "content.md"), Err(VfsError::ReadOnly)));
        assert!(matches!(
            fs.rename(file_ino, "Title", file_ino, "Renamed"),
            Err(VfsError::ReadOnly)
        ));
    }

    #[test]
    fn truncate_clears_content() {
        let (fs, dir, file_ino) = mounted("# Title\n\nbody\n");
        let content_ino = lookup_path(&fs, file_ino, &["Title", document::CONTENT_FILE]);

        fs.truncate(content_ino).unwrap();

        let (bytes, eof) = fs.read(content_ino, 0, 1024).unwrap();
        assert!(bytes.is_empty());
        assert!(eof);
        let persisted = std::fs::read_to_string(dir.path().join("doc.md")).unwrap();
        assert!(!persisted.contains("body"), "persisted doc was: {persisted}");
    }

    #[test]
    fn write_refuses_to_overwrite_a_file_changed_externally_since_mount() {
        let (fs, dir, file_ino) = mounted("# Title\n\noriginal\n");
        let content_ino = lookup_path(&fs, file_ino, &["Title", document::CONTENT_FILE]);
        let path = dir.path().join("doc.md");

        // Simulate an external edit, mtime pushed well past mount time.
        std::fs::write(&path, "# Title\n\nexternal edit\n").unwrap();
        let future = SystemTime::now() + std::time::Duration::from_secs(3600);
        std::fs::File::open(&path).unwrap().set_modified(future).unwrap();

        let err = fs.truncate(content_ino).unwrap_err();
        assert!(matches!(err, VfsError::Conflict));

        let persisted = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            persisted, "# Title\n\nexternal edit\n",
            "external edit must survive untouched, not be overwritten"
        );
    }

    #[test]
    fn create_exclusive_rejects_an_existing_canonical_name() {
        let (fs, _dir, file_ino) = mounted("# Title\n\nbody\n");
        let err = fs.create_exclusive(file_ino, document::CONTENT_FILE).unwrap_err();
        assert!(matches!(err, VfsError::Exists));
    }

    #[test]
    fn root_directory_rejects_structural_mutations() {
        let (fs, _dir, _file_ino) = mounted("# Title\n\nbody\n");
        assert!(matches!(fs.mkdir(ROOT_INO, "x"), Err(VfsError::PermissionDenied)));
        assert!(matches!(fs.remove(ROOT_INO, "x"), Err(VfsError::PermissionDenied)));
    }

    #[test]
    fn readdir_pagination_visits_every_entry_exactly_once_with_max_entries_one() {
        let (fs, _dir, file_ino) = mounted("# A\n\n# B\n\n# C\n\n# D\n");

        let mut seen = Vec::new();
        let mut cookie: Ino = 0;
        loop {
            let (page_entries, end) = fs.readdir(file_ino, cookie, 1).unwrap();
            assert!(page_entries.len() <= 1);
            for e in &page_entries {
                seen.push(e.ino);
                cookie = e.ino;
            }
            if end {
                break;
            }
            if seen.len() > 20 {
                panic!("readdir pagination did not converge: {seen:?}");
            }
        }

        let mut unique = seen.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(seen.len(), unique.len(), "pagination revisited an entry: {seen:?}");
        // content.md + A, B, C, D
        assert_eq!(seen.len(), 5);
    }

    #[test]
    fn nested_mount_paths_create_intermediate_directories() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        std::fs::write(&a, "# A\n\nbody\n").unwrap();
        std::fs::write(&b, "# B\n\nbody\n").unwrap();

        let fs = MqFs::new(
            vec![
                (a, vec!["docs".into(), "guide".into(), "a".into()]),
                (b, vec!["docs".into(), "api".into(), "b".into()]),
            ],
            false,
            false,
        )
        .unwrap();

        let docs_ino = fs.lookup(ROOT_INO, "docs").unwrap();
        let guide_ino = fs.lookup(docs_ino, "guide").unwrap();
        let api_ino = fs.lookup(docs_ino, "api").unwrap();
        let a_ino = fs.lookup(guide_ino, "a").unwrap();
        let b_ino = fs.lookup(api_ino, "b").unwrap();

        // both leaves resolve down to their own heading trees
        assert!(fs.lookup(a_ino, "A").is_ok());
        assert!(fs.lookup(b_ino, "B").is_ok());

        // .. climbs back up through the synthetic directories to the root
        assert_eq!(fs.parent_of(guide_ino).unwrap(), docs_ino);
        assert_eq!(fs.parent_of(docs_ino).unwrap(), ROOT_INO);
        assert_eq!(fs.parent_of(a_ino).unwrap(), guide_ino);

        // synthetic directories reject structural mutations, same as root
        assert!(matches!(fs.mkdir(docs_ino, "x"), Err(VfsError::PermissionDenied)));

        let (entries, _) = fs.readdir(docs_ino, 0, 100).unwrap();
        let mut names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["api", "guide"]);
    }

    #[test]
    fn colliding_mount_paths_get_suffixed_like_sibling_headings() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        std::fs::write(&a, "# A\n").unwrap();
        std::fs::write(&b, "# B\n").unwrap();

        let fs = MqFs::new(vec![(a, vec!["doc".into()]), (b, vec!["doc".into()])], false, false).unwrap();

        assert!(fs.lookup(ROOT_INO, "doc").is_ok());
        assert!(fs.lookup(ROOT_INO, "doc-2").is_ok());
    }

    #[test]
    fn frontmatter_write_then_read_round_trips() {
        let (fs, dir, file_ino) = mounted("---\nkey: 1\n---\n# Title\n\nbody\n");
        let fm_ino = fs.lookup(file_ino, "_frontmatter.yaml").unwrap();

        fs.write(fm_ino, 0, b"key: 2").unwrap();
        let (bytes, _) = fs.read(fm_ino, 0, 1024).unwrap();
        assert_eq!(bytes, b"key: 2");

        let persisted = std::fs::read_to_string(dir.path().join("doc.md")).unwrap();
        assert!(persisted.contains("key: 2"), "persisted doc was: {persisted}");
    }

    #[test]
    fn watch_add_mounts_a_new_top_level_file() {
        let (fs, dir, _file_ino) = mounted("# Doc\n\nbody\n");
        let new_path = dir.path().join("extra.md");
        std::fs::write(&new_path, "# Extra\n\nnew body\n").unwrap();

        fs.watch_add(new_path.canonicalize().unwrap(), vec!["extra".to_string()]);

        let extra_ino = fs.lookup(ROOT_INO, "extra").unwrap();
        let heading_ino = fs.lookup(extra_ino, "Extra").unwrap();
        let content_ino = fs.lookup(heading_ino, document::CONTENT_FILE).unwrap();
        let (bytes, _) = fs.read(content_ino, 0, 1024).unwrap();
        assert_eq!(bytes, b"new body\n");
    }

    #[test]
    fn watch_add_creates_missing_intermediate_directories() {
        let (fs, dir, _file_ino) = mounted("# Doc\n\nbody\n");
        std::fs::create_dir_all(dir.path().join("docs/guide")).unwrap();
        let new_path = dir.path().join("docs/guide/a.md");
        std::fs::write(&new_path, "# A\n").unwrap();

        fs.watch_add(
            new_path.canonicalize().unwrap(),
            vec!["docs".to_string(), "guide".to_string(), "a".to_string()],
        );

        let docs_ino = fs.lookup(ROOT_INO, "docs").unwrap();
        let guide_ino = fs.lookup(docs_ino, "guide").unwrap();
        assert!(fs.lookup(guide_ino, "a").is_ok());
    }

    #[test]
    fn watch_add_reuses_an_existing_intermediate_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("docs")).unwrap();
        let a = dir.path().join("docs/a.md");
        std::fs::write(&a, "# A\n").unwrap();
        let fs = MqFs::new(vec![(a, vec!["docs".into(), "a".into()])], false, false).unwrap();
        let docs_ino_before = fs.lookup(ROOT_INO, "docs").unwrap();

        let b = dir.path().join("docs/b.md");
        std::fs::write(&b, "# B\n").unwrap();
        fs.watch_add(b.canonicalize().unwrap(), vec!["docs".to_string(), "b".to_string()]);

        let docs_ino_after = fs.lookup(ROOT_INO, "docs").unwrap();
        assert_eq!(
            docs_ino_before, docs_ino_after,
            "must reuse the existing docs/ directory"
        );
        assert!(fs.lookup(docs_ino_after, "a").is_ok());
        assert!(fs.lookup(docs_ino_after, "b").is_ok());
    }

    #[test]
    fn watch_add_ignores_a_path_already_mounted() {
        let (fs, dir, file_ino) = mounted("# Doc\n\nbody\n");
        // Matches the exact (non-canonicalized) path `mounted()` mounted it under.
        let doc_path = dir.path().join("doc.md");

        fs.watch_add(doc_path, vec!["doc-2".to_string()]);

        assert!(matches!(fs.lookup(ROOT_INO, "doc-2"), Err(VfsError::NotFound)));
        assert!(fs.lookup(file_ino, "Doc").is_ok());
    }
}
