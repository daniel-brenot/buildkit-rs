use std::fs;
use std::io::Read;
use std::io::{self};
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use flate2::read::GzDecoder;
use tar::Archive;
use tar::EntryType;
use crate::winpath;

const WHITEOUT_PREFIX: &str = ".wh.";
const WHITEOUT_OPAQUE: &str = ".wh..wh..opq";

/// Guest-visible ownership (host inode stays owned by the unpacking user).
#[cfg(unix)]
const XATTR_UID: &str = "user.buildkit.uid";
#[cfg(unix)]
const XATTR_GID: &str = "user.buildkit.gid";

#[cfg(unix)]
fn set_virtual_owner(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    setxattr_u32(path, XATTR_UID, uid)?;
    setxattr_u32(path, XATTR_GID, gid)?;
    Ok(())
}

#[cfg(unix)]
fn setxattr_u32(path: &Path, name: &str, value: u32) -> io::Result<()> {
    use std::ffi::CString;
    let path_c = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let name_c =
        CString::new(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let data = value.to_string();
    let rc = unsafe {
        // Linux: setxattr(path, name, value, size, flags)
        // macOS: setxattr(path, name, value, size, position, options)
        #[cfg(target_os = "macos")]
        {
            libc::setxattr(
                path_c.as_ptr(),
                name_c.as_ptr(),
                data.as_ptr() as *const libc::c_void,
                data.len(),
                0,
                0,
            )
        }
        #[cfg(not(target_os = "macos"))]
        {
            libc::setxattr(
                path_c.as_ptr(),
                name_c.as_ptr(),
                data.as_ptr() as *const libc::c_void,
                data.len(),
                0,
            )
        }
    };
    if rc != 0 {
        // macOS often rejects user.* xattrs on some volumes; ownership is
        // best-effort for the Linux ABI layer and must not fail unpack.
        #[cfg(target_os = "macos")]
        {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EPERM)
                || err.kind() == io::ErrorKind::PermissionDenied
                || err.raw_os_error() == Some(libc::EOPNOTSUPP)
            {
                return Ok(());
            }
            return Err(err);
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(io::Error::last_os_error())
        }
    } else {
        Ok(())
    }
}

/// Apply one gzip-compressed tar layer onto `rootfs`.
pub fn apply_layer(rootfs: &Path, layer: &[u8]) -> io::Result<()> {
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
                    clear_directory(&winpath::join_root(rootfs, &parent.to_string_lossy()))?;
                }
                continue;
            }
            if let Some(target) = name.strip_prefix(WHITEOUT_PREFIX) {
                let parent = winpath::join_root(
                    rootfs,
                    &path.parent().unwrap_or(Path::new(".")).to_string_lossy(),
                );
                remove_path(&{
                    let mut p = parent;
                    winpath::push_guest_rel(&mut p, target);
                    p
                })?;
                continue;
            }
        }
        unpack_entry(&mut entry, rootfs, &path, &mut pending_links)?;
    }

    // Symlinks often point at files from earlier in the same layer (busybox
    // applets). Resolve them after the layer is fully extracted.
    resolve_pending_links(rootfs, &pending_links)?;
    Ok(())
}

/// Resolve any leftover deferred links after all layers have been applied.
pub fn finalize_rootfs(rootfs: &Path) -> io::Result<()> {
    let mut pending = Vec::new();
    collect_sidecar_links(rootfs, rootfs, &mut pending)?;
    resolve_pending_links(rootfs, &pending)?;
    // Absolute symlinks resolve against the host root when opened by host path.
    crate::rootfs::rewrite_absolute_symlinks(rootfs)?;
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

fn clear_directory(dir: &Path) -> io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
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
        Ok(meta) => {
            if meta.is_dir() && !meta.file_type().is_symlink() {
                fs::remove_dir_all(path)
            } else {
                // Works even when the file mode is 044x (unlike File::create).
                fs::remove_file(path)
            }
        }
    }
}

fn unpack_entry<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    rootfs: &Path,
    path: &Path,
    pending_links: &mut Vec<(PathBuf, PathBuf)>,
) -> io::Result<()> {
    let dest = winpath::join_root(rootfs, &path.to_string_lossy());
    match entry.header().entry_type() {
        EntryType::Directory => {
            fs::create_dir_all(&dest)?;
            #[cfg(unix)]
            {
                let uid = entry.header().uid().unwrap_or(0) as u32;
                let gid = entry.header().gid().unwrap_or(0) as u32;
                let _ = set_virtual_owner(&dest, uid, gid);
            }
        }
        EntryType::Regular | EntryType::Continuous => {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            // Prior layers may have left a read-only file (e.g. mode 0440
            // sudoers). `File::create` cannot truncate those (EACCES); unlink
            // first — the directory is still writable by the unpacking user.
            remove_path(&dest)?;
            let mut file = fs::File::create(&dest)?;
            io::copy(entry, &mut file)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(mode) = entry.header().mode() {
                    let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(mode));
                }
                let uid = entry.header().uid().unwrap_or(0) as u32;
                let gid = entry.header().gid().unwrap_or(0) as u32;
                let _ = set_virtual_owner(&dest, uid, gid);
            }
        }
        EntryType::Symlink | EntryType::Link => {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            let link_target = entry.link_name()?.unwrap_or_default();
            if link_target.as_os_str().is_empty() {
                return Ok(());
            }
            remove_path(&dest)?;
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&link_target, &dest)?;
            }
            #[cfg(windows)]
            {
                if !try_windows_symlink(&dest, &link_target)? {
                    // Fall back: copy the target file (busybox applets) or
                    // defer until the target exists.
                    pending_links.push((dest, PathBuf::from(link_target.as_ref())));
                }
            }
            #[cfg(not(any(unix, windows)))]
            {
                pending_links.push((dest, PathBuf::from(link_target.as_ref())));
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(windows)]
fn try_windows_symlink(dest: &Path, link_target: &Path) -> io::Result<bool> {
    use std::os::windows::fs::symlink_dir;
    use std::os::windows::fs::symlink_file;
    let target = link_target.as_os_str().to_string_lossy();
    // Prefer directory symlink when the target (relative or absolute) is a dir
    // under common merged-/usr names, or ends with a separator.
    let as_dir = target.ends_with('/')
        || target.ends_with('\\')
        || dest
            .parent()
            .map(|p| p.join(link_target).is_dir())
            .unwrap_or(false);
    let result = if as_dir {
        symlink_dir(link_target, dest)
    } else {
        symlink_file(link_target, dest)
    };
    match result {
        Ok(()) => Ok(true),
        // ERROR_PRIVILEGE_NOT_HELD — need Developer Mode / SeCreateSymbolicLinkPrivilege.
        Err(err) if err.raw_os_error() == Some(1314) => Ok(false),
        // ERROR_INVALID_NAME — target/name has chars Windows rejects (e.g. ':').
        Err(err) if err.raw_os_error() == Some(123) => Ok(false),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(err) => Err(err),
    }
}

/// Create a Windows directory junction (no admin rights required).
#[cfg(windows)]
fn make_windows_junction(dest: &Path, target: &Path) -> io::Result<bool> {
    use std::os::windows::process::CommandExt;
    let mut cmd = std::process::Command::new("cmd");
    cmd.arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(dest.as_os_str())
        .arg(target.as_os_str())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // CREATE_NO_WINDOW — image unpack can create many junctions; without this
    // each `cmd /C mklink` flashes a console window.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
    let status = cmd.status()?;
    Ok(status.success())
}

fn resolve_pending_links(rootfs: &Path, pending: &[(PathBuf, PathBuf)]) -> io::Result<()> {
    // Multiple passes for chains (a -> b -> busybox).
    let mut remaining: Vec<(PathBuf, PathBuf)> = pending.to_vec();
    for _ in 0..8 {
        if remaining.is_empty() {
            break;
        }
        let mut next = Vec::new();
        for (dest, target) in remaining {
            let resolved = resolve_link_target(rootfs, &dest, &target);
            if dest.exists() {
                // Older unpackers left empty dirs for /bin → usr/bin style links.
                let empty_dir_placeholder = dest.is_dir()
                    && resolved.is_dir()
                    && fs::read_dir(&dest)
                        .map(|mut d| d.next().is_none())
                        .unwrap_or(false);
                if empty_dir_placeholder {
                    let _ = fs::remove_dir(&dest);
                } else {
                    continue;
                }
            }
            if resolved.is_file() {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(&resolved, &dest)?;
            } else if resolved.is_dir() {
                // Never create an empty placeholder directory for a symlink to a
                // directory (Debian merged-/usr uses /bin -> usr/bin). On Windows
                // without symlink privilege, fall back to a directory junction.
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                let _ = fs::remove_dir(&dest);
                #[cfg(windows)]
                {
                    if try_windows_symlink(&dest, &target)? {
                        continue;
                    }
                    if make_windows_junction(&dest, &resolved)? {
                        continue;
                    }
                    next.push((dest, target));
                }
                #[cfg(not(windows))]
                {
                    std::os::unix::fs::symlink(&target, &dest)?;
                }
            } else {
                next.push((dest, target));
            }
        }
        remaining = next;
    }
    // Persist unresolved links so finalize_rootfs can retry after later layers.
    for (dest, target) in remaining {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let sidecar = sidecar_path(&dest);
        fs::write(sidecar, target.to_string_lossy().as_bytes())?;
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

fn collect_sidecar_links(
    rootfs: &Path,
    dir: &Path,
    out: &mut Vec<(PathBuf, PathBuf)>,
) -> io::Result<()> {
    // Use symlink_metadata so absolute guest symlinks (e.g. lib/ssl/private ->
    // /etc/ssl/private) are not followed into the host filesystem.
    let meta = match fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if !meta.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_sidecar_links(rootfs, &path, out)?;
            continue;
        }
        if file_type.is_symlink() {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if let Some(stem) = name.strip_suffix(".buildkit-symlink") {
            let dest = path.with_file_name(stem);
            let target = PathBuf::from(fs::read_to_string(&path)?.trim());
            out.push((dest, target));
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
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
        apply_layer(&dir, &layer).unwrap();
        assert_eq!(fs::read_to_string(dir.join("hello.txt")).unwrap(), "hello");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn whiteout_removes_file() {
        let dir = std::env::temp_dir().join(format!("buildkit-whiteout-test-{}", std::process::id()));
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
        apply_layer(&dir, &layer).unwrap();
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
        apply_layer(&dir, &layer).unwrap();
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
        let dir = std::env::temp_dir().join(format!(
            "buildkit-finalize-abs-link-{}",
            std::process::id()
        ));
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
        fs::write(dir.join("pending.buildkit-symlink"), "etc/ssl/private/secret").unwrap();
        finalize_rootfs(&dir).unwrap();
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
        apply_layer(&dir, &layer).unwrap();
        #[cfg(windows)]
        {
            let enc = crate::winpath::encode_component("libapr1:amd64.list");
            let path = dir
                .join("var")
                .join("lib")
                .join("dpkg")
                .join("info")
                .join(enc.as_ref());
            assert!(path.is_file(), "expected encoded file at {}", path.display());
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
