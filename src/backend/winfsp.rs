//! WinFSP backend (Windows): adapts [`crate::vfs::MountFs`] to the `winfsp`
//! crate's [`FileSystemContext`] trait.
//!
//! Unlike the NFS backend, this has not been built or run against a real
//! Windows target with WinFSP installed — there is no Windows toolchain or
//! WinFSP driver available in the environment this was written in. It is
//! written directly against the `winfsp` crate's documented trait
//! signatures and the `memfs-winfsp-rs` example filesystem shipped in that
//! crate's repository, but treat it as unverified until it has been built
//! and exercised on an actual Windows machine with WinFSP installed
//! (https://winfsp.dev).

use std::ffi::c_void;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use widestring::U16CStr;
use windows::Win32::Foundation::{
    STATUS_DIRECTORY_NOT_EMPTY, STATUS_FILE_LOCK_CONFLICT, STATUS_INVALID_DEVICE_REQUEST,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY};
use winfsp::filesystem::{DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo, WideNameInfo};
use winfsp::host::{FileSystemHost, FileSystemParams, VolumeParams};
use winfsp::service::FileSystemServiceBuilder;
use winfsp::{FspError, Result as FspResult};

use crate::vfs::{FileAttr, FileKind, Ino, MountFs, VfsError};

struct WinFspAdapter<T: MountFs>(T);

fn map_err(e: VfsError) -> FspError {
    use std::io::ErrorKind;
    match e {
        VfsError::NotFound => FspError::IO(ErrorKind::NotFound),
        VfsError::NotDir => FspError::IO(ErrorKind::NotADirectory),
        VfsError::IsDir => FspError::IO(ErrorKind::IsADirectory),
        VfsError::Exists => FspError::IO(ErrorKind::AlreadyExists),
        VfsError::NotEmpty => FspError::NTSTATUS(STATUS_DIRECTORY_NOT_EMPTY.0),
        VfsError::ReadOnly | VfsError::PermissionDenied => FspError::IO(ErrorKind::PermissionDenied),
        VfsError::Unsupported => FspError::NTSTATUS(STATUS_INVALID_DEVICE_REQUEST.0),
        VfsError::Invalid => FspError::IO(ErrorKind::InvalidInput),
        VfsError::Io => FspError::IO(ErrorKind::Other),
        VfsError::Conflict => FspError::NTSTATUS(STATUS_FILE_LOCK_CONFLICT.0),
    }
}

/// Windows FILETIME: 100ns intervals since 1601-01-01, vs. `SystemTime`'s
/// Unix epoch (1970-01-01); the difference between those epochs in 100ns
/// units is the well-known constant below.
fn filetime(t: SystemTime) -> u64 {
    const EPOCH_DIFF_100NS: u64 = 116_444_736_000_000_000;
    let unix = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    EPOCH_DIFF_100NS + unix.as_nanos() as u64 / 100
}

fn file_attributes(kind: FileKind, readonly: bool) -> u32 {
    let mut attrs = match kind {
        FileKind::Dir => FILE_ATTRIBUTE_DIRECTORY.0,
        FileKind::File => FILE_ATTRIBUTE_NORMAL.0,
    };
    if readonly {
        attrs |= FILE_ATTRIBUTE_READONLY.0;
    }
    attrs
}

impl<T: MountFs> WinFspAdapter<T> {
    fn fill_file_info(&self, info: &mut FileInfo, attr: &FileAttr) {
        let t = filetime(attr.mtime);
        info.file_attributes = file_attributes(attr.kind, self.0.readonly());
        info.file_size = attr.size;
        info.allocation_size = attr.size;
        info.creation_time = t;
        info.last_access_time = t;
        info.last_write_time = t;
        info.change_time = t;
    }

    /// Resolves an absolute, backslash-separated WinFSP path (e.g.
    /// `\Title\Sub\content.md`) to an inode by walking [`MountFs::lookup`]
    /// one component at a time.
    fn resolve(&self, file_name: &U16CStr) -> FspResult<Ino> {
        let path = file_name.to_string_lossy();
        let mut ino = self.0.root_ino();
        for component in path.split('\\').filter(|c| !c.is_empty()) {
            ino = self.0.lookup(ino, component).map_err(map_err)?;
        }
        Ok(ino)
    }

    /// Splits an absolute WinFSP path into its parent inode and leaf name.
    fn resolve_parent_and_name(&self, file_name: &U16CStr) -> FspResult<(Ino, String)> {
        let path = file_name.to_string_lossy();
        let trimmed = path.trim_start_matches('\\');
        let (parent, name) = trimmed.rsplit_once('\\').unwrap_or(("", trimmed));

        let mut ino = self.0.root_ino();
        for component in parent.split('\\').filter(|c| !c.is_empty()) {
            ino = self.0.lookup(ino, component).map_err(map_err)?;
        }
        Ok((ino, name.to_string()))
    }
}

impl<T: MountFs> FileSystemContext for WinFspAdapter<T> {
    type FileContext = Ino;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> FspResult<FileSecurity> {
        let ino = self.resolve(file_name)?;
        let attr = self.0.getattr(ino).map_err(map_err)?;
        Ok(FileSecurity {
            reparse: false,
            // No ACL support: report a zero-size descriptor.
            sz_security_descriptor: 0,
            attributes: file_attributes(attr.kind, self.0.readonly()),
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: winfsp_sys::FILE_ACCESS_RIGHTS,
        file_info: &mut OpenFileInfo,
    ) -> FspResult<Self::FileContext> {
        let ino = self.resolve(file_name)?;
        let attr = self.0.getattr(ino).map_err(map_err)?;
        self.fill_file_info(file_info.as_mut(), &attr);
        Ok(ino)
    }

    fn close(&self, _context: Self::FileContext) {}

    #[allow(clippy::too_many_arguments)]
    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: winfsp_sys::FILE_ACCESS_RIGHTS,
        _file_attributes: winfsp_sys::FILE_FLAGS_AND_ATTRIBUTES,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> FspResult<Self::FileContext> {
        const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
        let (parent, name) = self.resolve_parent_and_name(file_name)?;
        let (ino, attr) = if create_options & FILE_DIRECTORY_FILE != 0 {
            self.0.mkdir(parent, &name).map_err(map_err)?
        } else {
            self.0.create(parent, &name).map_err(map_err)?
        };
        self.fill_file_info(file_info.as_mut(), &attr);
        Ok(ino)
    }

    fn get_file_info(&self, context: &Self::FileContext, file_info: &mut FileInfo) -> FspResult<()> {
        let attr = self.0.getattr(*context).map_err(map_err)?;
        self.fill_file_info(file_info, &attr);
        Ok(())
    }

    fn read(&self, context: &Self::FileContext, buffer: &mut [u8], offset: u64) -> FspResult<u32> {
        let (bytes, _eof) = self.0.read(*context, offset, buffer.len() as u32).map_err(map_err)?;
        buffer[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len() as u32)
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        _constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> FspResult<u32> {
        let offset = if write_to_eof {
            self.0.getattr(*context).map_err(map_err)?.size
        } else {
            offset
        };
        let attr = self.0.write(*context, offset, buffer).map_err(map_err)?;
        self.fill_file_info(file_info, &attr);
        Ok(buffer.len() as u32)
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        _set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> FspResult<()> {
        let attr = if new_size == 0 {
            self.0.truncate(*context).map_err(map_err)?
        } else {
            self.0.getattr(*context).map_err(map_err)?
        };
        self.fill_file_info(file_info, &attr);
        Ok(())
    }

    fn set_delete(&self, _context: &Self::FileContext, file_name: &U16CStr, delete_file: bool) -> FspResult<()> {
        if !delete_file {
            return Ok(());
        }
        // This filesystem has no concept of a cancellable pending delete
        // (sections are deleted the same way a heading is removed through
        // any other backend), so unlike the WinFSP convention this deletes
        // immediately rather than deferring to `cleanup`. A delete that
        // Windows later decides not to go through with (rare in practice)
        // would still remove the section.
        let (parent, name) = self.resolve_parent_and_name(file_name)?;
        self.0.remove(parent, &name).map_err(map_err)
    }

    fn rename(
        &self,
        _context: &Self::FileContext,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        _replace_if_exists: bool,
    ) -> FspResult<()> {
        let (from_parent, from_name) = self.resolve_parent_and_name(file_name)?;
        let (to_parent, to_name) = self.resolve_parent_and_name(new_file_name)?;
        self.0
            .rename(from_parent, &from_name, to_parent, &to_name)
            .map_err(map_err)
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> FspResult<u32> {
        let ino = *context;
        let mut cursor = 0u32;
        let mut dir_info: DirInfo<255> = DirInfo::new();

        let is_root = ino == self.0.root_ino();
        let marker_name = marker.inner_as_cstr().map(|m| m.to_string_lossy());
        let marker_is_none = marker.is_none();
        let marker_is_dot = marker_name.as_deref() == Some(".");
        let marker_is_dotdot = marker_name.as_deref() == Some("..");

        if !is_root {
            if marker_is_none {
                let attr = self.0.getattr(ino).map_err(map_err)?;
                dir_info.reset();
                self.fill_file_info(dir_info.file_info_mut(), &attr);
                dir_info
                    .set_name_raw([b'.' as u16].as_slice())
                    .map_err(FspError::from)?;
                if !dir_info.append_to_buffer(buffer, &mut cursor) {
                    return Ok(cursor);
                }
            }
            if marker_is_none || marker_is_dot {
                let parent_ino = self.0.parent_of(ino).map_err(map_err)?;
                let attr = self.0.getattr(parent_ino).map_err(map_err)?;
                dir_info.reset();
                self.fill_file_info(dir_info.file_info_mut(), &attr);
                dir_info
                    .set_name_raw([b'.' as u16, b'.' as u16].as_slice())
                    .map_err(FspError::from)?;
                if !dir_info.append_to_buffer(buffer, &mut cursor) {
                    return Ok(cursor);
                }
            }
        }

        let (entries, _end) = self.0.readdir(ino, 0, usize::MAX).map_err(map_err)?;
        let start_index = if marker_is_none || marker_is_dot || marker_is_dotdot {
            0
        } else if let Some(name) = marker_name {
            entries
                .iter()
                .position(|e| e.name == name)
                .map(|i| i + 1)
                .unwrap_or(entries.len())
        } else {
            0
        };

        for entry in &entries[start_index..] {
            dir_info.reset();
            self.fill_file_info(dir_info.file_info_mut(), &entry.attr);
            dir_info.set_name(&entry.name).map_err(FspError::from)?;
            if !dir_info.append_to_buffer(buffer, &mut cursor) {
                return Ok(cursor);
            }
        }

        DirInfo::<255>::finalize_buffer(buffer, &mut cursor);
        Ok(cursor)
    }
}

pub fn run<T: MountFs + Send + Sync + 'static>(
    filesystem: T,
    mountpoint: &Path,
    file_count: usize,
    readonly: bool,
    name: &str,
) -> miette::Result<()> {
    let init = winfsp::winfsp_init().map_err(|e| miette::miette!("failed to initialize WinFSP: {e:?}"))?;

    let filesystem = std::sync::Mutex::new(Some(filesystem));
    let mountpoint_owned = mountpoint.to_path_buf();
    let filesystem_name = name.to_string();

    let mut service = FileSystemServiceBuilder::new()
        .with_start(move || {
            let fs = filesystem
                .lock()
                .unwrap()
                .take()
                .expect("mq-mount only starts the WinFSP service once");

            let mut volume_params = VolumeParams::new();
            volume_params
                .sector_size(4096)
                .sectors_per_allocation_unit(1)
                .case_sensitive_search(true)
                .case_preserved_names(true)
                .unicode_on_disk(true)
                .persistent_acls(false)
                .read_only_volume(readonly)
                .filesystem_name(&filesystem_name)
                .volume_creation_time(filetime(SystemTime::now()));
            let params = FileSystemParams::default_params(volume_params);

            let mut host: FileSystemHost<WinFspAdapter<T>> =
                FileSystemHost::new_with_options(params, WinFspAdapter(fs))?;
            host.mount(&mountpoint_owned)?;
            host.start()?;
            Ok(host)
        })
        .with_stop(|host| {
            if let Some(host) = host {
                host.stop();
            }
            Ok(())
        })
        .build(name, init)
        .map_err(|e| miette::miette!("failed to build WinFSP service: {e:?}"))?;

    service
        .start()
        .map_err(|e| miette::miette!("failed to start WinFSP service: {e:?}"))?;
    tracing::info!("mounted {file_count} file(s) at {}", mountpoint.display());

    service
        .join()
        .map_err(|e| miette::miette!("WinFSP service exited with an error: {e:?}"))?;
    tracing::info!("unmounted");
    Ok(())
}
