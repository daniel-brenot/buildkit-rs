//! Export a built rootfs into the local image store.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use flate2::write::GzEncoder;
use flate2::Compression;
use oci_distribution::config::{Architecture, Config, ConfigFile, History, Os, Rootfs};
use oci_distribution::Reference;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tar::{Builder, Header};

use crate::platform::Platform;
use crate::reference::parse_reference;
use crate::store::{ImageStore, StoredImage, StoredLayer};
use crate::Error;

/// Image config state accumulated while building a stage.
///
/// Written into the OCI image config on export (`ENV`, `CMD`, `ENTRYPOINT`, …).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageMeta {
    /// `KEY=value` environment entries.
    pub env: Vec<String>,
    /// Guest working directory (`WORKDIR`).
    pub working_dir: String,
    /// `USER` spec, if set.
    pub user: Option<String>,
    /// `ENTRYPOINT` exec form, if set.
    pub entrypoint: Option<Vec<String>>,
    /// `CMD` exec form, if set.
    pub cmd: Option<Vec<String>>,
    /// Image labels.
    pub labels: HashMap<String, String>,
    /// `EXPOSE` ports.
    pub exposed_ports: Vec<String>,
    /// `VOLUME` paths.
    pub volumes: Vec<String>,
    /// Shell used to expand `RUN` shell form (from `SHELL`).
    #[serde(default = "default_shell")]
    pub shell: Vec<String>,
}

fn default_shell() -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into()]
}

impl ImageMeta {
    /// Default metadata: `PATH` set, `WORKDIR /`, shell `/bin/sh -c`.
    pub fn new() -> Self {
        Self {
            env: vec!["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()],
            working_dir: "/".into(),
            shell: vec!["/bin/sh".into(), "-c".into()],
            ..Default::default()
        }
    }

    /// Set or replace an environment variable.
    pub fn set_env(&mut self, key: &str, value: &str) {
        let prefix = format!("{key}=");
        self.env.retain(|e| !e.starts_with(&prefix));
        self.env.push(format!("{key}={value}"));
    }
}

/// Pack `rootfs` as a single gzip layer and write a cached image for each tag.
#[allow(dead_code)]
pub fn export_image<S: ImageStore>(
    store: &S,
    rootfs: &Path,
    meta: &ImageMeta,
    tags: &[String],
    platform: &Platform,
    history_comment: &str,
) -> Result<Vec<Reference>, Error> {
    let layer = pack_rootfs(rootfs)?;
    export_image_layer(store, &layer, meta, tags, platform, history_comment)
}

/// Write tags from an already-packed layer blob (in-memory).
pub fn export_image_layer<S: ImageStore>(
    store: &S,
    layer: &[u8],
    meta: &ImageMeta,
    tags: &[String],
    platform: &Platform,
    history_comment: &str,
) -> Result<Vec<Reference>, Error> {
    let digest = format!("sha256:{}", layer_digest(layer));
    export_image_with_digest(
        store,
        Some(layer),
        None,
        &digest,
        meta,
        tags,
        platform,
        history_comment,
    )
}

/// Write tags by copying a packed layer file (avoids loading large blobs into RAM).
pub fn export_image_layer_file<S: ImageStore>(
    store: &S,
    layer_path: &Path,
    digest: &str,
    meta: &ImageMeta,
    tags: &[String],
    platform: &Platform,
    history_comment: &str,
) -> Result<Vec<Reference>, Error> {
    export_image_with_digest(
        store,
        None,
        Some(layer_path),
        digest,
        meta,
        tags,
        platform,
        history_comment,
    )
}

fn export_image_with_digest<S: ImageStore>(
    store: &S,
    layer_bytes: Option<&[u8]>,
    layer_path: Option<&Path>,
    digest: &str,
    meta: &ImageMeta,
    tags: &[String],
    platform: &Platform,
    history_comment: &str,
) -> Result<Vec<Reference>, Error> {
    if tags.is_empty() {
        return Err(Error::other("build requires at least one tag"));
    }

    let config = build_config_file(meta, platform, history_comment);
    let mut refs = Vec::new();

    for tag in tags {
        let reference = parse_reference(tag)?;
        if store.has_digest(&reference, platform, digest) {
            refs.push(reference);
            continue;
        }
        if let Some(bytes) = layer_bytes {
            store.put_image(
                &reference,
                platform,
                StoredImage {
                    digest: Some(digest.to_string()),
                    config: config.clone(),
                    layers: vec![StoredLayer {
                        digest: Some(digest.to_string()),
                        data: bytes.to_vec(),
                    }],
                },
            )?;
        } else if let Some(src) = layer_path {
            store.put_image_layer_file(&reference, platform, digest, &config, src)?;
        } else {
            return Err(Error::other("export: missing layer data"));
        }
        refs.push(reference);
    }
    Ok(refs)
}

/// Pack rootfs into a gzip layer (also used to populate the build cache blob).
pub fn pack_rootfs(rootfs: &Path) -> Result<Vec<u8>, Error> {
    let mut encoded = Vec::new();
    {
        let enc = GzEncoder::new(&mut encoded, Compression::fast());
        let mut archive = Builder::new(enc);
        add_dir(&mut archive, rootfs, Path::new(""))?;
        let enc = archive
            .into_inner()
            .map_err(|e| Error::other(format!("tar finish: {e}")))?;
        enc.finish()
            .map_err(|e| Error::other(format!("gzip finish: {e}")))?;
    }
    Ok(encoded)
}

fn build_config_file(meta: &ImageMeta, platform: &Platform, comment: &str) -> ConfigFile {
    let arch = match platform.architecture.as_str() {
        "arm64" | "aarch64" => Architecture::Arm64,
        "arm" => Architecture::Arm,
        _ => Architecture::Amd64,
    };
    let mut cfg = Config {
        env: Some(meta.env.clone()),
        working_dir: Some(meta.working_dir.clone()),
        user: meta.user.clone(),
        entrypoint: meta.entrypoint.clone(),
        cmd: meta.cmd.clone(),
        labels: if meta.labels.is_empty() {
            None
        } else {
            Some(meta.labels.clone())
        },
        ..Config::default()
    };
    if !meta.exposed_ports.is_empty() {
        cfg.exposed_ports = Some(meta.exposed_ports.iter().cloned().collect());
    }
    if !meta.volumes.is_empty() {
        cfg.volumes = Some(meta.volumes.iter().cloned().collect());
    }

    ConfigFile {
        architecture: arch,
        os: Os::Linux,
        config: Some(cfg),
        rootfs: Rootfs {
            r#type: "layers".into(),
            diff_ids: vec![format!("sha256:{}", layer_digest(b"squash"))],
        },
        history: Some(vec![History {
            created: None,
            author: None,
            created_by: Some(comment.into()),
            comment: Some("buildkit".into()),
            empty_layer: None,
        }]),
        ..ConfigFile::default()
    }
}

fn add_dir<W: Write>(archive: &mut Builder<W>, dir: &Path, prefix: &Path) -> Result<(), Error> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let rel = prefix.join(&name);
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            let mut header = Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            header.set_cksum();
            let tar_path = format!("{}/", rel.to_string_lossy().replace('\\', "/"));
            archive
                .append_data(&mut header, tar_path, std::io::empty())
                .map_err(|e| Error::other(format!("tar dir: {e}")))?;
            add_dir(archive, &path, &rel)?;
        } else if ft.is_file() {
            let data = fs::read(&path)?;
            let mut header = Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_size(data.len() as u64);
            header.set_cksum();
            let tar_path = rel.to_string_lossy().replace('\\', "/");
            archive
                .append_data(&mut header, tar_path, data.as_slice())
                .map_err(|e| Error::other(format!("tar file: {e}")))?;
        }
    }
    Ok(())
}

/// Digest label used for local image identity / cache blob reuse.
pub fn layer_digest(data: &[u8]) -> String {
    hex_encode(Sha256::digest(data))
}

pub(crate) fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Working directory for an in-progress build under the data root.
pub fn work_dir(build_root: &Path, id: &str) -> PathBuf {
    build_root.join(id)
}
