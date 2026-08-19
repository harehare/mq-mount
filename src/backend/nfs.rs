//! NFSv3 backend (macOS/Linux): adapts [`crate::vfs::MountFs`] to the
//! `nfsserve` crate's protocol types, and owns mounting/unmounting through
//! the OS's built-in NFS client.

use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use nfsserve::nfs::{fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfstime3, sattr3, set_size3, specdata3};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};

use crate::fs::MqFs;
use crate::vfs::{DirEntryOwned, FileAttr, FileKind, MountFs, VfsError};

struct NfsAdapter<T: MountFs>(T);

fn map_err(e: VfsError) -> nfsstat3 {
    match e {
        VfsError::NotFound => nfsstat3::NFS3ERR_NOENT,
        VfsError::NotDir => nfsstat3::NFS3ERR_NOTDIR,
        VfsError::IsDir => nfsstat3::NFS3ERR_ISDIR,
        VfsError::Exists => nfsstat3::NFS3ERR_EXIST,
        VfsError::NotEmpty => nfsstat3::NFS3ERR_NOTEMPTY,
        VfsError::ReadOnly => nfsstat3::NFS3ERR_ROFS,
        VfsError::PermissionDenied => nfsstat3::NFS3ERR_PERM,
        VfsError::Unsupported => nfsstat3::NFS3ERR_NOTSUPP,
        VfsError::Invalid => nfsstat3::NFS3ERR_INVAL,
        VfsError::Io => nfsstat3::NFS3ERR_IO,
        // Closest standard NFS errno for "the object you're operating on no
        // longer matches what you last saw of it".
        VfsError::Conflict => nfsstat3::NFS3ERR_STALE,
    }
}

fn nfs_time(t: SystemTime) -> nfstime3 {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    nfstime3 {
        seconds: d.as_secs() as u32,
        nseconds: d.subsec_nanos(),
    }
}

fn name_str(name: &filename3) -> Result<&str, nfsstat3> {
    std::str::from_utf8(name.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)
}

impl<T: MountFs> NfsAdapter<T> {
    fn to_fattr3(&self, attr: FileAttr) -> fattr3 {
        let readonly = self.0.readonly();
        let allow_other = self.0.allow_other();
        let ftype = match attr.kind {
            FileKind::Dir => ftype3::NF3DIR,
            FileKind::File => ftype3::NF3REG,
        };
        let mode = match attr.kind {
            FileKind::Dir if allow_other => 0o777,
            FileKind::Dir => 0o755,
            FileKind::File if readonly => 0o444,
            FileKind::File if allow_other => 0o666,
            FileKind::File => 0o644,
        };
        let time = nfs_time(attr.mtime);
        fattr3 {
            ftype,
            mode,
            nlink: 1,
            uid: self.0.uid(),
            gid: self.0.gid(),
            size: attr.size,
            used: attr.size,
            rdev: specdata3::default(),
            fsid: 0,
            fileid: attr.ino,
            atime: time,
            mtime: time,
            ctime: time,
        }
    }

    fn to_dir_entry(&self, entry: DirEntryOwned) -> DirEntry {
        DirEntry {
            fileid: entry.ino,
            name: filename3::from(entry.name.as_bytes()),
            attr: self.to_fattr3(entry.attr),
        }
    }
}

#[async_trait]
impl<T: MountFs> NFSFileSystem for NfsAdapter<T> {
    fn capabilities(&self) -> VFSCapabilities {
        if self.0.readonly() {
            VFSCapabilities::ReadOnly
        } else {
            VFSCapabilities::ReadWrite
        }
    }

    fn root_dir(&self) -> fileid3 {
        self.0.root_ino()
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let name = name_str(filename)?;
        if name == "." {
            return Ok(dirid);
        }
        if name == ".." {
            return self.0.parent_of(dirid).map_err(map_err);
        }
        self.0.lookup(dirid, name).map_err(map_err)
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        self.0.getattr(id).map(|a| self.to_fattr3(a)).map_err(map_err)
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        // The only attribute change this filesystem supports is truncating a
        // file to zero length (what an editor's whole-file save does before
        // writing); every other attribute change is silently accepted and
        // ignored, matching a synthetic filesystem with no real permission
        // bits to change.
        if let set_size3::size(0) = setattr.size {
            self.0.truncate(id).map(|a| self.to_fattr3(a)).map_err(map_err)
        } else {
            self.0.getattr(id).map(|a| self.to_fattr3(a)).map_err(map_err)
        }
    }

    async fn read(&self, id: fileid3, offset: u64, count: u32) -> Result<(Vec<u8>, bool), nfsstat3> {
        self.0.read(id, offset, count).map_err(map_err)
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        self.0
            .write(id, offset, data)
            .map(|a| self.to_fattr3(a))
            .map_err(map_err)
    }

    async fn create(&self, dirid: fileid3, filename: &filename3, _attr: sattr3) -> Result<(fileid3, fattr3), nfsstat3> {
        let name = name_str(filename)?;
        let (ino, attr) = self.0.create(dirid, name).map_err(map_err)?;
        Ok((ino, self.to_fattr3(attr)))
    }

    async fn create_exclusive(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let name = name_str(filename)?;
        self.0.create_exclusive(dirid, name).map_err(map_err)
    }

    async fn mkdir(&self, dirid: fileid3, dirname: &filename3) -> Result<(fileid3, fattr3), nfsstat3> {
        let name = name_str(dirname)?;
        let (ino, attr) = self.0.mkdir(dirid, name).map_err(map_err)?;
        Ok((ino, self.to_fattr3(attr)))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let name = name_str(filename)?;
        self.0.remove(dirid, name).map_err(map_err)
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        let name = name_str(from_filename)?;
        let newname = name_str(to_filename)?;
        self.0.rename(from_dirid, name, to_dirid, newname).map_err(map_err)
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let (entries, end) = self.0.readdir(dirid, start_after, max_entries).map_err(map_err)?;
        Ok(ReadDirResult {
            entries: entries.into_iter().map(|e| self.to_dir_entry(e)).collect(),
            end,
        })
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

fn nfs_mount_opts(nolock_flag: &str, port: u16, readonly: bool) -> String {
    let cache_opt = if readonly { "actimeo=30" } else { "noac" };
    let mut opts = format!("{cache_opt},{nolock_flag},vers=3,tcp,port={port},mountport={port}");
    if readonly {
        opts.push_str(",ro");
    }
    opts
}

#[cfg(target_os = "macos")]
fn mount_nfs(mountpoint: &Path, port: u16, readonly: bool, name: &str) -> miette::Result<()> {
    let opts = nfs_mount_opts("nolocks", port, readonly);
    run_mount_command(Command::new("mount_nfs").args([
        "-o",
        &opts,
        &format!("localhost:/{name}"),
        &mountpoint.to_string_lossy(),
    ]))
}

#[cfg(target_os = "linux")]
fn mount_nfs(mountpoint: &Path, port: u16, readonly: bool, name: &str) -> miette::Result<()> {
    let opts = nfs_mount_opts("nolock", port, readonly);
    run_mount_command(Command::new("mount").args([
        "-t",
        "nfs",
        "-o",
        &opts,
        &format!("localhost:/{name}"),
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

pub fn run(filesystem: MqFs, mountpoint: &Path, file_count: usize, readonly: bool, name: &str) -> miette::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| miette::miette!("failed to start async runtime: {e}"))?
        .block_on(run_mounted(filesystem, mountpoint, file_count, readonly, name))
}

async fn run_mounted(
    filesystem: MqFs,
    mountpoint: &Path,
    file_count: usize,
    readonly: bool,
    name: &str,
) -> miette::Result<()> {
    let mut listener = NFSTcpListener::bind("127.0.0.1:0", NfsAdapter(filesystem))
        .await
        .map_err(|e| miette::miette!("failed to start NFS server: {e}"))?;
    listener.with_export_name(name);
    let port = listener.get_listen_port();
    let server = tokio::spawn(async move {
        let _ = listener.handle_forever().await;
    });

    mount_nfs(mountpoint, port, readonly, name)?;
    tracing::info!("mounted {file_count} file(s) at {}", mountpoint.display());

    tokio::signal::ctrl_c().await.ok();

    tracing::info!("unmounting");
    unmount(mountpoint)?;
    server.abort();
    Ok(())
}
