use std::path::{Path, PathBuf};

use crate::winpath;

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
