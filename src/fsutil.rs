use std::fs;
use std::path::{Path, PathBuf};

use crate::winpath;
use crate::Error;

/// Join a guest path onto `cwd`, then normalize `.` / `..`.
pub fn join_workdir(cwd: &str, path: &str) -> String {
    if path.starts_with('/') {
        return normalize_guest(path);
    }
    let base = if cwd.ends_with('/') {
        cwd.to_string()
    } else {
        format!("{cwd}/")
    };
    normalize_guest(&(base + path))
}

pub fn normalize_guest(path: &str) -> String {
    let mut out = String::from("/");
    for part in path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            if let Some(pos) = out.rfind('/') {
                if pos == 0 {
                    out.truncate(1);
                } else {
                    out.truncate(pos);
                }
            }
            continue;
        }
        if out != "/" {
            out.push('/');
        }
        out.push_str(part);
    }
    if out.is_empty() {
        "/".into()
    } else {
        out
    }
}

pub fn guest_to_host(rootfs: &Path, guest: &str) -> PathBuf {
    winpath::join_root(rootfs, guest.trim_start_matches('/'))
}

pub fn copy_tree(src: &Path, dest: &Path) -> Result<(), Error> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_tree(&from, &to)?;
        } else if ft.is_symlink() {
            let target = fs::read_link(&from)?;
            #[cfg(unix)]
            {
                let _ = fs::remove_file(&to);
                std::os::unix::fs::symlink(&target, &to)?;
            }
            #[cfg(windows)]
            {
                let _ = fs::remove_file(&to);
                if from.is_dir() {
                    let _ = std::os::windows::fs::symlink_dir(&target, &to);
                } else {
                    let _ = std::os::windows::fs::symlink_file(&target, &to);
                }
            }
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
