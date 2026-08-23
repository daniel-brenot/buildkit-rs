//! Docker overlay2-style instruction cache.
//!
//! Each cached instruction is a layer directory with a `diff/` changeset, a
//! `lower` parent chain, and a `link` id — the same layout Docker's overlay2
//! graph driver uses. The merged view is applied in-process (no overlay mount).

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::export::ImageMeta;
use crate::fs::FileSystem;
use crate::Error;

const WHITEOUT_PREFIX: &str = ".wh.";
const WHITEOUT_OPAQUE: &str = ".wh..wh..opq";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    meta: ImageMeta,
    args: HashMap<String, String>,
    instruction: String,
    parent: String,
}

/// Docker overlay2 graph-driver cache.
///
/// Layers live under `<data_root>/overlay2/<id>/`, with `diff/`, `lower`,
/// `link`, and `committed` as in Docker.
#[derive(Debug, Clone)]
pub(crate) struct Overlay2 {
    root: PathBuf,
}

impl Overlay2 {
    /// Open (or create) the overlay2 cache under `<data_root>/overlay2`.
    pub(crate) fn open<F: FileSystem>(fs: &F, data_root: &Path) -> Result<Self, Error> {
        let root = data_root.join("overlay2");
        fs.create_dir_all(&root.join("l"))?;
        Ok(Self { root })
    }

    fn layer_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    fn diff_dir(&self, id: &str) -> PathBuf {
        self.layer_dir(id).join("diff")
    }

    fn merged_dir(&self, id: &str) -> PathBuf {
        self.layer_dir(id).join("merged")
    }

    fn short_id(id: &str) -> String {
        id.chars().take(12).collect()
    }

    fn read_entry<F: FileSystem>(&self, fs: &F, id: &str) -> Result<CacheEntry, Error> {
        let path = self.layer_dir(id).join("meta.json");
        let data = fs
            .read_to_string(&path)
            .map_err(|e| Error::other(format!("build cache read '{}': {e}", path.display())))?;
        serde_json::from_str(&data).map_err(|e| Error::other(format!("build cache meta: {e}")))
    }

    fn write_link<F: FileSystem>(&self, fs: &F, id: &str) -> Result<String, Error> {
        let short = Self::short_id(id);
        let link_path = self.root.join("l").join(&short);
        #[cfg(unix)]
        {
            let target = PathBuf::from("..").join(id);
            if fs.exists(&link_path) {
                let _ = fs.remove_file(&link_path);
            }
            fs.symlink(&target, &link_path)?;
        }
        #[cfg(windows)]
        {
            fs.write(&link_path, id.as_bytes())?;
        }
        fs.write(&self.layer_dir(id).join("link"), short.as_bytes())?;
        Ok(short)
    }

    fn parent_lower<F: FileSystem>(&self, fs: &F, parent: &str) -> Result<String, Error> {
        if parent.is_empty() {
            return Ok(String::new());
        }
        let parent_short = fs
            .read_to_string(&self.layer_dir(parent).join("link"))
            .unwrap_or_else(|_| Self::short_id(parent));
        let parent_lower = fs
            .read_to_string(&self.layer_dir(parent).join("lower"))
            .unwrap_or_default();
        let parent_lower = parent_lower.trim();
        if parent_lower.is_empty() {
            Ok(format!("l/{parent_short}"))
        } else {
            Ok(format!("l/{parent_short}:{parent_lower}"))
        }
    }

    fn chain_from_lower<F: FileSystem>(&self, fs: &F, id: &str) -> Result<Vec<PathBuf>, Error> {
        let mut diffs = Vec::new();
        let lower = fs
            .read_to_string(&self.layer_dir(id).join("lower"))
            .unwrap_or_default();
        let parts: Vec<&str> = lower.trim().split(':').filter(|s| !s.is_empty()).collect();
        for spec in parts.iter().rev() {
            diffs.push(self.resolve_lower_diff(fs, spec)?);
        }
        diffs.push(self.diff_dir(id));
        Ok(diffs)
    }

    fn resolve_lower_diff<F: FileSystem>(&self, fs: &F, spec: &str) -> Result<PathBuf, Error> {
        let link = self.root.join(spec);
        let layer = if fs.is_symlink(&link) {
            let target = fs.read_link(&link)?;
            self.root.join("l").join(target)
        } else if fs.is_file(&link) {
            let id = fs.read_to_string(&link)?;
            self.root.join(id.trim())
        } else {
            return Err(Error::other(format!(
                "build cache: missing overlay2 link '{spec}'"
            )));
        };
        let layer = if layer.ends_with("diff") {
            layer
        } else {
            dunce_layer(&layer)
        };
        Ok(layer.join("diff"))
    }

    pub(crate) fn overlay_root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn has_layer_blob<F: FileSystem>(&self, fs: &F, id: &str) -> bool {
        fs.is_file(&self.blob_path(id))
    }

    pub(crate) fn read_layer_blob<F: FileSystem>(
        &self,
        fs: &F,
        id: &str,
    ) -> Result<Vec<u8>, Error> {
        let path = self.blob_path(id);
        fs.read(&path)
            .map_err(|e| Error::other(format!("build cache layer blob '{}': {e}", path.display())))
    }

    pub(crate) fn layer_blob_digest<F: FileSystem>(
        &self,
        fs: &F,
        id: &str,
    ) -> Result<String, Error> {
        let path = self.layer_dir(id).join("layer.digest");
        if fs.is_file(&path) {
            return fs
                .read_to_string(&path)
                .map(|s| s.trim().to_string())
                .map_err(|e| Error::other(format!("build cache layer digest: {e}")));
        }
        let bytes = fs.read(&self.blob_path(id))?;
        let digest = format!("sha256:{}", crate::export::layer_digest(&bytes));
        let _ = fs.write(&path, digest.as_bytes());
        Ok(digest)
    }

    pub(crate) fn write_layer_blob<F: FileSystem>(
        &self,
        fs: &F,
        id: &str,
        bytes: &[u8],
    ) -> Result<(), Error> {
        let dir = self.layer_dir(id);
        fs.create_dir_all(&dir)?;
        fs.write(&self.blob_path(id), bytes)?;
        let digest = format!("sha256:{}", crate::export::layer_digest(bytes));
        fs.write(&dir.join("layer.digest"), digest.as_bytes())?;
        Ok(())
    }

    pub(crate) fn has_id<F: FileSystem>(&self, fs: &F, id: &str) -> bool {
        fs.is_file(&self.layer_dir(id).join("committed"))
            && fs.is_file(&self.layer_dir(id).join("meta.json"))
    }

    pub(crate) fn load_meta_id<F: FileSystem>(
        &self,
        fs: &F,
        id: &str,
    ) -> Result<(ImageMeta, HashMap<String, String>), Error> {
        let entry = self.read_entry(fs, id)?;
        Ok((entry.meta, entry.args))
    }

    pub(crate) fn resolve_id<F: FileSystem>(&self, fs: &F, id: &str) -> Result<PathBuf, Error> {
        let merged = self.merged_dir(id);
        if fs.is_dir(&merged) && fs.is_file(&self.layer_dir(id).join("committed")) {
            return Ok(merged);
        }
        if fs.exists(&merged) {
            fs.remove_dir_all(&merged)?;
        }
        fs.create_dir_all(&merged)?;
        for diff in self.chain_from_lower(fs, id)? {
            apply_diff(fs, &diff, &merged)?;
        }
        Ok(merged)
    }

    pub(crate) fn blob_path(&self, id: &str) -> PathBuf {
        self.layer_dir(id).join("layer.tar.gz")
    }

    pub(crate) fn save_layer<F: FileSystem>(
        &self,
        fs: &F,
        id: &str,
        parent: &str,
        instruction: &str,
        meta: &ImageMeta,
        args: &HashMap<String, String>,
        rootfs: &Path,
        filesystem_changed: bool,
    ) -> Result<(), Error> {
        let dir = self.layer_dir(id);
        let existing_blob = {
            let p = dir.join("layer.tar.gz");
            if fs.is_file(&p) {
                fs.read(&p).ok()
            } else {
                None
            }
        };

        if fs.exists(&dir) {
            fs.remove_dir_all(&dir)?;
        }
        fs.create_dir_all(&dir.join("diff"))?;

        let lower = self.parent_lower(fs, parent)?;
        if !lower.is_empty() {
            fs.write(&dir.join("lower"), lower.as_bytes())?;
        }
        self.write_link(fs, id)?;

        if filesystem_changed || parent.is_empty() {
            let parent_merged = if parent.is_empty() {
                None
            } else {
                Some(self.resolve_id(fs, parent)?)
            };
            write_layer_diff(fs, parent_merged.as_deref(), rootfs, &self.diff_dir(id))?;
        }

        let entry = CacheEntry {
            meta: meta.clone(),
            args: args.clone(),
            instruction: instruction.to_string(),
            parent: parent.to_string(),
        };
        fs.write(
            &dir.join("meta.json"),
            serde_json::to_string_pretty(&entry)?.as_bytes(),
        )?;
        if let Some(blob) = existing_blob {
            let _ = fs.write(&dir.join("layer.tar.gz"), &blob);
        }
        fs.write(&dir.join("committed"), b"")?;
        Ok(())
    }

    pub(crate) fn clear_all<F: FileSystem>(&self, fs: &F) -> Result<(), Error> {
        if fs.exists(&self.root) {
            fs.remove_dir_all(&self.root)?;
        }
        fs.create_dir_all(&self.root.join("l"))?;
        Ok(())
    }
}

fn dunce_layer(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                let _ = out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn write_layer_diff<F: FileSystem>(
    fs: &F,
    parent: Option<&Path>,
    current: &Path,
    diff: &Path,
) -> Result<(), Error> {
    fs.create_dir_all(diff)?;
    match parent {
        None => crate::fs::copy_tree(fs, current, diff),
        Some(parent) => {
            diff_walk(fs, parent, current, diff)?;
            Ok(())
        }
    }
}

fn diff_walk<F: FileSystem>(
    fs: &F,
    parent: &Path,
    current: &Path,
    diff: &Path,
) -> Result<(), Error> {
    let mut seen = HashSet::new();
    if fs.is_dir(current) {
        for entry in fs.read_dir(current)? {
            let name = entry.name;
            if is_whiteout_name(&name.to_string_lossy()) {
                continue;
            }
            seen.insert(name.clone());
            let c = current.join(&name);
            let p = parent.join(&name);
            let d = diff.join(&name);
            if !fs.exists(&p) {
                copy_one(fs, &c, &d)?;
                continue;
            }
            let c_dir = fs.is_dir(&c) && !fs.is_symlink(&c);
            let p_dir = fs.is_dir(&p) && !fs.is_symlink(&p);
            if c_dir && p_dir {
                diff_walk(fs, &p, &c, &d)?;
                prune_empty(fs, &d)?;
            } else if !same_entry(fs, &p, &c)? {
                copy_one(fs, &c, &d)?;
            }
        }
    }
    if fs.is_dir(parent) {
        for entry in fs.read_dir(parent)? {
            let name = entry.name;
            if is_whiteout_name(&name.to_string_lossy()) {
                continue;
            }
            if !seen.contains(&name) {
                write_whiteout(fs, diff, &name.to_string_lossy())?;
            }
        }
    }
    Ok(())
}

fn prune_empty<F: FileSystem>(fs: &F, dir: &Path) -> Result<(), Error> {
    if fs.is_dir(dir) && fs.read_dir(dir).map(|d| d.is_empty()).unwrap_or(false) {
        fs.remove_dir(dir)?;
    }
    Ok(())
}

fn is_whiteout_name(name: &str) -> bool {
    name == WHITEOUT_OPAQUE || name.starts_with(WHITEOUT_PREFIX)
}

fn same_entry<F: FileSystem>(fs: &F, a: &Path, b: &Path) -> Result<bool, Error> {
    let am = fs.symlink_metadata(a)?;
    let bm = fs.symlink_metadata(b)?;
    if am.is_symlink() || bm.is_symlink() {
        if !(am.is_symlink() && bm.is_symlink()) {
            return Ok(false);
        }
        return Ok(fs.read_link(a)? == fs.read_link(b)?);
    }
    if am.is_dir() || bm.is_dir() {
        return Ok(am.is_dir() && bm.is_dir());
    }
    if am.len() != bm.len() {
        return Ok(false);
    }
    Ok(fs.read(a)? == fs.read(b)?)
}

fn copy_one<F: FileSystem>(fs: &F, src: &Path, dest: &Path) -> Result<(), Error> {
    if let Some(parent) = dest.parent() {
        fs.create_dir_all(parent)?;
    }
    if fs.is_dir(src) && !fs.is_symlink(src) {
        crate::fs::copy_tree(fs, src, dest)
    } else if fs.is_symlink(src) {
        let target = fs.read_link(src)?;
        let _ = fs.remove_file(dest);
        fs.symlink(&target, dest)?;
        Ok(())
    } else {
        fs.copy(src, dest)?;
        Ok(())
    }
}

fn write_whiteout<F: FileSystem>(fs: &F, diff: &Path, name: &str) -> Result<(), Error> {
    fs.create_dir_all(diff)?;
    fs.write(&diff.join(format!("{WHITEOUT_PREFIX}{name}")), b"")?;
    Ok(())
}

fn apply_diff<F: FileSystem>(fs: &F, diff: &Path, dest: &Path) -> Result<(), Error> {
    if !fs.exists(diff) {
        return Ok(());
    }
    apply_diff_dir(fs, diff, dest)
}

fn apply_diff_dir<F: FileSystem>(fs: &F, diff: &Path, dest: &Path) -> Result<(), Error> {
    fs.create_dir_all(dest)?;
    for entry in fs.read_dir(diff)? {
        let name = entry.name;
        let ns = name.to_string_lossy();
        if ns == WHITEOUT_OPAQUE {
            clear_directory(fs, dest)?;
            continue;
        }
        if let Some(target) = ns.strip_prefix(WHITEOUT_PREFIX) {
            fs.remove(&dest.join(target))?;
            continue;
        }
        let from = diff.join(&name);
        let to = dest.join(&name);
        if fs.is_dir(&from) && !fs.is_symlink(&from) {
            apply_diff_dir(fs, &from, &to)?;
        } else {
            copy_one(fs, &from, &to)?;
        }
    }
    Ok(())
}

fn clear_directory<F: FileSystem>(fs: &F, dir: &Path) -> Result<(), Error> {
    if !fs.is_dir(dir) {
        return Ok(());
    }
    for entry in fs.read_dir(dir)? {
        if entry.is_dir && !entry.is_symlink {
            fs.remove_dir_all(&entry.path)?;
        } else {
            fs.remove_file(&entry.path)?;
        }
    }
    Ok(())
}
