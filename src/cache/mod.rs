//! Docker overlay2 local layer cache.
//!
//! Each instruction produces a chain id:
//! `sha256(parent_id || "\\n" || instruction_key)`.
//! Snapshots use Docker's overlay2 graph-driver layout. Cache hits return a
//! path into the snapshot (no copy). Callers must copy-on-write before
//! mutating the rootfs. Packed `layer.tar.gz` blobs are stored after the first
//! export so fully-cached rebuilds skip retarring.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use dockerfile::Instruction;
use sha2::{Digest, Sha256};

use crate::context::BuildContext;
use crate::expand;
use crate::export::ImageMeta;
use crate::fs::FileSystem;
use crate::fsutil::join_workdir;
use crate::platform::Platform;
use crate::reference::parse_reference;
use crate::store::ImageStore;
use crate::Error;

mod overlay2;

use overlay2::Overlay2;

/// Docker overlay2 instruction-layer cache.
///
/// Opened under `<data_root>/overlay2`. Not a swap point — image blobs go
/// through [`ImageStore`] (via [`FileSystem`]); this type only holds instruction
/// snapshots.
#[derive(Debug, Clone)]
pub struct LayerCache {
    overlay: Overlay2,
}

impl LayerCache {
    /// Open the overlay2 cache under `data_root`.
    pub fn open<F: FileSystem>(fs: &F, data_root: &Path) -> Result<Self, Error> {
        Ok(Self {
            overlay: Overlay2::open(fs, data_root)?,
        })
    }

    /// Directory that holds per-id snapshot folders.
    pub fn root(&self) -> &Path {
        self.overlay.overlay_root()
    }

    /// Whether a complete cache entry exists for chain id `id`.
    pub fn has<F: FileSystem>(&self, fs: &F, id: &str) -> bool {
        self.overlay.has_id(fs, id)
    }

    /// Load image config and build-arg state for chain id `id` (no filesystem copy).
    pub fn load_meta<F: FileSystem>(
        &self,
        fs: &F,
        id: &str,
    ) -> Result<(ImageMeta, HashMap<String, String>), Error> {
        self.overlay.load_meta_id(fs, id)
    }

    /// Path to the materialized rootfs for `id`.
    pub fn resolve_rootfs<F: FileSystem>(&self, fs: &F, id: &str) -> Result<PathBuf, Error> {
        self.overlay.resolve_id(fs, id)
    }

    /// Path to a packed layer blob if stored as a file.
    pub fn layer_blob_path(&self, id: &str) -> Option<PathBuf> {
        Some(self.overlay.blob_path(id))
    }

    /// Whether a packed layer blob exists for this chain id.
    pub fn has_layer_blob<F: FileSystem>(&self, fs: &F, id: &str) -> bool {
        self.overlay.has_layer_blob(fs, id)
    }

    /// Read the packed export blob for `id`.
    pub fn read_layer_blob<F: FileSystem>(&self, fs: &F, id: &str) -> Result<Vec<u8>, Error> {
        self.overlay.read_layer_blob(fs, id)
    }

    /// Digest label stored for the packed blob (`sha256:…`).
    pub fn layer_blob_digest<F: FileSystem>(&self, fs: &F, id: &str) -> Result<String, Error> {
        self.overlay.layer_blob_digest(fs, id)
    }

    /// Store a packed export blob for `id`.
    pub fn write_layer_blob<F: FileSystem>(
        &self,
        fs: &F,
        id: &str,
        bytes: &[u8],
    ) -> Result<(), Error> {
        self.overlay.write_layer_blob(fs, id, bytes)
    }

    /// Persist the current stage state under chain id `id`.
    ///
    /// When `filesystem_changed` is false, the new layer has an empty `diff/`
    /// and reuses the parent's lower chain. When true, `rootfs` is stored as an
    /// overlay2 changeset against the parent.
    pub fn save<F: FileSystem>(
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
        self.overlay.save_layer(
            fs,
            id,
            parent,
            instruction,
            meta,
            args,
            rootfs,
            filesystem_changed,
        )
    }

    /// Remove all cached layers.
    pub fn clear<F: FileSystem>(&self, fs: &F) -> Result<(), Error> {
        self.overlay.clear_all(fs)
    }
}

/// Compute the chain id `sha256(parent || "\\n" || instruction_key)` as hex.
pub fn chain_id(parent: &str, instruction_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(parent.as_bytes());
    hasher.update(b"\n");
    hasher.update(instruction_key.as_bytes());
    crate::export::hex_encode(hasher.finalize())
}

/// Instruction-key prefix for a `FROM` line (includes base digest when known).
pub fn from_cache_key<F: FileSystem>(
    store: &ImageStore<F>,
    base: &str,
    scratch: bool,
    platform: &Platform,
) -> String {
    if scratch {
        return format!("FROM scratch|{}|{}", platform.os, platform.architecture);
    }
    let digest = parse_reference_digest(store, base, platform);
    format!(
        "FROM {base}|digest={digest}|{}|{}",
        platform.os, platform.architecture
    )
}

fn parse_reference_digest<F: FileSystem>(
    store: &ImageStore<F>,
    base: &str,
    platform: &Platform,
) -> String {
    match parse_reference(base) {
        Ok(reference) => store
            .image_digest(&reference, platform)
            .unwrap_or_else(|| "unknown".into()),
        Err(_) => "unknown".into(),
    }
}

/// Build the cache key for one instruction (expanded, content-aware for COPY/ADD).
pub fn instruction_cache_key<F: FileSystem>(
    fs: &F,
    inst: &Instruction,
    meta: &ImageMeta,
    args: &HashMap<String, String>,
    context: &BuildContext,
    completed_rootfs: &HashMap<String, (PathBuf, String)>,
    network: &str,
) -> Result<(String, bool), Error> {
    let vars = merged_vars(meta, args);
    match inst {
        Instruction::Run(run) => {
            let cmd = command_cache_text(&run.command, &vars);
            let mut env: Vec<_> = meta.env.clone();
            env.sort();
            Ok((
                format!(
                    "RUN|{cmd}|cwd={}|user={}|env={}|net={network}",
                    meta.working_dir,
                    meta.user.as_deref().unwrap_or(""),
                    env.join("\n")
                ),
                true,
            ))
        }
        Instruction::Copy(copy) => {
            let dest = expand::expand(&copy.destination, &vars);
            let from = copy
                .flags
                .iter()
                .find(|f| f.is("from"))
                .and_then(|f| f.value.clone());
            let mut parts = Vec::new();
            for src in &copy.sources {
                let src = expand::expand(src, &vars);
                let hash = match &from {
                    Some(stage) => {
                        let (root, wd) = completed_rootfs.get(stage).ok_or_else(|| {
                            Error::other(format!(
                                "COPY --from={stage}: stage not available for cache key"
                            ))
                        })?;
                        let host = if src.starts_with('/') {
                            crate::fsutil::guest_to_host(root, &src)
                        } else {
                            crate::fsutil::guest_to_host(root, &join_workdir(wd, &src))
                        };
                        hash_path(fs, &host)?
                    }
                    None => {
                        let host = context.resolve(fs, &src)?;
                        hash_path(fs, &host)?
                    }
                };
                parts.push(format!("{src}#{hash}"));
            }
            let from_s = from.as_deref().unwrap_or("");
            Ok((
                format!("COPY|--from={from_s}|{}|{dest}", parts.join(",")),
                true,
            ))
        }
        Instruction::Add(add) => {
            let dest = expand::expand(&add.destination, &vars);
            let mut parts = Vec::new();
            for src in &add.sources {
                let src = expand::expand(src, &vars);
                let hash = if is_remote_url(&src) {
                    format!("url:{src}")
                } else {
                    let host = context.resolve(fs, &src)?;
                    hash_path(fs, &host)?
                };
                parts.push(format!("{src}#{hash}"));
            }
            Ok((format!("ADD|{}|{dest}", parts.join(",")), true))
        }
        Instruction::Env(env) => {
            let body: Vec<_> = env
                .pairs
                .iter()
                .map(|p| format!("{}={}", p.key, expand::expand(&p.value, &vars)))
                .collect();
            Ok((format!("ENV|{}", body.join(" ")), false))
        }
        Instruction::Label(label) => {
            let body: Vec<_> = label
                .pairs
                .iter()
                .map(|p| format!("{}={}", p.key, expand::expand(&p.value, &vars)))
                .collect();
            Ok((format!("LABEL|{}", body.join(" ")), false))
        }
        Instruction::Arg(arg) => {
            let body: Vec<_> = arg
                .args
                .iter()
                .map(|a| {
                    let d = a
                        .default
                        .as_ref()
                        .map(|v| expand::expand(v, &vars))
                        .unwrap_or_default();
                    format!("{}|{d}", a.name)
                })
                .collect();
            Ok((format!("ARG|{}", body.join(";")), false))
        }
        Instruction::Workdir(wd) => {
            let path = expand::expand(&wd.path, &vars);
            Ok((format!("WORKDIR|{path}"), true))
        }
        Instruction::User(user) => {
            let user = expand::expand(&user.spec, &vars);
            Ok((format!("USER|{user}"), false))
        }
        Instruction::Entrypoint(ep) => {
            let text = command_cache_text(&ep.command, &vars);
            Ok((format!("ENTRYPOINT|{text}"), false))
        }
        Instruction::Cmd(cmd) => {
            let text = command_cache_text(&cmd.command, &vars);
            Ok((format!("CMD|{text}"), false))
        }
        Instruction::Expose(ex) => {
            let ports: Vec<_> = ex
                .ports
                .iter()
                .map(|p| expand::expand(&p.to_string(), &vars))
                .collect();
            Ok((format!("EXPOSE|{}", ports.join(" ")), false))
        }
        Instruction::Volume(vol) => {
            let vols: Vec<_> = vol.paths.iter().map(|v| expand::expand(v, &vars)).collect();
            Ok((format!("VOLUME|{}", vols.join(" ")), false))
        }
        Instruction::Shell(sh) => Ok((format!("SHELL|{:?}", sh.args), false)),
        other => Ok((format!("{}|{}", other.keyword(), other.keyword()), false)),
    }
}

fn command_cache_text(command: &dockerfile::Command, vars: &HashMap<String, String>) -> String {
    match command {
        dockerfile::Command::Shell(s) => expand::expand(s, vars),
        dockerfile::Command::Exec(args) => format!("{:?}", expand::expand_vec(args, vars)),
    }
}

fn merged_vars(meta: &ImageMeta, args: &HashMap<String, String>) -> HashMap<String, String> {
    let mut vars = args.clone();
    for entry in &meta.env {
        if let Some((k, v)) = entry.split_once('=') {
            vars.entry(k.to_string()).or_insert_with(|| v.to_string());
        }
    }
    vars
}

fn is_remote_url(src: &str) -> bool {
    let lower = src.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

fn hash_path<F: FileSystem>(fs: &F, path: &Path) -> Result<String, Error> {
    let mut hasher = Sha256::new();
    if fs.is_dir(path) {
        hash_dir(fs, path, path, &mut hasher)?;
    } else if fs.is_file(path) {
        hash_file(fs, path, &mut hasher)?;
    } else {
        hasher.update(b"missing");
    }
    Ok(crate::export::hex_encode(hasher.finalize()))
}

fn hash_dir<F: FileSystem>(
    fs: &F,
    root: &Path,
    dir: &Path,
    hasher: &mut Sha256,
) -> Result<(), Error> {
    let mut entries = fs
        .read_dir(dir)
        .map_err(|e| Error::other(format!("cache hash {}: {e}", dir.display())))?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for ent in entries {
        let path = ent.path;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        hasher.update(rel.as_bytes());
        hasher.update(&[0]);
        if fs.is_dir(&path) {
            hash_dir(fs, root, &path, hasher)?;
        } else if fs.is_file(&path) {
            hash_file(fs, &path, hasher)?;
        }
    }
    Ok(())
}

fn hash_file<F: FileSystem>(fs: &F, path: &Path, hasher: &mut Sha256) -> Result<(), Error> {
    let mut file = fs
        .open_file(path)
        .map_err(|e| Error::other(format!("cache hash {}: {e}", path.display())))?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| Error::other(format!("cache hash {}: {e}", path.display())))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::ImageMeta;

    #[test]
    fn overlay2_save_and_has() {
        let root = std::env::temp_dir().join(format!(
            "buildkit-cache-handler-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let fs = crate::fs::LocalFs;
        let cache = LayerCache::open(&fs, &root).unwrap();
        assert!(!cache.has(&fs, "abc"));

        let rootfs = root.join("stage-rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::write(rootfs.join("hello.txt"), b"hi").unwrap();

        cache
            .save(
                &fs,
                "abc",
                "",
                "FROM scratch",
                &ImageMeta::new(),
                &HashMap::new(),
                &rootfs,
                true,
            )
            .unwrap();
        assert!(cache.has(&fs, "abc"));
        assert!(cache
            .resolve_rootfs(&fs, "abc")
            .unwrap()
            .join("hello.txt")
            .is_file());

        let child_root = root.join("stage-child");
        std::fs::create_dir_all(&child_root).unwrap();
        std::fs::write(child_root.join("hello.txt"), b"hi").unwrap();
        std::fs::write(child_root.join("extra.txt"), b"x").unwrap();
        cache
            .save(
                &fs,
                "def",
                "abc",
                "COPY extra.txt /",
                &ImageMeta::new(),
                &HashMap::new(),
                &child_root,
                true,
            )
            .unwrap();
        let merged = cache.resolve_rootfs(&fs, "def").unwrap();
        assert!(merged.join("hello.txt").is_file());
        assert!(merged.join("extra.txt").is_file());
        let child_diff = root.join("overlay2").join("def").join("diff");
        assert!(child_diff.join("extra.txt").is_file());
        assert!(!child_diff.join("hello.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
