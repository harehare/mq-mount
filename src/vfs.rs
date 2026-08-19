//! OS/protocol-agnostic virtual filesystem contract implemented by
//! [`crate::fs::MqFs`]. Each mount backend (NFSv3 on Unix, WinFsp on
//! Windows) is a thin adapter that translates its protocol's calls and
//! types into calls against this trait and back.

use std::time::SystemTime;

pub type Ino = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Dir,
    File,
}

#[derive(Debug, Clone, Copy)]
pub struct FileAttr {
    pub ino: Ino,
    pub kind: FileKind,
    pub size: u64,
    pub mtime: SystemTime,
}

#[derive(Debug, Clone)]
pub struct DirEntryOwned {
    pub ino: Ino,
    pub name: String,
    pub attr: FileAttr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsError {
    NotFound,
    NotDir,
    IsDir,
    Exists,
    NotEmpty,
    ReadOnly,
    PermissionDenied,
    Unsupported,
    Invalid,
    Io,
    /// The source file changed on disk outside the mount since it was last
    /// read; the write was refused rather than silently overwriting it.
    Conflict,
}

/// A directory-tree-shaped virtual filesystem, addressed by inode. `"."` and `".."` are not passed to `lookup`; a backend resolves `"."` to the same inode itself and calls [`MountFs::parent_of`] for `".."`.
pub trait MountFs: Send + Sync {
    fn root_ino(&self) -> Ino;
    fn readonly(&self) -> bool;
    /// Whether directory/file permission bits should be loosened for other local users, for backends (NFS) that expose POSIX-style mode bits.
    fn allow_other(&self) -> bool;
    fn uid(&self) -> u32;
    fn gid(&self) -> u32;

    fn lookup(&self, parent: Ino, name: &str) -> Result<Ino, VfsError>;
    /// Resolves `..` from `ino`, which must be a directory.
    fn parent_of(&self, ino: Ino) -> Result<Ino, VfsError>;
    fn getattr(&self, ino: Ino) -> Result<FileAttr, VfsError>;
    fn truncate(&self, ino: Ino) -> Result<FileAttr, VfsError>;
    fn read(&self, ino: Ino, offset: u64, count: u32) -> Result<(Vec<u8>, bool), VfsError>;
    fn write(&self, ino: Ino, offset: u64, data: &[u8]) -> Result<FileAttr, VfsError>;
    fn create(&self, parent: Ino, name: &str) -> Result<(Ino, FileAttr), VfsError>;
    fn create_exclusive(&self, parent: Ino, name: &str) -> Result<Ino, VfsError>;
    fn mkdir(&self, parent: Ino, name: &str) -> Result<(Ino, FileAttr), VfsError>;
    fn remove(&self, parent: Ino, name: &str) -> Result<(), VfsError>;
    fn rename(&self, from_parent: Ino, from_name: &str, to_parent: Ino, to_name: &str) -> Result<(), VfsError>;
    fn readdir(&self, ino: Ino, start_after: Ino, max_entries: usize) -> Result<(Vec<DirEntryOwned>, bool), VfsError>;
}

/// Lets a backend and a background task (e.g. a directory watcher) share one
/// filesystem instance behind an `Arc`, without every call site needing to
/// know whether it's holding the owned filesystem or a shared handle to it.
impl<T: MountFs + ?Sized> MountFs for std::sync::Arc<T> {
    fn root_ino(&self) -> Ino {
        (**self).root_ino()
    }

    fn readonly(&self) -> bool {
        (**self).readonly()
    }

    fn allow_other(&self) -> bool {
        (**self).allow_other()
    }

    fn uid(&self) -> u32 {
        (**self).uid()
    }

    fn gid(&self) -> u32 {
        (**self).gid()
    }

    fn lookup(&self, parent: Ino, name: &str) -> Result<Ino, VfsError> {
        (**self).lookup(parent, name)
    }

    fn parent_of(&self, ino: Ino) -> Result<Ino, VfsError> {
        (**self).parent_of(ino)
    }

    fn getattr(&self, ino: Ino) -> Result<FileAttr, VfsError> {
        (**self).getattr(ino)
    }

    fn truncate(&self, ino: Ino) -> Result<FileAttr, VfsError> {
        (**self).truncate(ino)
    }

    fn read(&self, ino: Ino, offset: u64, count: u32) -> Result<(Vec<u8>, bool), VfsError> {
        (**self).read(ino, offset, count)
    }

    fn write(&self, ino: Ino, offset: u64, data: &[u8]) -> Result<FileAttr, VfsError> {
        (**self).write(ino, offset, data)
    }

    fn create(&self, parent: Ino, name: &str) -> Result<(Ino, FileAttr), VfsError> {
        (**self).create(parent, name)
    }

    fn create_exclusive(&self, parent: Ino, name: &str) -> Result<Ino, VfsError> {
        (**self).create_exclusive(parent, name)
    }

    fn mkdir(&self, parent: Ino, name: &str) -> Result<(Ino, FileAttr), VfsError> {
        (**self).mkdir(parent, name)
    }

    fn remove(&self, parent: Ino, name: &str) -> Result<(), VfsError> {
        (**self).remove(parent, name)
    }

    fn rename(&self, from_parent: Ino, from_name: &str, to_parent: Ino, to_name: &str) -> Result<(), VfsError> {
        (**self).rename(from_parent, from_name, to_parent, to_name)
    }

    fn readdir(&self, ino: Ino, start_after: Ino, max_entries: usize) -> Result<(Vec<DirEntryOwned>, bool), VfsError> {
        (**self).readdir(ino, start_after, max_entries)
    }
}
