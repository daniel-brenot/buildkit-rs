//! Directory-snapshot cache used by the current OS default handlers.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::export::ImageMeta;
use crate::Error;

use super::handler::CacheHandler;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    meta: ImageMeta,
    args: HashMap<String, String>,
    /// When set, filesystem contents are reused from this cache id.
    rootfs_from: Option<String>,
    /// Debug / human-readable instruction key.
    instruction: String,
    parent: String,
}

/// On-disk snapshot cache under `<data_root>/cache/<id>/`.
#[derive(Debug, Clone)]
pub(crate) struct FsCacheHandler {
    root: PathBuf,
}

impl FsCacheHandler {
    pub(crate) fn open(data_root: &Path) -> Result<Self, Error> {
        let root = data_root.join("cache");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn has(&self, id: &str) -> bool {
        self.root.join(id).join("meta.json").is_file()
    }

    pub(crate) fn load_meta(
        &self,
        id: &str,
    ) -> Result<(ImageMeta, HashMap<String, String>), Error> {
        let entry = self.read_entry(id)?;
        Ok((entry.meta, entry.args))
    }

    pub(crate) fn resolve_rootfs(&self, id: &str) -> Result<PathBuf, Error> {
        let entry = self.read_entry(id)?;
        let src_id = entry.rootfs_from.as_deref().unwrap_or(id);
        let src_rootfs = self.root.join(src_id).join("rootfs");
        if !src_rootfs.is_dir() {
            return Err(Error::other(format!(
                "corrupt build cache entry '{id}': missing rootfs for '{src_id}'"
            )));
        }
        Ok(src_rootfs)
    }

    pub(crate) fn layer_blob_path(&self, id: &str) -> PathBuf {
        self.root.join(id).join("layer.tar.gz")
    }

    pub(crate) fn has_layer_blob(&self, id: &str) -> bool {
        self.layer_blob_path(id).is_file()
    }

    pub(crate) fn read_layer_blob(&self, id: &str) -> Result<Vec<u8>, Error> {
        let path = self.layer_blob_path(id);
        fs::read(&path)
            .map_err(|e| Error::other(format!("build cache layer blob '{}': {e}", path.display())))
    }

    pub(crate) fn layer_blob_digest(&self, id: &str) -> Result<String, Error> {
        let path = self.root.join(id).join("layer.digest");
        if path.is_file() {
            return fs::read_to_string(&path)
                .map(|s| s.trim().to_string())
                .map_err(|e| Error::other(format!("build cache layer digest: {e}")));
        }
        let bytes = self.read_layer_blob(id)?;
        let digest = format!("sha256:{}", crate::export::layer_digest(&bytes));
        let _ = fs::write(path, &digest);
        Ok(digest)
    }

    pub(crate) fn write_layer_blob(&self, id: &str, bytes: &[u8]) -> Result<(), Error> {
        let dir = self.root.join(id);
        fs::create_dir_all(&dir)?;
        fs::write(self.layer_blob_path(id), bytes)?;
        let digest = format!("sha256:{}", crate::export::layer_digest(bytes));
        fs::write(dir.join("layer.digest"), &digest)?;
        Ok(())
    }

    pub(crate) fn save(
        &self,
        id: &str,
        parent: &str,
        instruction: &str,
        meta: &ImageMeta,
        args: &HashMap<String, String>,
        rootfs: &Path,
        filesystem_changed: bool,
    ) -> Result<(), Error> {
        let dir = self.root.join(id);
        let existing_blob = {
            let p = dir.join("layer.tar.gz");
            if p.is_file() {
                fs::read(&p).ok()
            } else {
                None
            }
        };

        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(&dir)?;

        let rootfs_from = if filesystem_changed || parent.is_empty() {
            let dest = dir.join("rootfs");
            fs::create_dir_all(&dest)?;
            crate::fsutil::copy_tree(rootfs, &dest)?;
            None
        } else {
            let parent_entry = self.read_entry(parent)?;
            Some(
                parent_entry
                    .rootfs_from
                    .unwrap_or_else(|| parent.to_string()),
            )
        };

        let entry = CacheEntry {
            meta: meta.clone(),
            args: args.clone(),
            rootfs_from,
            instruction: instruction.to_string(),
            parent: parent.to_string(),
        };
        fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&entry)?)?;
        if let Some(blob) = existing_blob {
            let _ = fs::write(dir.join("layer.tar.gz"), blob);
        }
        Ok(())
    }

    fn read_entry(&self, id: &str) -> Result<CacheEntry, Error> {
        let path = self.root.join(id).join("meta.json");
        let data = fs::read_to_string(&path)
            .map_err(|e| Error::other(format!("build cache read '{}': {e}", path.display())))?;
        serde_json::from_str(&data).map_err(|e| Error::other(format!("build cache meta: {e}")))
    }

    pub(crate) fn clear(&self) -> Result<(), Error> {
        if self.root.exists() {
            fs::remove_dir_all(&self.root)?;
        }
        fs::create_dir_all(&self.root)?;
        Ok(())
    }
}

/// Implemented by the per-OS default handlers that still use directory snapshots.
pub(crate) trait FsBackend: Send + Sync + std::fmt::Debug {
    fn fs(&self) -> &FsCacheHandler;
}

impl<T: FsBackend> CacheHandler for T {
    fn root(&self) -> &Path {
        self.fs().root()
    }

    fn has(&self, id: &str) -> bool {
        self.fs().has(id)
    }

    fn load_meta(&self, id: &str) -> Result<(ImageMeta, HashMap<String, String>), Error> {
        self.fs().load_meta(id)
    }

    fn resolve_rootfs(&self, id: &str) -> Result<PathBuf, Error> {
        self.fs().resolve_rootfs(id)
    }

    fn layer_blob_path(&self, id: &str) -> Option<PathBuf> {
        Some(self.fs().layer_blob_path(id))
    }

    fn has_layer_blob(&self, id: &str) -> bool {
        self.fs().has_layer_blob(id)
    }

    fn read_layer_blob(&self, id: &str) -> Result<Vec<u8>, Error> {
        self.fs().read_layer_blob(id)
    }

    fn layer_blob_digest(&self, id: &str) -> Result<String, Error> {
        self.fs().layer_blob_digest(id)
    }

    fn write_layer_blob(&self, id: &str, bytes: &[u8]) -> Result<(), Error> {
        self.fs().write_layer_blob(id, bytes)
    }

    fn save(
        &self,
        id: &str,
        parent: &str,
        instruction: &str,
        meta: &ImageMeta,
        args: &HashMap<String, String>,
        rootfs: &Path,
        filesystem_changed: bool,
    ) -> Result<(), Error> {
        self.fs()
            .save(id, parent, instruction, meta, args, rootfs, filesystem_changed)
    }

    fn clear(&self) -> Result<(), Error> {
        self.fs().clear()
    }
}
