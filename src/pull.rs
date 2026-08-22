//! Pull images from OCI registries into a local [`crate::ImageStore`].

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::stream::{self, StreamExt, TryStreamExt};
use oci_distribution::client::ClientConfig;
use oci_distribution::config::ConfigFile;
use oci_distribution::manifest::IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE;
use oci_distribution::manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE;
use oci_distribution::manifest::OciDescriptor;
use oci_distribution::Client;
use oci_distribution::Reference;

use crate::auth::auth_for_reference;
use crate::platform::default_pull_platform;
use crate::platform::platform_resolver;
use crate::platform::Platform;
use crate::progress::{short_layer_id, NullProgress, PullEvent, PullProgress};
use crate::store::ImageStore;
use crate::Error;

/// Accepted OCI/Docker layer media types.
const LAYER_MEDIA_TYPES: &[&str] = &[
    IMAGE_LAYER_GZIP_MEDIA_TYPE,
    IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE,
    "application/vnd.docker.image.rootfs.diff.tar.gzip",
];

const MAX_CONCURRENT_DOWNLOAD: usize = 4;
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(50);

/// A pulled image stored in the local image cache.
#[derive(Debug)]
pub struct PulledImage {
    /// Parsed reference that was pulled.
    pub reference: Reference,
    /// Platform selected from the image index.
    pub platform: Platform,
    /// Image config from the registry (or the local cache on an up-to-date hit).
    pub config: ConfigFile,
    /// Directory under the store that holds `config.json`, `digest`, and layers.
    pub cache_dir: std::path::PathBuf,
    /// Full layer digests in manifest order (for unpack progress).
    pub layer_digests: Vec<String>,
    /// Manifest digest when the registry returned one.
    pub digest: Option<String>,
    /// `false` when the local cache already matched the registry digest
    /// (Docker `--pull=always` "Image is up to date" path).
    pub updated: bool,
}

fn client_for_platform(platform: &Platform) -> Client {
    let mut config = ClientConfig::default();
    config.platform_resolver = Some(platform_resolver(platform.clone()));
    config.max_concurrent_download = MAX_CONCURRENT_DOWNLOAD;
    Client::new(config)
}

fn validate_layer_media_types(layers: &[OciDescriptor]) -> Result<(), Error> {
    if layers.is_empty() {
        return Err(Error::other("image manifest has no layers"));
    }
    for layer in layers {
        if !LAYER_MEDIA_TYPES
            .iter()
            .any(|accepted| layer.media_type == *accepted)
        {
            return Err(Error::other(format!(
                "incompatible layer media type: {}",
                layer.media_type
            )));
        }
    }
    Ok(())
}

fn pulling_from_parts(reference: &Reference) -> (String, String) {
    let tag = reference
        .tag()
        .map(str::to_string)
        .unwrap_or_else(|| "latest".to_string());
    let repository = reference.repository().to_string();
    (tag, repository)
}

/// Pull `reference` from its registry into `store`.
///
/// Skips re-downloading layers when the local digest already matches the
/// registry. Progress events are discarded; see [`pull_image_with_progress`].
pub async fn pull_image(
    store: &ImageStore,
    reference: &Reference,
    platform: &Platform,
) -> Result<PulledImage, Error> {
    pull_image_with_progress(store, reference, platform, &mut NullProgress).await
}

/// Pull `reference` while emitting Docker-style layer download events.
///
/// Does not emit the final `Digest` / `Status` lines — call
/// [`finish_pull_progress`] after unpacking so ordering matches Docker.
pub async fn pull_image_with_progress(
    store: &ImageStore,
    reference: &Reference,
    platform: &Platform,
    progress: &mut dyn PullProgress,
) -> Result<PulledImage, Error> {
    let cache_dir = store.image_dir(reference, platform);
    fs::create_dir_all(&cache_dir)?;

    let client = client_for_platform(platform);
    let auth = auth_for_reference(reference);
    tracing::debug!(image = %reference, os = %platform.os, arch = %platform.architecture, "fetching manifest and layers");

    let (tag, repository) = pulling_from_parts(reference);
    progress.event(PullEvent::PullingFrom { tag, repository });

    let (manifest, digest, config_blob) = client
        .pull_manifest_and_config(reference, &auth)
        .await?;
    validate_layer_media_types(&manifest.layers)?;

    let layer_digests: Vec<String> = manifest.layers.iter().map(|l| l.digest.clone()).collect();

    // Docker `--pull=always`: still consult the registry, but skip re-download
    // when the local digest and layer blobs already match.
    if layers_cache_matches(&cache_dir, &digest, &layer_digests) {
        let config = read_cached_config(&cache_dir, &config_blob)?;
        tracing::debug!(image = %reference, %digest, "image already up to date");
        return Ok(PulledImage {
            reference: reference.clone(),
            platform: platform.clone(),
            config,
            cache_dir,
            layer_digests,
            digest: if digest.is_empty() {
                None
            } else {
                Some(digest)
            },
            updated: false,
        });
    }

    let config: ConfigFile = serde_json::from_str(&config_blob).map_err(|e| {
        Error::other(format!("image config: {e}"))
    })?;

    let layers_dir = cache_dir.join("layers");
    if layers_dir.exists() {
        fs::remove_dir_all(&layers_dir)?;
    }
    fs::create_dir_all(&layers_dir)?;

    let progress = Arc::new(Mutex::new(progress));

    let downloaded: Vec<(usize, Vec<u8>)> = stream::iter(manifest.layers.iter().enumerate())
        .map(|(index, layer)| {
            let client = &client;
            let progress = Arc::clone(&progress);
            let reference = reference;
            async move {
                let id = short_layer_id(&layer.digest);
                let total = layer.size.max(0) as u64;
                {
                    let mut p = progress.lock().unwrap_or_else(|e| e.into_inner());
                    p.event(PullEvent::LayerStatus {
                        id: id.clone(),
                        status: "Pulling fs layer".into(),
                    });
                    p.event(PullEvent::LayerStatus {
                        id: id.clone(),
                        status: "Downloading".into(),
                    });
                }

                let mut stream = client.pull_blob_stream(reference, layer).await?;
                let mut data = Vec::with_capacity(total.min(64 * 1024 * 1024) as usize);
                let mut last_emit = Instant::now() - PROGRESS_MIN_INTERVAL;
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(|e| {
                        Error::other(format!("failed to download layer {id}: {e}"))
                    })?;
                    data.extend_from_slice(&chunk);
                    let now = Instant::now();
                    if now.duration_since(last_emit) >= PROGRESS_MIN_INTERVAL {
                        last_emit = now;
                        let mut p = progress.lock().unwrap_or_else(|e| e.into_inner());
                        p.event(PullEvent::LayerProgress {
                            id: id.clone(),
                            status: "Downloading".into(),
                            current: data.len() as u64,
                            total,
                        });
                    }
                }

                {
                    let mut p = progress.lock().unwrap_or_else(|e| e.into_inner());
                    if total > 0 {
                        p.event(PullEvent::LayerProgress {
                            id: id.clone(),
                            status: "Downloading".into(),
                            current: data.len() as u64,
                            total,
                        });
                    }
                    p.event(PullEvent::LayerStatus {
                        id,
                        status: "Download complete".into(),
                    });
                }

                Ok::<_, Error>((index, data))
            }
        })
        .buffer_unordered(MAX_CONCURRENT_DOWNLOAD)
        .try_collect()
        .await?;

    let mut ordered = vec![Vec::new(); downloaded.len()];
    for (index, data) in downloaded {
        ordered[index] = data;
    }

    for (index, data) in ordered.into_iter().enumerate() {
        let path = layers_dir.join(format!("{index}.tar.gz"));
        fs::write(&path, &data)?;
    }
    fs::write(
        layers_dir.join("digests"),
        layer_digests.join("\n") + "\n",
    )?;

    fs::write(
        cache_dir.join("config.json"),
        serde_json::to_string_pretty(&config)?,
    )?;
    fs::write(
        cache_dir.join("platform"),
        format!("{}/{}", platform.os, platform.architecture),
    )?;

    if !digest.is_empty() {
        fs::write(cache_dir.join("digest"), &digest)?;
    }

    Ok(PulledImage {
        reference: reference.clone(),
        platform: platform.clone(),
        config,
        cache_dir,
        layer_digests,
        digest: if digest.is_empty() {
            None
        } else {
            Some(digest)
        },
        updated: true,
    })
}

fn layers_cache_matches(cache_dir: &Path, digest: &str, layer_digests: &[String]) -> bool {
    if digest.is_empty() {
        return false;
    }
    let Ok(local) = fs::read_to_string(cache_dir.join("digest")) else {
        return false;
    };
    if local.trim() != digest.trim() {
        return false;
    }
    if !cache_dir.join("config.json").is_file() {
        return false;
    }
    let layers_dir = cache_dir.join("layers");
    let Ok(stored) = fs::read_to_string(layers_dir.join("digests")) else {
        return false;
    };
    let stored: Vec<&str> = stored.lines().filter(|l| !l.is_empty()).collect();
    if stored.len() != layer_digests.len() {
        return false;
    }
    for (i, expected) in layer_digests.iter().enumerate() {
        if stored.get(i).copied() != Some(expected.as_str()) {
            return false;
        }
        let blob = layers_dir.join(format!("{i}.tar.gz"));
        if !blob.is_file() {
            return false;
        }
    }
    true
}

fn read_cached_config(cache_dir: &Path, config_blob: &str) -> Result<ConfigFile, Error> {
    match fs::read_to_string(cache_dir.join("config.json")) {
        Ok(s) => serde_json::from_str(&s)
            .map_err(|e| Error::other(format!("cached image config: {e}"))),
        Err(_) => serde_json::from_str(config_blob)
            .map_err(|e| Error::other(format!("image config: {e}"))),
    }
}

/// Emit the final Docker pull footer (`Digest` + `Status`).
pub fn finish_pull_progress(
    reference: &str,
    digest: Option<&str>,
    updated: bool,
    progress: &mut dyn PullProgress,
) {
    if let Some(digest) = digest {
        progress.event(PullEvent::Digest {
            digest: digest.to_string(),
        });
    }
    let message = if updated {
        format!("Downloaded newer image for {reference}")
    } else {
        format!("Image is up to date for {reference}")
    };
    progress.event(PullEvent::Status { message });
}

/// Pull `image`, unpack its layers into `dest`, and return the resolved reference.
pub async fn pull_and_materialize(
    store: &ImageStore,
    image: &str,
    dest: &Path,
    platform: &Platform,
) -> Result<Reference, Error> {
    let reference = crate::parse_reference(image)?;
    pull_image(store, &reference, platform).await?;
    crate::materialize::materialize_rootfs(store, &reference, platform, dest)?;
    Ok(reference)
}

/// Pull `reference` using [`crate::default_pull_platform`].
pub async fn pull_image_default(
    store: &ImageStore,
    reference: &Reference,
) -> Result<PulledImage, Error> {
    pull_image(store, reference, &default_pull_platform()).await
}
