//! Docker overlay2-style instruction cache.
//!
//! Each cached instruction is a layer directory with a `diff/` changeset, a
//! `lower` parent chain, and a `link` id — the same layout Docker's overlay2
//! graph driver uses. The merged view is applied in-process (no overlay mount).

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::export::ImageMeta;
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
    pub(crate) fn open(data_root: &Path) -> Result<Self, Error> {
        let root = data_root.join("overlay2");
        fs::create_dir_all(root.join("l"))?;
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

    fn read_entry(&self, id: &str) -> Result<CacheEntry, Error> {
        let path = self.layer_dir(id).join("meta.json");
        let data = fs::read_to_string(&path)
            .map_err(|e| Error::other(format!("build cache read '{}': {e}", path.display())))?;
        serde_json::from_str(&data).map_err(|e| Error::other(format!("build cache meta: {e}")))
    }

    fn write_link(&self, id: &str) -> Result<String, Error> {
        let short = Self::short_id(id);
        let link_path = self.root.join("l").join(&short);
        #[cfg(unix)]
        {
            let target = PathBuf::from("..").join(id);
            if link_path.exists() {
                let _ = fs::remove_file(&link_path);
            }
            std::os::unix::fs::symlink(&target, &link_path)?;
        }
        #[cfg(windows)]
        {
            fs::write(&link_path, id.as_bytes())?;
        }
        fs::write(self.layer_dir(id).join("link"), short.as_bytes())?;
        Ok(short)
    }

    fn parent_lower(&self, parent: &str) -> Result<String, Error> {
        if parent.is_empty() {
            return Ok(String::new());
        }
        let parent_short = fs::read_to_string(self.layer_dir(parent).join("link"))
            .unwrap_or_else(|_| Self::short_id(parent));
        let parent_lower =
            fs::read_to_string(self.layer_dir(parent).join("lower")).unwrap_or_default();
        let parent_lower = parent_lower.trim();
        if parent_lower.is_empty() {
            Ok(format!("l/{parent_short}"))
        } else {
            Ok(format!("l/{parent_short}:{parent_lower}"))
        }
    }

    fn chain_from_lower(&self, id: &str) -> Result<Vec<PathBuf>, Error> {
        let mut diffs = Vec::new();
        let lower = fs::read_to_string(self.layer_dir(id).join("lower")).unwrap_or_default();
        let parts: Vec<&str> = lower.trim().split(':').filter(|s| !s.is_empty()).collect();
        // Docker `lower` lists nearest parent first; overlay applies lowest last.
        for spec in parts.iter().rev() {
            diffs.push(self.resolve_lower_diff(spec)?);
        }
        diffs.push(self.diff_dir(id));
        Ok(diffs)
    }

    fn resolve_lower_diff(&self, spec: &str) -> Result<PathBuf, Error> {
        let link = self.root.join(spec);
        let layer = if link.is_symlink() {
            let target = fs::read_link(&link)?;
            self.root.join("l").join(target)
        } else if link.is_file() {
            let id = fs::read_to_string(&link)?;
            self.root.join(id.trim())
        } else {
            return Err(Error::other(format!(
                "build cache: missing overlay2 link '{spec}'"
            )));
        };
        // unix symlink is `../<id>` from `l/`, so join("l", "../id") works
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

    pub(crate) fn has_layer_blob(&self, id: &str) -> bool {
        self.blob_path(id).is_file()
    }

    pub(crate) fn read_layer_blob(&self, id: &str) -> Result<Vec<u8>, Error> {
        let path = self.blob_path(id);
        fs::read(&path)
            .map_err(|e| Error::other(format!("build cache layer blob '{}': {e}", path.display())))
    }

    pub(crate) fn layer_blob_digest(&self, id: &str) -> Result<String, Error> {
        let path = self.layer_dir(id).join("layer.digest");
        if path.is_file() {
            return fs::read_to_string(&path)
                .map(|s| s.trim().to_string())
                .map_err(|e| Error::other(format!("build cache layer digest: {e}")));
        }
        let bytes = fs::read(self.blob_path(id))?;
        let digest = format!("sha256:{}", crate::export::layer_digest(&bytes));
        let _ = fs::write(path, &digest);
        Ok(digest)
    }

    pub(crate) fn write_layer_blob(&self, id: &str, bytes: &[u8]) -> Result<(), Error> {
        let dir = self.layer_dir(id);
        fs::create_dir_all(&dir)?;
        fs::write(self.blob_path(id), bytes)?;
        let digest = format!("sha256:{}", crate::export::layer_digest(bytes));
        fs::write(dir.join("layer.digest"), &digest)?;
        Ok(())
    }

    pub(crate) fn has_id(&self, id: &str) -> bool {
        self.layer_dir(id).join("committed").is_file()
            && self.layer_dir(id).join("meta.json").is_file()
    }

    pub(crate) fn load_meta_id(
        &self,
        id: &str,
    ) -> Result<(ImageMeta, HashMap<String, String>), Error> {
        let entry = self.read_entry(id)?;
        Ok((entry.meta, entry.args))
    }

    pub(crate) fn resolve_id(&self, id: &str) -> Result<PathBuf, Error> {
        let merged = self.merged_dir(id);
        if merged.is_dir() && self.layer_dir(id).join("committed").is_file() {
            return Ok(merged);
        }
        if merged.exists() {
            fs::remove_dir_all(&merged)?;
        }
        fs::create_dir_all(&merged)?;
        for diff in self.chain_from_lower(id)? {
            apply_diff(&diff, &merged)?;
        }
        Ok(merged)
    }

    pub(crate) fn blob_path(&self, id: &str) -> PathBuf {
        self.layer_dir(id).join("layer.tar.gz")
    }

    pub(crate) fn save_layer(
        &self,
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
            if p.is_file() {
                fs::read(&p).ok()
            } else {
                None
            }
        };

        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(dir.join("diff"))?;

        let lower = self.parent_lower(parent)?;
        if !lower.is_empty() {
            fs::write(dir.join("lower"), lower.as_bytes())?;
        }
        self.write_link(id)?;

        if filesystem_changed || parent.is_empty() {
            let parent_merged = if parent.is_empty() {
                None
            } else {
                Some(self.resolve_id(parent)?)
            };
            write_layer_diff(parent_merged.as_deref(), rootfs, &self.diff_dir(id))?;
        }

        let entry = CacheEntry {
            meta: meta.clone(),
            args: args.clone(),
            instruction: instruction.to_string(),
            parent: parent.to_string(),
        };
        fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&entry)?)?;
        if let Some(blob) = existing_blob {
            let _ = fs::write(dir.join("layer.tar.gz"), blob);
        }
        fs::write(dir.join("committed"), b"")?;
        Ok(())
    }

    pub(crate) fn clear_all(&self) -> Result<(), Error> {
        if self.root.exists() {
            fs::remove_dir_all(&self.root)?;
        }
        fs::create_dir_all(self.root.join("l"))?;
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

fn write_layer_diff(parent: Option<&Path>, current: &Path, diff: &Path) -> Result<(), Error> {
    fs::create_dir_all(diff)?;
    match parent {
        None => crate::fsutil::copy_tree(current, diff),
        Some(parent) => {
            diff_walk(parent, current, diff)?;
            Ok(())
        }
    }
}

fn diff_walk(parent: &Path, current: &Path, diff: &Path) -> Result<(), Error> {
    let mut seen = HashSet::new();
    if current.is_dir() {
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            let name = entry.file_name();
            if is_whiteout_name(&name.to_string_lossy()) {
                continue;
            }
            seen.insert(name.clone());
            let c = current.join(&name);
            let p = parent.join(&name);
            let d = diff.join(&name);
            if !p.exists() {
                copy_one(&c, &d)?;
                continue;
            }
            let c_dir = c.is_dir() && !is_symlink(&c);
            let p_dir = p.is_dir() && !is_symlink(&p);
            if c_dir && p_dir {
                diff_walk(&p, &c, &d)?;
                prune_empty(&d)?;
            } else if !same_entry(&p, &c)? {
                copy_one(&c, &d)?;
            }
        }
    }
    if parent.is_dir() {
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            let name = entry.file_name();
            if is_whiteout_name(&name.to_string_lossy()) {
                continue;
            }
            if !seen.contains(&name) {
                write_whiteout(diff, &name.to_string_lossy())?;
            }
        }
    }
    Ok(())
}

fn prune_empty(dir: &Path) -> io::Result<()> {
    if dir.is_dir() && fs::read_dir(dir)?.next().is_none() {
        fs::remove_dir(dir)?;
    }
    Ok(())
}

fn is_whiteout_name(name: &str) -> bool {
    name == WHITEOUT_OPAQUE || name.starts_with(WHITEOUT_PREFIX)
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn same_entry(a: &Path, b: &Path) -> Result<bool, Error> {
    let am = fs::symlink_metadata(a)?;
    let bm = fs::symlink_metadata(b)?;
    if am.file_type().is_symlink() || bm.file_type().is_symlink() {
        if !(am.file_type().is_symlink() && bm.file_type().is_symlink()) {
            return Ok(false);
        }
        return Ok(fs::read_link(a)? == fs::read_link(b)?);
    }
    if am.is_dir() || bm.is_dir() {
        return Ok(am.is_dir() && bm.is_dir());
    }
    if am.len() != bm.len() {
        return Ok(false);
    }
    Ok(fs::read(a)? == fs::read(b)?)
}

fn copy_one(src: &Path, dest: &Path) -> Result<(), Error> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if src.is_dir() && !is_symlink(src) {
        crate::fsutil::copy_tree(src, dest)
    } else if is_symlink(src) {
        let target = fs::read_link(src)?;
        let _ = fs::remove_file(dest);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, dest)?;
        }
        #[cfg(windows)]
        {
            if src.is_dir() {
                let _ = std::os::windows::fs::symlink_dir(&target, dest);
            } else {
                let _ = std::os::windows::fs::symlink_file(&target, dest);
            }
        }
        Ok(())
    } else {
        fs::copy(src, dest)?;
        Ok(())
    }
}

fn write_whiteout(diff: &Path, name: &str) -> Result<(), Error> {
    fs::create_dir_all(diff)?;
    fs::write(diff.join(format!("{WHITEOUT_PREFIX}{name}")), b"")?;
    Ok(())
}

fn apply_diff(diff: &Path, dest: &Path) -> Result<(), Error> {
    if !diff.exists() {
        return Ok(());
    }
    apply_diff_dir(diff, dest)
}

fn apply_diff_dir(diff: &Path, dest: &Path) -> Result<(), Error> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(diff)? {
        let entry = entry?;
        let name = entry.file_name();
        let ns = name.to_string_lossy();
        if ns == WHITEOUT_OPAQUE {
            clear_directory(dest)?;
            continue;
        }
        if let Some(target) = ns.strip_prefix(WHITEOUT_PREFIX) {
            remove_path(&dest.join(target))?;
            continue;
        }
        let from = diff.join(&name);
        let to = dest.join(&name);
        if from.is_dir() && !is_symlink(&from) {
            apply_diff_dir(&from, &to)?;
        } else {
            copy_one(&from, &to)?;
        }
    }
    Ok(())
}

fn clear_directory(dir: &Path) -> io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() && !is_symlink(&path) {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn remove_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
    }
}
