//! Default cache handler on Windows.

use std::path::Path;

use crate::Error;

use super::fs::{FsBackend, FsCacheHandler};

/// Default Windows instruction-layer cache.
///
/// Currently stores snapshots as directories under `<store>/cache`. This type
/// is the swap point for a Windows-specific cache mechanism.
#[derive(Debug, Clone)]
pub struct WindowsCacheHandler {
    inner: FsCacheHandler,
}

impl WindowsCacheHandler {
    /// Open (or create) the Windows cache under `<data_root>/cache`.
    pub fn open(data_root: &Path) -> Result<Self, Error> {
        Ok(Self {
            inner: FsCacheHandler::open(data_root)?,
        })
    }
}

impl FsBackend for WindowsCacheHandler {
    fn fs(&self) -> &FsCacheHandler {
        &self.inner
    }
}
