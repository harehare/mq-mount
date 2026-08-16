//! Per-OS mount backends, each adapting [`crate::vfs::MountFs`] to that OS's
//! native filesystem-hosting protocol.

#[cfg(unix)]
pub mod nfs;

#[cfg(windows)]
pub mod winfsp;
