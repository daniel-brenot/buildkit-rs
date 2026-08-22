//! Default cache handler on macOS.

use std::path::Path;

use crate::Error;

use super::fs::{FsBackend, FsCacheHandler};

/// Default macOS instruction-layer cache.
///
/// Currently stores snapshots as directories under `<store>/cache`. This type
/// is the swap point for a macOS-specific cache mechanism.
#[derive(Debug, Clone)]
pub struct MacosCacheHandler {
    inner: FsCacheHandler,
}

impl MacosCacheHandler {
    /// Open (or create) the macOS cache under `<data_root>/cache`.
    pub fn open(data_root: &Path) -> Result<Self, Error> {
        Ok(Self {
            inner: FsCacheHandler::open(data_root)?,
        })
    }
}

impl FsBackend for MacosCacheHandler {
    fn fs(&self) -> &FsCacheHandler {
        &self.inner
    }
}
