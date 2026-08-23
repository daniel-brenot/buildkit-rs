//! Rootfs helpers shared by image unpack and stage materialization.

use std::path::{Path, PathBuf};

use crate::fs::FileSystem;
use crate::winpath;
use crate::Error;

/// Convert absolute symlink targets into rootfs-relative ones.
///
/// OCI layers often contain links like `lib64/ld-linux.so.2` → `/lib/...`.
/// When a sandbox opens those by host path, the kernel resolves the absolute
/// target against the *host* root. Relative targets keep resolution inside
/// the rootfs.
pub fn rewrite_absolute_symlinks<F: FileSystem>(fs: &F, rootfs: &Path) -> Result<(), Error> {
    walk(fs, rootfs, rootfs)
}

fn walk<F: FileSystem>(fs: &F, rootfs: &Path, dir: &Path) -> Result<(), Error> {
    let entries = match fs.read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.is_not_found() => return Ok(()),
        Err(e) => return Err(e),
    };
    for ent in entries {
        let path = ent.path;
        if ent.is_symlink {
            rewrite_one(fs, rootfs, &path)?;
        } else if ent.is_dir {
            walk(fs, rootfs, &path)?;
        }
    }
    Ok(())
}

fn rewrite_one<F: FileSystem>(fs: &F, rootfs: &Path, path: &Path) -> Result<(), Error> {
    let target = fs.read_link(path)?;
    let t = target.to_string_lossy();
    if !t.starts_with('/') {
        return Ok(());
    }
    let dest = winpath::join_root(rootfs, &t);
    let Some(link_dir) = path.parent() else {
        return Ok(());
    };
    let Some(rel) = path_relative_to(link_dir, &dest) else {
        return Ok(());
    };
    fs.remove_file(path)?;
    let _ = fs.symlink(&rel, path);
    Ok(())
}

fn path_relative_to(from_dir: &Path, to: &Path) -> Option<PathBuf> {
    let from: Vec<_> = from_dir.components().collect();
    let to_comps: Vec<_> = to.components().collect();
    let mut i = 0;
    while i < from.len() && i < to_comps.len() && from[i] == to_comps[i] {
        i += 1;
    }
    let mut rel = PathBuf::new();
    for _ in i..from.len() {
        rel.push("..");
    }
    for c in &to_comps[i..] {
        rel.push(c);
    }
    if rel.as_os_str().is_empty() {
        rel.push(".");
    }
    Some(rel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn relative_basic() {
        let rel = path_relative_to(Path::new("/a/b/c"), Path::new("/a/b/d/e")).unwrap();
        assert_eq!(rel, PathBuf::from("../d/e"));
    }
}
