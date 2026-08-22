//! Rootfs helpers shared by image unpack and stage materialization.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::winpath;

/// Convert absolute symlink targets into rootfs-relative ones.
///
/// OCI layers often contain links like `lib64/ld-linux.so.2` → `/lib/...`.
/// When a sandbox opens those by host path, the kernel resolves the absolute
/// target against the *host* root. Relative targets keep resolution inside
/// the rootfs.
pub fn rewrite_absolute_symlinks(rootfs: &Path) -> io::Result<()> {
    walk(rootfs, rootfs)
}

fn walk(rootfs: &Path, dir: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for ent in entries {
        let ent = ent?;
        let path = ent.path();
        let ft = ent.file_type()?;
        if ft.is_symlink() {
            rewrite_one(rootfs, &path)?;
        } else if ft.is_dir() {
            walk(rootfs, &path)?;
        }
    }
    Ok(())
}

fn rewrite_one(rootfs: &Path, path: &Path) -> io::Result<()> {
    let target = fs::read_link(path)?;
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
    fs::remove_file(path)?;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&rel, path)?;
    }
    #[cfg(windows)]
    {
        let _ = std::os::windows::fs::symlink_file(&rel, path)
            .or_else(|_| std::os::windows::fs::symlink_dir(&rel, path));
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (rel, path);
    }
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

    #[test]
    fn relative_basic() {
        let rel = path_relative_to(Path::new("/a/b/c"), Path::new("/a/b/d/e")).unwrap();
        assert_eq!(rel, PathBuf::from("../d/e"));
    }
}
