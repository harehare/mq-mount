//! `NFSFileSystem` glue: translates NFSv3 calls into [`crate::document`]
//! mutations. State lives behind a `Mutex` since every trait method takes
//! `&self`.
//!
//! Each mounted file gets its own top-level directory (named after the file,
//! deduplicated like sibling headings) under a synthetic super-root at
//! `ROOT_INO`. NFSv3 has no open/release; writes to `content.md` /
//! frontmatter files are committed straight into the document on every
//! `write()` call, and files created under a name we don't recognize (an
//! editor's atomic-save temp file) are buffered in `scratch` until a
//! `rename()` lands them on a canonical name.

use std::fs;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use nfsserve::nfs::{fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfstime3, sattr3, set_size3, specdata3};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use rustc_hash::FxHashMap;

use crate::document::{self, Document, FrontMatterKind, MutationError, Section};
use crate::inode::{InodeTable, MountPath, ROOT_INO};

struct FileMount {
    slug: String,
    document: Document,
    tree: document::SectionTree,
    inodes: InodeTable,
    source_path: PathBuf,
    last_persisted: String,
}

struct MountState {
    files: Vec<FileMount>,
    root_children: Vec<(String, u64)>,
    ino_owner: FxHashMap<u64, usize>,
    next_ino: u64,
    /// Buffers for files created under a non-canonical name (editor temp
    /// files), keyed by their allocated fileid. Consumed by `rename()` onto a
    /// canonical name; otherwise leaked for the life of the mount, matching
    /// how the equivalent FUSE handle-based buffers behaved.
    scratch: FxHashMap<u64, Vec<u8>>,
    scratch_by_name: FxHashMap<(u64, String), u64>,
    readonly: bool,
    allow_other: bool,
    uid: u32,
    gid: u32,
}

impl MountState {
    fn open(source_paths: Vec<PathBuf>, readonly: bool, allow_other: bool) -> std::io::Result<Self> {
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

        let root_children: Vec<(String, u64)> = files
            .iter()
            .map(|f| (f.slug.clone(), f.inodes.root_dir_ino()))
            .collect();
        let mut ino_owner = FxHashMap::default();
        for (idx, f) in files.iter().enumerate() {
            for (ino, _) in f.inodes.entries() {
                ino_owner.insert(ino, idx);
            }
        }

        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Ok(Self {
            files,
            root_children,
            ino_owner,
            next_ino,
            scratch: FxHashMap::default(),
            scratch_by_name: FxHashMap::default(),
            readonly,
            allow_other,
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

    fn alloc_ino(&mut self) -> u64 {
        let ino = self.next_ino;
        self.next_ino += 1;
        ino
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

    fn commit_content(&mut self, file_idx: usize, dir_path: &[String], text: &str) -> Result<(), nfsstat3> {
        let Some(range) = self.section_range(file_idx, dir_path) else {
            return Ok(());
        };
        self.files[file_idx]
            .document
            .replace_section_content(range, text)
            .map_err(map_mutation_error)?;
        self.rebuild(file_idx);
        self.persist(file_idx).map_err(|_| nfsstat3::NFS3ERR_IO)?;
        Ok(())
    }

    fn commit_frontmatter(&mut self, file_idx: usize, kind: FrontMatterKind, text: &str) -> Result<(), nfsstat3> {
        if let Some((found_kind, idx)) = self.files[file_idx].tree.front_matter
            && found_kind == kind
        {
            self.files[file_idx].document.set_frontmatter(idx, text.to_string());
            self.rebuild(file_idx);
            self.persist(file_idx).map_err(|_| nfsstat3::NFS3ERR_IO)?;
        }
        Ok(())
    }

    fn attr(&self, ino: u64, kind: ftype3, size: u64) -> fattr3 {
        let now = nfs_now();
        let mode = match kind {
            ftype3::NF3DIR if self.allow_other => 0o777,
            ftype3::NF3DIR => 0o755,
            _ if self.readonly => 0o444,
            _ if self.allow_other => 0o666,
            _ => 0o644,
        };
        fattr3 {
            ftype: kind,
            mode,
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            size,
            used: size,
            rdev: specdata3::default(),
            fsid: 0,
            fileid: ino,
            atime: now,
            mtime: now,
            ctime: now,
        }
    }

    fn attr_for(&self, file_idx: usize, ino: u64, path: &MountPath) -> fattr3 {
        match path {
            MountPath::Dir(_) => self.attr(ino, ftype3::NF3DIR, 0),
            MountPath::Content(p) => self.attr(ino, ftype3::NF3REG, self.section_bytes(file_idx, p).len() as u64),
            MountPath::FrontMatter(kind) => self.attr(
                ino,
                ftype3::NF3REG,
                self.frontmatter_bytes(file_idx, *kind).len() as u64,
            ),
        }
    }
}

fn nfs_now() -> nfstime3 {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    nfstime3 {
        seconds: d.as_secs() as u32,
        nseconds: d.subsec_nanos(),
    }
}

fn map_mutation_error(e: MutationError) -> nfsstat3 {
    match e {
        MutationError::AlreadyExists(_) => nfsstat3::NFS3ERR_EXIST,
        MutationError::NotEmpty => nfsstat3::NFS3ERR_NOTEMPTY,
        MutationError::NotADirectory => nfsstat3::NFS3ERR_NOTDIR,
        MutationError::Parse(_) => nfsstat3::NFS3ERR_INVAL,
    }
}

fn frontmatter_kind_for_name(name: &str) -> Option<FrontMatterKind> {
    [FrontMatterKind::Yaml, FrontMatterKind::Toml]
        .into_iter()
        .find(|k| k.file_name() == name)
}

fn name_str(name: &filename3) -> Result<&str, nfsstat3> {
    std::str::from_utf8(name.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)
}

fn splice(buf: &mut Vec<u8>, offset: usize, data: &[u8]) {
    if offset + data.len() > buf.len() {
        buf.resize(offset + data.len(), 0);
    }
    buf[offset..offset + data.len()].copy_from_slice(data);
}

fn paginate(entries: Vec<DirEntry>, start_after: fileid3, max_entries: usize) -> ReadDirResult {
    let start_index = if start_after == 0 {
        0
    } else {
        match entries.iter().position(|e| e.fileid == start_after) {
            Some(i) => i + 1,
            None => entries.len(),
        }
    };
    let max_entries = max_entries.max(1);
    let end = entries.len().saturating_sub(start_index) <= max_entries;
    let page = entries.into_iter().skip(start_index).take(max_entries).collect();
    ReadDirResult { entries: page, end }
}

pub struct MqFs {
    state: Mutex<MountState>,
}

impl MqFs {
    pub fn new(source_paths: Vec<PathBuf>, readonly: bool, allow_other: bool) -> std::io::Result<Self> {
        Ok(Self {
            state: Mutex::new(MountState::open(source_paths, readonly, allow_other)?),
        })
    }
}

#[async_trait]
impl NFSFileSystem for MqFs {
    fn capabilities(&self) -> VFSCapabilities {
        if self.state.lock().unwrap().readonly {
            VFSCapabilities::ReadOnly
        } else {
            VFSCapabilities::ReadWrite
        }
    }

    fn root_dir(&self) -> fileid3 {
        ROOT_INO
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let state = self.state.lock().unwrap();
        let name = name_str(filename)?;
        tracing::debug!("lookup(dirid={dirid}, name={name:?})");

        if name == "." {
            return Ok(dirid);
        }
        if name == ".." {
            if dirid == ROOT_INO {
                return Ok(ROOT_INO);
            }
            let (file_idx, path) = state.dir_path(dirid).ok_or(nfsstat3::NFS3ERR_NOTDIR)?;
            return Ok(if path.is_empty() {
                ROOT_INO
            } else {
                state.files[file_idx]
                    .inodes
                    .ino_for(&MountPath::Dir(path[..path.len() - 1].to_vec()))
                    .unwrap_or(ROOT_INO)
            });
        }

        if dirid == ROOT_INO {
            return state
                .root_children
                .iter()
                .find(|(slug, _)| slug == name)
                .map(|&(_, ino)| ino)
                .ok_or(nfsstat3::NFS3ERR_NOENT);
        }

        let (file_idx, parent_path) = state.dir_path(dirid).ok_or(nfsstat3::NFS3ERR_NOTDIR)?;
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
            return Err(nfsstat3::NFS3ERR_NOENT);
        };

        state.files[file_idx]
            .inodes
            .ino_for(&child_path)
            .ok_or(nfsstat3::NFS3ERR_NOENT)
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let state = self.state.lock().unwrap();
        if let Some(buf) = state.scratch.get(&id) {
            tracing::debug!("getattr({id}): scratch, {} bytes", buf.len());
            return Ok(state.attr(id, ftype3::NF3REG, buf.len() as u64));
        }
        if id == ROOT_INO {
            return Ok(state.attr(id, ftype3::NF3DIR, 0));
        }
        let (file_idx, path) = state.path_for_ino(id).ok_or_else(|| {
            tracing::debug!("getattr({id}): no path for this fileid");
            nfsstat3::NFS3ERR_NOENT
        })?;
        let attr = state.attr_for(file_idx, id, &path);
        tracing::debug!("getattr({id}): path={path:?} size={}", attr.size);
        Ok(attr)
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let mut state = self.state.lock().unwrap();
        if state.readonly {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        tracing::debug!("setattr({id}): size={:?}", setattr.size);
        if let set_size3::size(0) = setattr.size {
            tracing::debug!("setattr({id}): truncating to 0");
            if let Some(buf) = state.scratch.get_mut(&id) {
                buf.clear();
            } else if let Some((file_idx, path)) = state.path_for_ino(id) {
                match &path {
                    MountPath::Content(p) => state.commit_content(file_idx, p, "")?,
                    MountPath::FrontMatter(kind) => state.commit_frontmatter(file_idx, *kind, "")?,
                    MountPath::Dir(_) => return Err(nfsstat3::NFS3ERR_ISDIR),
                }
            }
        }
        if let Some(buf) = state.scratch.get(&id) {
            return Ok(state.attr(id, ftype3::NF3REG, buf.len() as u64));
        }
        if id == ROOT_INO {
            return Ok(state.attr(id, ftype3::NF3DIR, 0));
        }
        let (file_idx, path) = state.path_for_ino(id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        Ok(state.attr_for(file_idx, id, &path))
    }

    async fn read(&self, id: fileid3, offset: u64, count: u32) -> Result<(Vec<u8>, bool), nfsstat3> {
        let state = self.state.lock().unwrap();
        let bytes = if let Some(buf) = state.scratch.get(&id) {
            buf.clone()
        } else {
            match state.path_for_ino(id) {
                Some((file_idx, MountPath::Content(p))) => state.section_bytes(file_idx, &p),
                Some((file_idx, MountPath::FrontMatter(kind))) => state.frontmatter_bytes(file_idx, kind),
                Some((_, MountPath::Dir(_))) => return Err(nfsstat3::NFS3ERR_ISDIR),
                None => {
                    tracing::debug!("read({id}): no path for this fileid");
                    return Err(nfsstat3::NFS3ERR_NOENT);
                }
            }
        };
        tracing::debug!(
            "read({id}, offset={offset}, count={count}): {} bytes available",
            bytes.len()
        );
        let offset = offset as usize;
        if offset >= bytes.len() {
            return Ok((Vec::new(), true));
        }
        let end = (offset + count as usize).min(bytes.len());
        Ok((bytes[offset..end].to_vec(), end >= bytes.len()))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        let mut state = self.state.lock().unwrap();
        if state.readonly {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let offset = offset as usize;

        if let Some(buf) = state.scratch.get_mut(&id) {
            splice(buf, offset, data);
            let len = buf.len() as u64;
            return Ok(state.attr(id, ftype3::NF3REG, len));
        }

        let (file_idx, path) = state.path_for_ino(id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        match &path {
            MountPath::Content(p) => {
                let mut buf = state.section_bytes(file_idx, p);
                splice(&mut buf, offset, data);
                let text = String::from_utf8_lossy(&buf).into_owned();
                state.commit_content(file_idx, p, &text)?;
            }
            MountPath::FrontMatter(kind) => {
                let mut buf = state.frontmatter_bytes(file_idx, *kind);
                splice(&mut buf, offset, data);
                let text = String::from_utf8_lossy(&buf).into_owned();
                state.commit_frontmatter(file_idx, *kind, &text)?;
            }
            MountPath::Dir(_) => return Err(nfsstat3::NFS3ERR_ISDIR),
        }
        Ok(state.attr_for(file_idx, id, &path))
    }

    async fn create(&self, dirid: fileid3, filename: &filename3, _attr: sattr3) -> Result<(fileid3, fattr3), nfsstat3> {
        let mut state = self.state.lock().unwrap();
        if state.readonly {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let name = name_str(filename)?;
        let (file_idx, parent_path) = state.dir_path(dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;

        let canonical = if name == document::CONTENT_FILE {
            Some(MountPath::Content(parent_path))
        } else if parent_path.is_empty()
            && let Some(kind) = frontmatter_kind_for_name(name)
        {
            Some(MountPath::FrontMatter(kind))
        } else {
            None
        };
        if let Some(mp) = canonical {
            let ino = state.files[file_idx].inodes.ino_for(&mp).ok_or(nfsstat3::NFS3ERR_IO)?;
            return Ok((ino, state.attr_for(file_idx, ino, &mp)));
        }

        let key = (dirid, name.to_string());
        if let Some(&ino) = state.scratch_by_name.get(&key) {
            let len = state.scratch.get(&ino).map(Vec::len).unwrap_or(0) as u64;
            return Ok((ino, state.attr(ino, ftype3::NF3REG, len)));
        }
        let ino = state.alloc_ino();
        state.scratch.insert(ino, Vec::new());
        state.scratch_by_name.insert(key, ino);
        Ok((ino, state.attr(ino, ftype3::NF3REG, 0)))
    }

    async fn create_exclusive(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let mut state = self.state.lock().unwrap();
        if state.readonly {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let name = name_str(filename)?;

        let exists = if dirid == ROOT_INO {
            state.root_children.iter().any(|(slug, _)| slug == name)
        } else {
            let (file_idx, parent_path) = state.dir_path(dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;
            name == document::CONTENT_FILE
                || (parent_path.is_empty() && frontmatter_kind_for_name(name).is_some())
                || state
                    .find_section(file_idx, &parent_path)
                    .is_some_and(|s| s.children.iter().any(|c| c.name == name))
                || state.scratch_by_name.contains_key(&(dirid, name.to_string()))
        };
        if exists {
            return Err(nfsstat3::NFS3ERR_EXIST);
        }

        let ino = state.alloc_ino();
        state.scratch.insert(ino, Vec::new());
        state.scratch_by_name.insert((dirid, name.to_string()), ino);
        Ok(ino)
    }

    async fn mkdir(&self, dirid: fileid3, dirname: &filename3) -> Result<(fileid3, fattr3), nfsstat3> {
        let mut state = self.state.lock().unwrap();
        if state.readonly {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        if dirid == ROOT_INO {
            return Err(nfsstat3::NFS3ERR_PERM);
        }
        let name = name_str(dirname)?;
        let (file_idx, parent_path) = state.dir_path(dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        let parent_section = state
            .find_section(file_idx, &parent_path)
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;

        state.files[file_idx]
            .document
            .insert_heading(&parent_section, name)
            .map_err(map_mutation_error)?;
        state.rebuild(file_idx);
        state.persist(file_idx).map_err(|_| nfsstat3::NFS3ERR_IO)?;

        let mut child_path = parent_path;
        child_path.push(name.to_string());
        let mp = MountPath::Dir(child_path);
        let ino = state.files[file_idx].inodes.ino_for(&mp).ok_or(nfsstat3::NFS3ERR_IO)?;
        Ok((ino, state.attr_for(file_idx, ino, &mp)))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let mut state = self.state.lock().unwrap();
        if state.readonly {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let name = name_str(filename)?;

        let key = (dirid, name.to_string());
        if let Some(ino) = state.scratch_by_name.remove(&key) {
            state.scratch.remove(&ino);
            return Ok(());
        }
        if dirid == ROOT_INO {
            return Err(nfsstat3::NFS3ERR_PERM);
        }
        let (file_idx, parent_path) = state.dir_path(dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;

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
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;
        state.files[file_idx]
            .document
            .remove_heading(&section)
            .map_err(map_mutation_error)?;
        state.rebuild(file_idx);
        state.persist(file_idx).map_err(|_| nfsstat3::NFS3ERR_IO)
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        let mut state = self.state.lock().unwrap();
        if state.readonly {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let name = name_str(from_filename)?;
        let newname = name_str(to_filename)?;

        let (dest_file_idx, new_parent_path) = state.dir_path(to_dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;

        let dest_kind = if newname == document::CONTENT_FILE {
            Some(None)
        } else if new_parent_path.is_empty() {
            frontmatter_kind_for_name(newname).map(Some)
        } else {
            None
        };

        if let Some(front_matter_kind) = dest_kind {
            let key = (from_dirid, name.to_string());
            let (bytes, clear_source_path) = if let Some(ino) = state.scratch_by_name.get(&key).copied() {
                (state.scratch.get(&ino).cloned(), None)
            } else if name == document::CONTENT_FILE {
                match state.dir_path(from_dirid) {
                    Some((src_file_idx, _)) if src_file_idx != dest_file_idx => {
                        return Err(nfsstat3::NFS3ERR_NOTSUPP);
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

            let bytes = bytes.ok_or(nfsstat3::NFS3ERR_NOENT)?;
            let text = String::from_utf8_lossy(&bytes).into_owned();

            match front_matter_kind {
                None => state.commit_content(dest_file_idx, &new_parent_path, &text)?,
                Some(kind) => state.commit_frontmatter(dest_file_idx, kind, &text)?,
            }
            if let Some(p) = clear_source_path {
                state.commit_content(dest_file_idx, &p, "")?;
            }
            if let Some(ino) = state.scratch_by_name.remove(&key) {
                state.scratch.remove(&ino);
            }
            return Ok(());
        }

        let src_key = (from_dirid, name.to_string());
        if let Some(ino) = state.scratch_by_name.remove(&src_key) {
            state.scratch_by_name.insert((to_dirid, newname.to_string()), ino);
            return Ok(());
        }

        if from_dirid != to_dirid {
            return Err(nfsstat3::NFS3ERR_NOTSUPP);
        }
        let (file_idx, parent_path) = state.dir_path(from_dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        let section = state
            .find_child_section(file_idx, &parent_path, name)
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;
        state.files[file_idx]
            .document
            .rename_heading(&section, newname)
            .map_err(map_mutation_error)?;
        state.rebuild(file_idx);
        state.persist(file_idx).map_err(|_| nfsstat3::NFS3ERR_IO)
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let state = self.state.lock().unwrap();
        let mut entries: Vec<DirEntry> = Vec::new();

        if dirid == ROOT_INO {
            for (slug, ino) in &state.root_children {
                entries.push(DirEntry {
                    fileid: *ino,
                    name: filename3::from(slug.as_bytes()),
                    attr: state.attr(*ino, ftype3::NF3DIR, 0),
                });
            }
            return Ok(paginate(entries, start_after, max_entries));
        }

        let (file_idx, path) = state.dir_path(dirid).ok_or(nfsstat3::NFS3ERR_NOTDIR)?;
        let section = state.find_section(file_idx, &path).ok_or(nfsstat3::NFS3ERR_NOENT)?;

        let content_ino = state.files[file_idx]
            .inodes
            .ino_for(&MountPath::Content(path.clone()))
            .unwrap_or(dirid);
        let content_size = state.section_bytes(file_idx, &path).len() as u64;

        entries.push(DirEntry {
            fileid: content_ino,
            name: filename3::from(document::CONTENT_FILE.as_bytes()),
            attr: state.attr(content_ino, ftype3::NF3REG, content_size),
        });

        if path.is_empty()
            && let Some((kind, _)) = state.files[file_idx].tree.front_matter
            && let Some(fm_ino) = state.files[file_idx].inodes.ino_for(&MountPath::FrontMatter(kind))
        {
            let size = state.frontmatter_bytes(file_idx, kind).len() as u64;
            entries.push(DirEntry {
                fileid: fm_ino,
                name: filename3::from(kind.file_name().as_bytes()),
                attr: state.attr(fm_ino, ftype3::NF3REG, size),
            });
        }

        for child in &section.children {
            let mut p = path.clone();
            p.push(child.name.clone());
            if let Some(child_ino) = state.files[file_idx].inodes.ino_for(&MountPath::Dir(p)) {
                entries.push(DirEntry {
                    fileid: child_ino,
                    name: filename3::from(child.name.as_bytes()),
                    attr: state.attr(child_ino, ftype3::NF3DIR, 0),
                });
            }
        }

        Ok(paginate(entries, start_after, max_entries))
    }

    async fn symlink(
        &self,
        _dirid: fileid3,
        _linkname: &filename3,
        _symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn readlink(&self, _id: fileid3) -> Result<nfspath3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_INVAL)
    }
}

#[cfg(test)]
mod tests {
    use nfsserve::nfs::filename3;

    use super::*;

    /// Mounts `content` as a single file and returns the filesystem, the
    /// tempdir keeping its source path alive, and the fileid of that file's
    /// own top-level directory.
    async fn mounted(content: &str) -> (MqFs, tempfile::TempDir, fileid3) {
        mounted_with(content, false, false).await
    }

    async fn mounted_with(content: &str, readonly: bool, allow_other: bool) -> (MqFs, tempfile::TempDir, fileid3) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.md");
        std::fs::write(&path, content).unwrap();
        let fs = MqFs::new(vec![path.clone()], readonly, allow_other).unwrap();
        let listing = fs.readdir(fs.root_dir(), 0, 100).await.unwrap();
        let file_ino = listing.entries[0].fileid;
        (fs, dir, file_ino)
    }

    fn name(s: &str) -> filename3 {
        filename3::from(s.as_bytes())
    }

    async fn lookup_path(fs: &MqFs, mut dirid: fileid3, path: &[&str]) -> fileid3 {
        for part in path {
            dirid = fs.lookup(dirid, &name(part)).await.unwrap();
        }
        dirid
    }

    #[tokio::test]
    async fn write_then_read_round_trips_through_content_md() {
        let (fs, dir, file_ino) = mounted("# Title\n\noriginal body\n").await;
        let content_ino = lookup_path(&fs, file_ino, &["Title", document::CONTENT_FILE]).await;

        // `write` only overwrites the byte range it's given, like a real
        // write(2) — a whole-file save (shell redirection, an editor) always
        // truncates first, same as O_TRUNC would.
        fs.setattr(
            content_ino,
            sattr3 {
                size: set_size3::size(0),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        fs.write(content_ino, 0, b"new body\n").await.unwrap();
        let (bytes, eof) = fs.read(content_ino, 0, 1024).await.unwrap();
        assert_eq!(bytes, b"new body\n");
        assert!(eof);

        let persisted = std::fs::read_to_string(dir.path().join("doc.md")).unwrap();
        assert!(persisted.contains("new body"), "persisted doc was: {persisted}");
    }

    #[tokio::test]
    async fn mkdir_adds_a_heading_visible_in_readdir_and_on_disk() {
        let (fs, dir, file_ino) = mounted("# Title\n\nbody\n").await;
        let title_ino = fs.lookup(file_ino, &name("Title")).await.unwrap();

        let (sub_ino, attr) = fs.mkdir(title_ino, &name("Sub")).await.unwrap();
        assert_eq!(attr.ftype as u32, ftype3::NF3DIR as u32);

        let listing = fs.readdir(title_ino, 0, 100).await.unwrap();
        assert!(
            listing
                .entries
                .iter()
                .any(|e| e.fileid == sub_ino && e.name.0 == b"Sub")
        );

        let persisted = std::fs::read_to_string(dir.path().join("doc.md")).unwrap();
        assert!(persisted.contains("Sub"), "persisted doc was: {persisted}");
    }

    #[tokio::test]
    async fn remove_rejects_nonempty_directory() {
        let (fs, _dir, file_ino) = mounted("# A\n\n## B\n\nbody\n").await;

        let err = fs.remove(file_ino, &name("A")).await.unwrap_err();
        assert!(matches!(err, nfsstat3::NFS3ERR_NOTEMPTY));
    }

    #[tokio::test]
    async fn remove_deletes_an_empty_heading() {
        let (fs, _dir, file_ino) = mounted("# A\n\n# B\n").await;
        fs.remove(file_ino, &name("A")).await.unwrap();
        assert!(matches!(
            fs.lookup(file_ino, &name("A")).await,
            Err(nfsstat3::NFS3ERR_NOENT)
        ));
        assert!(fs.lookup(file_ino, &name("B")).await.is_ok());
    }

    #[tokio::test]
    async fn rename_renames_a_heading_and_persists() {
        let (fs, dir, file_ino) = mounted("# Old\n\nbody\n").await;
        fs.rename(file_ino, &name("Old"), file_ino, &name("New")).await.unwrap();

        assert!(matches!(
            fs.lookup(file_ino, &name("Old")).await,
            Err(nfsstat3::NFS3ERR_NOENT)
        ));
        assert!(fs.lookup(file_ino, &name("New")).await.is_ok());
        let persisted = std::fs::read_to_string(dir.path().join("doc.md")).unwrap();
        assert!(persisted.contains("New"));
    }

    #[tokio::test]
    async fn atomic_save_via_scratch_rename_commits_content() {
        let (fs, dir, file_ino) = mounted("# Title\n\noriginal\n").await;
        let title_ino = fs.lookup(file_ino, &name("Title")).await.unwrap();

        let (tmp_ino, _attr) = fs.create(title_ino, &name(".tmp"), sattr3::default()).await.unwrap();
        fs.write(tmp_ino, 0, b"saved via temp file\n").await.unwrap();
        fs.rename(title_ino, &name(".tmp"), title_ino, &name(document::CONTENT_FILE))
            .await
            .unwrap();

        let content_ino = fs.lookup(title_ino, &name(document::CONTENT_FILE)).await.unwrap();
        let (bytes, _) = fs.read(content_ino, 0, 1024).await.unwrap();
        assert_eq!(bytes, b"saved via temp file\n");

        let persisted = std::fs::read_to_string(dir.path().join("doc.md")).unwrap();
        assert!(
            persisted.contains("saved via temp file"),
            "persisted doc was: {persisted}"
        );
    }

    #[tokio::test]
    async fn readonly_mount_rejects_every_mutation() {
        let (fs, _dir, file_ino) = mounted_with("# Title\n\nbody\n", true, false).await;
        let title_ino = fs.lookup(file_ino, &name("Title")).await.unwrap();
        let content_ino = fs.lookup(title_ino, &name(document::CONTENT_FILE)).await.unwrap();

        assert!(matches!(
            fs.write(content_ino, 0, b"x").await,
            Err(nfsstat3::NFS3ERR_ROFS)
        ));
        assert!(matches!(
            fs.create(title_ino, &name("x"), sattr3::default()).await,
            Err(nfsstat3::NFS3ERR_ROFS)
        ));
        assert!(matches!(
            fs.mkdir(title_ino, &name("Sub")).await,
            Err(nfsstat3::NFS3ERR_ROFS)
        ));
        assert!(matches!(
            fs.remove(title_ino, &name("content.md")).await,
            Err(nfsstat3::NFS3ERR_ROFS)
        ));
        assert!(matches!(
            fs.rename(file_ino, &name("Title"), file_ino, &name("Renamed")).await,
            Err(nfsstat3::NFS3ERR_ROFS)
        ));
    }

    #[tokio::test]
    async fn setattr_size_zero_truncates_content() {
        let (fs, dir, file_ino) = mounted("# Title\n\nbody\n").await;
        let content_ino = lookup_path(&fs, file_ino, &["Title", document::CONTENT_FILE]).await;

        fs.setattr(
            content_ino,
            sattr3 {
                size: set_size3::size(0),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let (bytes, eof) = fs.read(content_ino, 0, 1024).await.unwrap();
        assert!(bytes.is_empty());
        assert!(eof);
        let persisted = std::fs::read_to_string(dir.path().join("doc.md")).unwrap();
        assert!(!persisted.contains("body"), "persisted doc was: {persisted}");
    }

    #[tokio::test]
    async fn create_exclusive_rejects_an_existing_canonical_name() {
        let (fs, _dir, file_ino) = mounted("# Title\n\nbody\n").await;
        let err = fs
            .create_exclusive(file_ino, &name(document::CONTENT_FILE))
            .await
            .unwrap_err();
        assert!(matches!(err, nfsstat3::NFS3ERR_EXIST));
    }

    #[tokio::test]
    async fn root_directory_rejects_structural_mutations() {
        let (fs, _dir, _file_ino) = mounted("# Title\n\nbody\n").await;
        assert!(matches!(
            fs.mkdir(ROOT_INO, &name("x")).await,
            Err(nfsstat3::NFS3ERR_PERM)
        ));
        assert!(matches!(
            fs.remove(ROOT_INO, &name("x")).await,
            Err(nfsstat3::NFS3ERR_PERM)
        ));
    }

    #[tokio::test]
    async fn readdir_pagination_visits_every_entry_exactly_once_with_max_entries_one() {
        let (fs, _dir, file_ino) = mounted("# A\n\n# B\n\n# C\n\n# D\n").await;

        let mut seen = Vec::new();
        let mut cookie: fileid3 = 0;
        loop {
            let page = fs.readdir(file_ino, cookie, 1).await.unwrap();
            assert!(page.entries.len() <= 1);
            for e in &page.entries {
                seen.push(e.fileid);
                cookie = e.fileid;
            }
            if page.end {
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

    #[tokio::test]
    async fn frontmatter_write_then_read_round_trips() {
        let (fs, dir, file_ino) = mounted("---\nkey: 1\n---\n# Title\n\nbody\n").await;
        let fm_ino = fs.lookup(file_ino, &name("_frontmatter.yaml")).await.unwrap();

        fs.write(fm_ino, 0, b"key: 2").await.unwrap();
        let (bytes, _) = fs.read(fm_ino, 0, 1024).await.unwrap();
        assert_eq!(bytes, b"key: 2");

        let persisted = std::fs::read_to_string(dir.path().join("doc.md")).unwrap();
        assert!(persisted.contains("key: 2"), "persisted doc was: {persisted}");
    }
}
