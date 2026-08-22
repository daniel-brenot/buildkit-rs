//! Local build context and basic `.dockerignore` filtering.

use std::fs;
use std::path::{Path, PathBuf};

use crate::Error;

#[derive(Debug, Clone)]
pub struct BuildContext {
    root: PathBuf,
    ignore: Vec<IgnoreRule>,
}

#[derive(Debug, Clone)]
struct IgnoreRule {
    pattern: String,
    negate: bool,
}

impl BuildContext {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, Error> {
        let root = root.into();
        if !root.is_dir() {
            return Err(Error::other(format!(
                "build context is not a directory: {}",
                root.display()
            )));
        }
        let ignore = load_dockerignore(&root)?;
        Ok(BuildContext { root, ignore })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn resolve(&self, rel: &str) -> Result<PathBuf, Error> {
        let rel = rel.trim_start_matches("./");
        let path = if rel == "." || rel.is_empty() {
            self.root.clone()
        } else {
            self.root.join(rel)
        };
        let canon_root = self
            .root
            .canonicalize()
            .map_err(|e| Error::other(format!("context: {e}")))?;
        if path.exists() {
            let canon = path
                .canonicalize()
                .map_err(|e| Error::other(format!("context path '{}': {e}", path.display())))?;
            if !canon.starts_with(&canon_root) {
                return Err(Error::other(format!(
                    "path '{rel}' escapes the build context"
                )));
            }
            return Ok(canon);
        }
        if path.starts_with(&self.root) || path == self.root {
            Ok(path)
        } else {
            Err(Error::other(format!(
                "path '{rel}' escapes the build context"
            )))
        }
    }

    pub fn is_ignored(&self, rel: &Path) -> bool {
        let rel = rel.to_string_lossy().replace('\\', "/");
        let mut ignored = false;
        for rule in &self.ignore {
            if match_pattern(&rule.pattern, &rel) {
                ignored = !rule.negate;
            }
        }
        ignored
    }
}

fn load_dockerignore(root: &Path) -> Result<Vec<IgnoreRule>, Error> {
    let path = root.join(".dockerignore");
    if !path.is_file() {
        return Ok(default_ignore());
    }
    let data = fs::read_to_string(&path)?;
    let mut rules = default_ignore();
    for line in data.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (negate, pattern) = if let Some(rest) = line.strip_prefix('!') {
            (true, rest.trim())
        } else {
            (false, line)
        };
        rules.push(IgnoreRule {
            pattern: pattern.trim_start_matches("./").to_string(),
            negate,
        });
    }
    Ok(rules)
}

fn default_ignore() -> Vec<IgnoreRule> {
    vec![
        IgnoreRule {
            pattern: ".git".into(),
            negate: false,
        },
        IgnoreRule {
            pattern: ".git/**".into(),
            negate: false,
        },
    ]
}

fn match_pattern(pattern: &str, path: &str) -> bool {
    if pattern == path {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return path == prefix || path.starts_with(&(prefix.to_string() + "/"));
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        if let Some(rest) = path.strip_prefix(&(prefix.to_string() + "/")) {
            return !rest.contains('/');
        }
        return false;
    }
    if let Some(suffix) = pattern.strip_prefix("**/") {
        return path == suffix || path.ends_with(&format!("/{suffix}"));
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return path.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return path.starts_with(prefix);
    }
    path == pattern || path.starts_with(&(pattern.to_string() + "/"))
}

/// Recursively copy `src` (file or directory) into `dest`, honoring
/// `.dockerignore` when `src` is under the context.
pub fn copy_into(context: &BuildContext, src: &Path, dest: &Path) -> Result<(), Error> {
    if src.is_file() {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dest).map_err(|e| {
            Error::other(format!("copy {} -> {}: {e}", src.display(), dest.display()))
        })?;
        return Ok(());
    }
    if !src.is_dir() {
        return Err(Error::other(format!(
            "COPY source not found: {}",
            src.display()
        )));
    }
    fs::create_dir_all(dest)?;
    copy_dir(context, src, dest, src)?;
    Ok(())
}

fn copy_dir(
    context: &BuildContext,
    src_dir: &Path,
    dest_dir: &Path,
    walk_root: &Path,
) -> Result<(), Error> {
    for entry in fs::read_dir(src_dir)? {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(walk_root).unwrap_or(path.as_path());
        if context.is_ignored(rel) {
            continue;
        }
        let dest = dest_dir.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() {
            fs::create_dir_all(&dest)?;
            copy_dir(context, &path, &dest, walk_root)?;
        } else if ft.is_file() {
            fs::copy(&path, &dest).map_err(|e| {
                Error::other(format!(
                    "copy {} -> {}: {e}",
                    path.display(),
                    dest.display()
                ))
            })?;
        }
    }
    Ok(())
}
