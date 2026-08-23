use std::io::Read;
use std::io::{self};
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use crate::fs::FileSystem;
use crate::winpath;
use crate::Error;
use flate2::read::GzDecoder;
use tar::Archive;
use tar::EntryType;

const WHITEOUT_PREFIX: &str = ".wh.";
const WHITEOUT_OPAQUE: &str = ".wh..wh..opq";

/// Apply one gzip-compressed tar layer onto `rootfs`.
pub fn apply_layer<F: FileSystem>(fs: &F, rootfs: &Path, layer: &[u8]) -> Result<(), Error> {
    let decoder = GzDecoder::new(layer);
    let mut archive = Archive::new(decoder);
    let mut pending_links: Vec<(PathBuf, PathBuf)> = Vec::new();

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if !path_is_safe(&path) {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name == WHITEOUT_OPAQUE {
                if let Some(parent) = path.parent() {
                    clear_directory(fs, &winpath::join_root(rootfs, &parent.to_string_lossy()))?;
                }
                continue;
            }
            if let Some(target) = name.strip_prefix(WHITEOUT_PREFIX) {
                let parent = winpath::join_root(
                    rootfs,
                    &path.parent().unwrap_or(Path::new(".")).to_string_lossy(),
                );
                let mut p = parent;
                winpath::push_guest_rel(&mut p, target);
                fs.remove(&p)?;
                continue;
            }
        }
        unpack_entry(fs, &mut entry, rootfs, &path, &mut pending_links)?;
    }

    resolve_pending_links(fs, rootfs, &pending_links)?;
    Ok(())
}

/// Resolve any leftover deferred links after all layers have been applied.
pub fn finalize_rootfs<F: FileSystem>(fs: &F, rootfs: &Path) -> Result<(), Error> {
    let mut pending = Vec::new();
    collect_sidecar_links(fs, rootfs, &mut pending)?;
    resolve_pending_links(fs, rootfs, &pending)?;
    crate::rootfs::rewrite_absolute_symlinks(fs, rootfs)?;
    Ok(())
}

fn path_is_safe(path: &Path) -> bool {
    !path.is_absolute()
        && path.components().all(|c| {
            !matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
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

fn unpack_entry<F: FileSystem, R: Read>(
    fs: &F,
    entry: &mut tar::Entry<'_, R>,
    rootfs: &Path,
    path: &Path,
    pending_links: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), Error> {
    let dest = winpath::join_root(rootfs, &path.to_string_lossy());
    match entry.header().entry_type() {
        EntryType::Directory => {
            fs.create_dir_all(&dest)?;
            #[cfg(unix)]
            {
                let uid = entry.header().uid().unwrap_or(0) as u32;
                let gid = entry.header().gid().unwrap_or(0) as u32;
                let _ = fs.set_virtual_owner(&dest, uid, gid);
            }
        }
        EntryType::Regular | EntryType::Continuous => {
            if let Some(parent) = dest.parent() {
                fs.create_dir_all(parent)?;
            }
            fs.remove(&dest)?;
            let mut file = fs.create_file(&dest)?;
            io::copy(entry, &mut file)?;
            #[cfg(unix)]
            {
                if let Ok(mode) = entry.header().mode() {
                    let _ = fs.set_permissions(&dest, mode);
                }
                let uid = entry.header().uid().unwrap_or(0) as u32;
                let gid = entry.header().gid().unwrap_or(0) as u32;
                let _ = fs.set_virtual_owner(&dest, uid, gid);
            }
        }
        EntryType::Symlink | EntryType::Link => {
            if let Some(parent) = dest.parent() {
                fs.create_dir_all(parent)?;
            }
            let link_target = entry.link_name()?.unwrap_or_default();
            if link_target.as_os_str().is_empty() {
                return Ok(());
            }
            fs.remove(&dest)?;
            if !try_symlink(fs, &dest, link_target.as_ref())? {
                pending_links.push((dest, PathBuf::from(link_target.as_ref())));
            }
        }
        _ => {}
    }
    Ok(())
}

fn try_symlink<F: FileSystem>(fs: &F, dest: &Path, target: &Path) -> Result<bool, Error> {
    match fs.symlink(target, dest) {
        Ok(()) => Ok(true),
        Err(e) => {
            // ERROR_PRIVILEGE_NOT_HELD / ERROR_INVALID_NAME / already exists
            if e.raw_os_error() == Some(1314)
                || e.raw_os_error() == Some(123)
                || matches!(&e, Error::Io { source, .. } if source.kind() == io::ErrorKind::AlreadyExists)
            {
                Ok(false)
            } else {
                Err(e)
            }
        }
    }
}

fn resolve_pending_links<F: FileSystem>(
    fs: &F,
    rootfs: &Path,
    pending: &[(PathBuf, PathBuf)],
) -> Result<(), Error> {
    let mut remaining: Vec<(PathBuf, PathBuf)> = pending.to_vec();
    for _ in 0..8 {
        if remaining.is_empty() {
            break;
        }
        let mut next = Vec::new();
        for (dest, target) in remaining {
            let resolved = resolve_link_target(rootfs, &dest, &target);
            if fs.exists(&dest) {
                let empty_dir_placeholder = fs.is_dir(&dest)
                    && fs.is_dir(&resolved)
                    && fs.read_dir(&dest).map(|d| d.is_empty()).unwrap_or(false);
                if empty_dir_placeholder {
                    let _ = fs.remove_dir(&dest);
                } else {
                    continue;
                }
            }
            if fs.is_file(&resolved) {
                if let Some(parent) = dest.parent() {
                    fs.create_dir_all(parent)?;
                }
                fs.copy(&resolved, &dest)?;
            } else if fs.is_dir(&resolved) {
                if let Some(parent) = dest.parent() {
                    fs.create_dir_all(parent)?;
                }
                let _ = fs.remove_dir(&dest);
                if try_symlink(fs, &dest, &target)? {
                    continue;
                }
                if fs.junction(&resolved, &dest)? {
                    continue;
                }
                #[cfg(not(windows))]
                {
                    fs.symlink(&target, &dest)?;
                    continue;
                }
                #[cfg(windows)]
                {
                    next.push((dest, target));
                }
            } else {
                next.push((dest, target));
            }
        }
        remaining = next;
    }
    for (dest, target) in remaining {
        if let Some(parent) = dest.parent() {
            fs.create_dir_all(parent)?;
        }
        let sidecar = sidecar_path(&dest);
        fs.write(&sidecar, target.to_string_lossy().as_bytes())?;
    }
    Ok(())
}

fn resolve_link_target(rootfs: &Path, dest: &Path, target: &Path) -> PathBuf {
    let t = target.to_string_lossy();
    // Linux absolute paths like `/bin/busybox` are not `Path::is_absolute()` on
    // Windows (no drive letter), so detect the leading slash explicitly.
    if t.starts_with('/') || t.starts_with('\\') || target.is_absolute() {
        let stripped = t.trim_start_matches(['/', '\\']);
        winpath::join_root(rootfs, stripped)
    } else if let Some(parent) = dest.parent() {
        let mut out = parent.to_path_buf();
        winpath::push_guest_rel(&mut out, &t);
        out
    } else {
        winpath::join_root(rootfs, &t)
    }
}

fn sidecar_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(".buildkit-symlink");
    PathBuf::from(s)
}

fn collect_sidecar_links<F: FileSystem>(
    fs: &F,
    dir: &Path,
    out: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), Error> {
    let meta = match fs.symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.is_not_found() => return Ok(()),
        Err(e) => return Err(e),
    };
    if !meta.is_dir() {
        return Ok(());
    }
    for entry in fs.read_dir(dir)? {
        let path = entry.path;
        if entry.is_dir {
            collect_sidecar_links(fs, &path, out)?;
            continue;
        }
        if entry.is_symlink {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if let Some(stem) = name.strip_suffix(".buildkit-symlink") {
            let dest = path.with_file_name(stem);
            let target = PathBuf::from(fs.read_to_string(&path)?.trim());
            out.push((dest, target));
            let _ = fs.remove_file(&path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::fs;
    use std::io::Write;
    use tar::Builder;

    fn gzip_tar<F: FnOnce(&mut Builder<Vec<u8>>)>(build: F) -> Vec<u8> {
        let mut tar = Builder::new(Vec::new());
        build(&mut tar);
        let inner = tar.into_inner().unwrap();
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&inner).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn applies_regular_file() {
        let layer = gzip_tar(|tar| {
            let data = b"hello".as_slice();
            let mut header = tar::Header::new_gnu();
            header.set_path("hello.txt").unwrap();
            header.set_size(5);
            header.set_cksum();
            tar.append(&header, data).unwrap();
        });
        let dir = std::env::temp_dir().join(format!("buildkit-unpack-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        apply_layer(&crate::fs::LocalFs, &dir, &layer).unwrap();
        assert_eq!(fs::read_to_string(dir.join("hello.txt")).unwrap(), "hello");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn whiteout_removes_file() {
        let dir =
            std::env::temp_dir().join(format!("buildkit-whiteout-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let layer = gzip_tar(|tar| {
            let mut header = tar::Header::new_gnu();
            header.set_path("dir/.wh.remove-me").unwrap();
            header.set_size(0);
            header.set_cksum();
            tar.append(&header, &[] as &[u8]).unwrap();
        });
        fs::create_dir_all(dir.join("dir")).unwrap();
        fs::write(dir.join("dir/remove-me"), "x").unwrap();
        apply_layer(&crate::fs::LocalFs, &dir, &layer).unwrap();
        assert!(!dir.join("dir/remove-me").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn symlink_falls_back_to_copy() {
        let layer = gzip_tar(|tar| {
            let data = b"busybox".as_slice();
            let mut header = tar::Header::new_gnu();
            header.set_path("bin/busybox").unwrap();
            header.set_size(7);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append(&header, data).unwrap();

            let mut link = tar::Header::new_gnu();
            link.set_path("bin/sh").unwrap();
            link.set_entry_type(EntryType::Symlink);
            link.set_size(0);
            link.set_link_name("busybox").unwrap();
            link.set_cksum();
            tar.append(&link, &[] as &[u8]).unwrap();
        });
        let dir = std::env::temp_dir().join(format!("buildkit-link-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        apply_layer(&crate::fs::LocalFs, &dir, &layer).unwrap();
        assert!(dir.join("bin/busybox").is_file());
        assert!(
            dir.join("bin/sh").is_file() || dir.join("bin/sh").is_symlink(),
            "bin/sh should exist as symlink or copied file"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn finalize_skips_absolute_symlinks() {
        // Images like nginx ship absolute symlinks (lib/ssl/private -> /etc/ssl/private).
        // Walking those with Path::is_dir would escape the rootfs onto the host.
        let dir =
            std::env::temp_dir().join(format!("buildkit-finalize-abs-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("etc/ssl/private")).unwrap();
        fs::create_dir_all(dir.join("lib/ssl")).unwrap();
        fs::write(dir.join("etc/ssl/private/secret"), "x").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/ssl/private", dir.join("lib/ssl/private")).unwrap();
        }
        #[cfg(windows)]
        {
            // Absolute POSIX paths aren't meaningful on Windows; still verify
            // finalize does not recurse through directory symlinks.
            let _ = std::os::windows::fs::symlink_dir(
                dir.join("etc/ssl/private"),
                dir.join("lib/ssl/private"),
            );
        }
        fs::write(
            dir.join("pending.buildkit-symlink"),
            "etc/ssl/private/secret",
        )
        .unwrap();
        finalize_rootfs(&crate::fs::LocalFs, &dir).unwrap();
        assert!(
            !dir.join("pending.buildkit-symlink").exists(),
            "sidecar should be consumed"
        );
        assert!(
            dir.join("pending").is_file(),
            "deferred link should resolve inside the rootfs"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn applies_windows_illegal_colon_name() {
        let layer = gzip_tar(|tar| {
            let data = b"ok".as_slice();
            let mut header = tar::Header::new_gnu();
            header
                .set_path("var/lib/dpkg/info/libapr1:amd64.list")
                .unwrap();
            header.set_size(2);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, data).unwrap();
        });
        let dir = std::env::temp_dir().join(format!("buildkit-colon-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        apply_layer(&crate::fs::LocalFs, &dir, &layer).unwrap();
        #[cfg(windows)]
        {
            let enc = crate::winpath::encode_component("libapr1:amd64.list");
            let path = dir
                .join("var")
                .join("lib")
                .join("dpkg")
                .join("info")
                .join(enc.as_ref());
            assert!(
                path.is_file(),
                "expected encoded file at {}",
                path.display()
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), "ok");
        }
        #[cfg(not(windows))]
        {
            assert_eq!(
                fs::read_to_string(dir.join("var/lib/dpkg/info/libapr1:amd64.list")).unwrap(),
                "ok"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
