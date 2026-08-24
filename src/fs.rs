//! Pluggable filesystem used for every build, cache, and unpack I/O.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::Error;

/// Metadata for one path, from [`FileSystem::metadata`] or
/// [`FileSystem::symlink_metadata`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsMetadata {
    is_dir: bool,
    is_file: bool,
    is_symlink: bool,
    len: u64,
}

impl FsMetadata {
    /// Directory (not a symlink to a directory, unless metadata followed it).
    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    /// Regular file.
    pub fn is_file(&self) -> bool {
        self.is_file
    }

    /// Symbolic link. Only meaningful for [`FileSystem::symlink_metadata`].
    pub fn is_symlink(&self) -> bool {
        self.is_symlink
    }

    /// Size in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether [`Self::len`] is zero.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// One entry from [`FileSystem::read_dir`].
#[derive(Debug, Clone)]
pub struct FsDirEntry {
    /// File name within the parent directory.
    pub name: OsString,
    /// Full path of the entry.
    pub path: PathBuf,
    /// Directory entry (does not follow symlinks).
    pub is_dir: bool,
    /// Regular file (does not follow symlinks).
    pub is_file: bool,
    /// Symbolic link.
    pub is_symlink: bool,
}

/// How this crate creates, reads, and deletes files.
///
/// Implement these methods to control every filesystem operation during a
/// build (image blobs, overlay2 cache, stage rootfs, `COPY` / `ADD`, unpack,
/// export). [`crate::ImageStore`] uses this trait for I/O; blob layout stays
/// fixed. [`LocalFs`] is the default: the host `std::fs`.
pub trait FileSystem: Send + Sync {
    /// Create `path` and any missing parents.
    fn create_dir_all(&self, path: &Path) -> Result<(), Error>;

    /// Remove a directory tree. Error if `path` does not exist.
    fn remove_dir_all(&self, path: &Path) -> Result<(), Error>;

    /// Remove an empty directory.
    fn remove_dir(&self, path: &Path) -> Result<(), Error>;

    /// Remove a file or symlink.
    fn remove_file(&self, path: &Path) -> Result<(), Error>;

    /// Write `data`, creating or replacing the file.
    fn write(&self, path: &Path, data: &[u8]) -> Result<(), Error>;

    /// Read the entire file.
    fn read(&self, path: &Path) -> Result<Vec<u8>, Error>;

    /// Read the entire file as UTF-8 text.
    fn read_to_string(&self, path: &Path) -> Result<String, Error>;

    /// Copy a file from `from` to `to`.
    fn copy(&self, from: &Path, to: &Path) -> Result<(), Error>;

    /// Metadata, following symlinks.
    fn metadata(&self, path: &Path) -> Result<FsMetadata, Error>;

    /// Metadata without following the final symlink.
    fn symlink_metadata(&self, path: &Path) -> Result<FsMetadata, Error>;

    /// Read a symlink's target.
    fn read_link(&self, path: &Path) -> Result<PathBuf, Error>;

    /// Create a symlink at `path` pointing at `target`.
    fn symlink(&self, target: &Path, path: &Path) -> Result<(), Error>;

    /// Directory entries (does not follow the directory if it is a symlink).
    fn read_dir(&self, path: &Path) -> Result<Vec<FsDirEntry>, Error>;

    /// Create or truncate `path` for streaming writes.
    fn create_file(&self, path: &Path) -> Result<Box<dyn Write + Send>, Error>;

    /// Open `path` for streaming reads.
    fn open_file(&self, path: &Path) -> Result<Box<dyn Read + Send>, Error>;

    /// Set Unix mode bits. Implementations may ignore this on Windows.
    fn set_permissions(&self, path: &Path, mode: u32) -> Result<(), Error>;

    /// Canonical absolute path.
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, Error>;

    /// Whether `path` exists (does not follow a dangling symlink).
    fn exists(&self, path: &Path) -> bool {
        self.symlink_metadata(path).is_ok()
    }

    /// Regular file or symlink-to-file (follows symlinks).
    fn is_file(&self, path: &Path) -> bool {
        self.metadata(path).map(|m| m.is_file()).unwrap_or(false)
    }

    /// Directory or symlink-to-directory (follows symlinks).
    fn is_dir(&self, path: &Path) -> bool {
        self.metadata(path).map(|m| m.is_dir()).unwrap_or(false)
    }

    /// Whether the final component is a symlink.
    fn is_symlink(&self, path: &Path) -> bool {
        self.symlink_metadata(path)
            .map(|m| m.is_symlink())
            .unwrap_or(false)
    }

    /// Remove a file, symlink, or directory tree. Missing paths succeed.
    fn remove(&self, path: &Path) -> Result<(), Error> {
        match self.symlink_metadata(path) {
            Err(e) if e.is_not_found() => Ok(()),
            Err(e) => Err(e),
            Ok(meta) if meta.is_dir() && !meta.is_symlink() => self.remove_dir_all(path),
            Ok(_) => self.remove_file(path),
        }
    }

    /// Record guest uid/gid (unix xattrs). Default is a no-op.
    fn set_virtual_owner(&self, path: &Path, uid: u32, gid: u32) -> Result<(), Error> {
        let _ = (path, uid, gid);
        Ok(())
    }

    /// Windows directory junction. Default returns `false` (unsupported).
    fn junction(&self, target: &Path, path: &Path) -> Result<bool, Error> {
        let _ = (target, path);
        Ok(false)
    }
}

/// Host filesystem (`std::fs`).
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalFs;

impl FileSystem for LocalFs {
    fn create_dir_all(&self, path: &Path) -> Result<(), Error> {
        std::fs::create_dir_all(path).map_err(|e| Error::io(path, e))
    }

    fn remove_dir_all(&self, path: &Path) -> Result<(), Error> {
        std::fs::remove_dir_all(path).map_err(|e| Error::io(path, e))
    }

    fn remove_dir(&self, path: &Path) -> Result<(), Error> {
        std::fs::remove_dir(path).map_err(|e| Error::io(path, e))
    }

    fn remove_file(&self, path: &Path) -> Result<(), Error> {
        std::fs::remove_file(path).map_err(|e| Error::io(path, e))
    }

    fn write(&self, path: &Path, data: &[u8]) -> Result<(), Error> {
        std::fs::write(path, data).map_err(|e| Error::io(path, e))
    }

    fn read(&self, path: &Path) -> Result<Vec<u8>, Error> {
        std::fs::read(path).map_err(|e| Error::io(path, e))
    }

    fn read_to_string(&self, path: &Path) -> Result<String, Error> {
        std::fs::read_to_string(path).map_err(|e| Error::io(path, e))
    }

    fn copy(&self, from: &Path, to: &Path) -> Result<(), Error> {
        std::fs::copy(from, to)
            .map(|_| ())
            .map_err(|e| Error::io(to, e))
    }

    fn metadata(&self, path: &Path) -> Result<FsMetadata, Error> {
        std::fs::metadata(path)
            .map(meta_from_std)
            .map_err(|e| Error::io(path, e))
    }

    fn symlink_metadata(&self, path: &Path) -> Result<FsMetadata, Error> {
        std::fs::symlink_metadata(path)
            .map(meta_from_std)
            .map_err(|e| Error::io(path, e))
    }

    fn read_link(&self, path: &Path) -> Result<PathBuf, Error> {
        std::fs::read_link(path).map_err(|e| Error::io(path, e))
    }

    fn symlink(&self, target: &Path, path: &Path) -> Result<(), Error> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, path).map_err(|e| Error::io(path, e))
        }
        #[cfg(windows)]
        {
            let result = if target.to_string_lossy().ends_with(['/', '\\'])
                || path
                    .parent()
                    .map(|p| p.join(target).is_dir())
                    .unwrap_or(false)
            {
                std::os::windows::fs::symlink_dir(target, path)
            } else {
                std::os::windows::fs::symlink_file(target, path)
                    .or_else(|_| std::os::windows::fs::symlink_dir(target, path))
            };
            result.map_err(|e| Error::io(path, e))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (target, path);
            Err(Error::other("symlinks are not supported on this platform"))
        }
    }

    fn read_dir(&self, path: &Path) -> Result<Vec<FsDirEntry>, Error> {
        let mut out = Vec::new();
        for ent in std::fs::read_dir(path).map_err(|e| Error::io(path, e))? {
            let ent = ent.map_err(|e| Error::io(path, e))?;
            let ft = ent.file_type().map_err(|e| Error::io(ent.path(), e))?;
            out.push(FsDirEntry {
                name: ent.file_name(),
                path: ent.path(),
                is_dir: ft.is_dir(),
                is_file: ft.is_file(),
                is_symlink: ft.is_symlink(),
            });
        }
        Ok(out)
    }

    fn create_file(&self, path: &Path) -> Result<Box<dyn Write + Send>, Error> {
        std::fs::File::create(path)
            .map(|f| Box::new(f) as Box<dyn Write + Send>)
            .map_err(|e| Error::io(path, e))
    }

    fn open_file(&self, path: &Path) -> Result<Box<dyn Read + Send>, Error> {
        std::fs::File::open(path)
            .map(|f| Box::new(f) as Box<dyn Read + Send>)
            .map_err(|e| Error::io(path, e))
    }

    fn set_permissions(&self, path: &Path, mode: u32) -> Result<(), Error> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                .map_err(|e| Error::io(path, e))
        }
        #[cfg(not(unix))]
        {
            let _ = (path, mode);
            Ok(())
        }
    }

    fn canonicalize(&self, path: &Path) -> Result<PathBuf, Error> {
        std::fs::canonicalize(path).map_err(|e| Error::io(path, e))
    }

    fn set_virtual_owner(&self, path: &Path, uid: u32, gid: u32) -> Result<(), Error> {
        #[cfg(unix)]
        {
            set_virtual_owner_unix(path, uid, gid).map_err(|e| Error::io(path, e))
        }
        #[cfg(not(unix))]
        {
            let _ = (path, uid, gid);
            Ok(())
        }
    }

    fn junction(&self, target: &Path, path: &Path) -> Result<bool, Error> {
        #[cfg(windows)]
        {
            make_windows_junction(target, path).map_err(|e| Error::io(path, e))
        }
        #[cfg(not(windows))]
        {
            let _ = (target, path);
            Ok(false)
        }
    }
}

fn meta_from_std(m: std::fs::Metadata) -> FsMetadata {
    FsMetadata {
        is_dir: m.is_dir(),
        is_file: m.is_file(),
        is_symlink: m.file_type().is_symlink(),
        len: m.len(),
    }
}

/// Forward every [`FileSystem`] method to `self.fs`.
macro_rules! impl_filesystem_via_fs {
    ($ty:ident) => {
        impl<F: $crate::fs::FileSystem> $crate::fs::FileSystem for $ty<F> {
            fn create_dir_all(&self, path: &Path) -> Result<(), Error> {
                self.fs.create_dir_all(path)
            }
            fn remove_dir_all(&self, path: &Path) -> Result<(), Error> {
                self.fs.remove_dir_all(path)
            }
            fn remove_dir(&self, path: &Path) -> Result<(), Error> {
                self.fs.remove_dir(path)
            }
            fn remove_file(&self, path: &Path) -> Result<(), Error> {
                self.fs.remove_file(path)
            }
            fn write(&self, path: &Path, data: &[u8]) -> Result<(), Error> {
                self.fs.write(path, data)
            }
            fn read(&self, path: &Path) -> Result<Vec<u8>, Error> {
                self.fs.read(path)
            }
            fn read_to_string(&self, path: &Path) -> Result<String, Error> {
                self.fs.read_to_string(path)
            }
            fn copy(&self, from: &Path, to: &Path) -> Result<(), Error> {
                self.fs.copy(from, to)
            }
            fn metadata(&self, path: &Path) -> Result<$crate::fs::FsMetadata, Error> {
                self.fs.metadata(path)
            }
            fn symlink_metadata(&self, path: &Path) -> Result<$crate::fs::FsMetadata, Error> {
                self.fs.symlink_metadata(path)
            }
            fn read_link(&self, path: &Path) -> Result<PathBuf, Error> {
                self.fs.read_link(path)
            }
            fn symlink(&self, target: &Path, path: &Path) -> Result<(), Error> {
                self.fs.symlink(target, path)
            }
            fn read_dir(&self, path: &Path) -> Result<Vec<$crate::fs::FsDirEntry>, Error> {
                self.fs.read_dir(path)
            }
            fn create_file(&self, path: &Path) -> Result<Box<dyn Write + Send>, Error> {
                self.fs.create_file(path)
            }
            fn open_file(&self, path: &Path) -> Result<Box<dyn Read + Send>, Error> {
                self.fs.open_file(path)
            }
            fn set_permissions(&self, path: &Path, mode: u32) -> Result<(), Error> {
                self.fs.set_permissions(path, mode)
            }
            fn canonicalize(&self, path: &Path) -> Result<PathBuf, Error> {
                self.fs.canonicalize(path)
            }
            fn set_virtual_owner(&self, path: &Path, uid: u32, gid: u32) -> Result<(), Error> {
                self.fs.set_virtual_owner(path, uid, gid)
            }
            fn junction(&self, target: &Path, path: &Path) -> Result<bool, Error> {
                self.fs.junction(target, path)
            }
        }
    };
}

pub(crate) use impl_filesystem_via_fs;

/// Recursively copy a file or directory tree.
pub fn copy_tree<F: FileSystem>(fs: &F, src: &Path, dest: &Path) -> Result<(), Error> {
    fs.create_dir_all(dest)?;
    for entry in fs.read_dir(src)? {
        let from = entry.path;
        let to = dest.join(&entry.name);
        if entry.is_dir && !entry.is_symlink {
            copy_tree(fs, &from, &to)?;
        } else if entry.is_symlink {
            let target = fs.read_link(&from)?;
            let _ = fs.remove_file(&to);
            fs.symlink(&target, &to)?;
        } else {
            fs.copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_virtual_owner_unix(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    setxattr_u32(path, "user.buildkit.uid", uid)?;
    setxattr_u32(path, "user.buildkit.gid", gid)?;
    Ok(())
}

#[cfg(unix)]
fn setxattr_u32(path: &Path, name: &str, value: u32) -> io::Result<()> {
    use std::ffi::CString;
    let path_c = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let name_c = CString::new(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let data = value.to_string();
    let rc = unsafe {
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

#[cfg(windows)]
fn make_windows_junction(target: &Path, dest: &Path) -> io::Result<bool> {
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
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
    let status = cmd.status()?;
    Ok(status.success())
}
