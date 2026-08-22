//! Default cache handler on Linux.

use std::path::Path;

use crate::Error;

use super::fs::{FsBackend, FsCacheHandler};

/// Default Linux instruction-layer cache.
///
/// Currently stores snapshots as directories under `<store>/cache`. This type
/// is the swap point for a Linux-specific cache mechanism.
#[derive(Debug, Clone)]
pub struct LinuxCacheHandler {
    inner: FsCacheHandler,
}

impl LinuxCacheHandler {
    /// Open (or create) the Linux cache under `<data_root>/cache`.
    pub fn open(data_root: &Path) -> Result<Self, Error> {
        Ok(Self {
            inner: FsCacheHandler::open(data_root)?,
        })
    }
}

impl FsBackend for LinuxCacheHandler {
    fn fs(&self) -> &FsCacheHandler {
        &self.inner
    }
}
