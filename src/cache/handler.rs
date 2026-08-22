//! Pluggable instruction-layer cache storage.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::export::ImageMeta;
use crate::Error;

/// Reads and writes instruction-layer cache entries.
///
/// [`crate::LayerCache`] delegates all storage to a handler. The default is
/// OS-specific ([`crate::LinuxCacheHandler`], [`crate::MacosCacheHandler`], or
/// [`crate::WindowsCacheHandler`]); supply another implementation with
/// [`crate::LayerCache::with_handler`].
///
/// Cache hits return a host path to a rootfs snapshot. Callers must
/// copy-on-write before mutating it.
pub trait CacheHandler: Send + Sync + std::fmt::Debug {
    /// On-disk directory this handler uses for snapshots and blobs.
    fn root(&self) -> &Path;

    /// Whether a complete cache entry exists for chain id `id`.
    fn has(&self, id: &str) -> bool;

    /// Load image config and build-arg state for `id` (no rootfs copy).
    fn load_meta(&self, id: &str) -> Result<(ImageMeta, HashMap<String, String>), Error>;

    /// Host path of the materialized rootfs for `id`.
    fn resolve_rootfs(&self, id: &str) -> Result<PathBuf, Error>;

    /// Path to a packed layer file if this handler stores blobs as files.
    fn layer_blob_path(&self, id: &str) -> Option<PathBuf>;

    /// Whether a packed layer blob exists for `id`.
    fn has_layer_blob(&self, id: &str) -> bool;

    /// Read the packed export blob for `id`.
    fn read_layer_blob(&self, id: &str) -> Result<Vec<u8>, Error>;

    /// Digest label for the packed blob (`sha256:…`).
    fn layer_blob_digest(&self, id: &str) -> Result<String, Error>;

    /// Store a packed export blob for `id`.
    fn write_layer_blob(&self, id: &str, bytes: &[u8]) -> Result<(), Error>;

    /// Persist stage state under chain id `id`.
    ///
    /// When `filesystem_changed` is false, the rootfs is reused from `parent`.
    /// When true, `rootfs` is copied into the new entry.
    fn save(
        &self,
        id: &str,
        parent: &str,
        instruction: &str,
        meta: &ImageMeta,
        args: &HashMap<String, String>,
        rootfs: &Path,
        filesystem_changed: bool,
    ) -> Result<(), Error>;

    /// Remove all cached layers.
    fn clear(&self) -> Result<(), Error>;
}

/// Default cache handler for this host OS.
///
/// Selects [`crate::MacosCacheHandler`], [`crate::WindowsCacheHandler`], or
/// [`crate::LinuxCacheHandler`].
pub fn default_cache_handler(data_root: &Path) -> Result<Box<dyn CacheHandler>, Error> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(crate::cache::macos::MacosCacheHandler::open(
            data_root,
        )?))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(crate::cache::windows::WindowsCacheHandler::open(
            data_root,
        )?))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Ok(Box::new(crate::cache::linux::LinuxCacheHandler::open(
            data_root,
        )?))
    }
}
