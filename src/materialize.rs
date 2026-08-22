//! Expand cached layers into a root filesystem directory.

use std::fs;
use std::path::Path;

use oci_distribution::config::ConfigFile;
use oci_distribution::config::Os;

use crate::progress::{short_layer_id, NullProgress, PullEvent, PullProgress};
use crate::store::ImageStore;
use crate::unpack::apply_layer;
use crate::Error;

/// Reject native Windows and macOS image configs; only Linux rootfs is unpacked.
pub fn check_runnable_platform(config: &ConfigFile) -> Result<(), Error> {
    if config.os == Os::Windows {
        return Err(Error::other(
            "native Windows images are not supported; use a linux/* platform",
        ));
    }
    if config.os == Os::Darwin {
        return Err(Error::other(
            "macOS images are not supported; use a linux/* platform",
        ));
    }
    Ok(())
}

/// Expand cached layers into `dest` (typically a stage rootfs).
///
/// Reuses an existing materialization when the image content fingerprint still
/// matches.
pub fn materialize_rootfs(
    store: &ImageStore,
    reference: &oci_distribution::Reference,
    platform: &crate::platform::Platform,
    dest: &Path,
) -> Result<(), Error> {
    materialize_rootfs_with_progress(store, reference, platform, dest, &mut NullProgress, false)
}

/// Like [`materialize_rootfs`], optionally emitting Docker-style extract progress.
pub fn materialize_rootfs_with_progress(
    store: &ImageStore,
    reference: &oci_distribution::Reference,
    platform: &crate::platform::Platform,
    dest: &Path,
    progress: &mut dyn PullProgress,
    report_extract: bool,
) -> Result<(), Error> {
    let cache_dir = store.image_dir(reference, platform);
    if !cache_dir.join("config.json").is_file() {
        return Err(Error::other(format!(
            "image {reference} is not cached; pull it first"
        )));
    }

    let config_data = fs::read_to_string(cache_dir.join("config.json"))?;
    let image_config: ConfigFile = serde_json::from_str(&config_data)?;
    check_runnable_platform(&image_config)?;

    let stamp = bundle_content_stamp(&cache_dir, &config_data)?;
    let stamp_path = dest.join(ROOTFS_STAMP);
    let existing_stamp = fs::read_to_string(&stamp_path).unwrap_or_default();
    if dest.is_dir() && stamp_path.is_file() && existing_stamp.trim() == stamp.trim() {
        tracing::debug!(rootfs = %dest.display(), "reusing materialized rootfs");
        return Ok(());
    }

    if dest.exists() {
        fs::remove_dir_all(dest)?;
    }
    fs::create_dir_all(dest)?;

    let layers_dir = cache_dir.join("layers");
    let digests = read_layer_digests(&layers_dir);
    let mut layers: Vec<_> = if layers_dir.is_dir() {
        fs::read_dir(&layers_dir)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".tar.gz"))
            })
            .collect()
    } else {
        Vec::new()
    };
    layers.sort();

    if report_extract && !layers.is_empty() {
        progress.event(PullEvent::Unpacking {
            reference: reference.to_string(),
            layers: layers.len(),
        });
    }

    for (index, layer_path) in layers.iter().enumerate() {
        let id = digests
            .get(index)
            .map(|d| short_layer_id(d))
            .unwrap_or_else(|| format!("{index:012}"));
        let total = fs::metadata(layer_path).map(|m| m.len()).unwrap_or(0);
        if report_extract {
            progress.event(PullEvent::LayerProgress {
                id: id.clone(),
                status: format!("Extracting ({}/{})", index + 1, layers.len()),
                current: 0,
                total,
            });
        }
        tracing::debug!(layer = %layer_path.display(), "unpacking layer");
        let data = fs::read(layer_path)?;
        apply_layer(dest, &data).map_err(|e| {
            Error::other(format!("failed to unpack {}: {e}", layer_path.display()))
        })?;
        if report_extract {
            progress.event(PullEvent::LayerProgress {
                id: id.clone(),
                status: format!("Extracting ({}/{})", index + 1, layers.len()),
                current: total,
                total,
            });
            progress.event(PullEvent::LayerStatus {
                id,
                status: "Pull complete".into(),
            });
        }
    }

    crate::unpack::finalize_rootfs(dest)
        .map_err(|e| Error::other(format!("failed to finalize rootfs links: {e}")))?;

    fs::write(&stamp_path, stamp)?;
    Ok(())
}

fn read_layer_digests(layers_dir: &Path) -> Vec<String> {
    let path = layers_dir.join("digests");
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

const ROOTFS_STAMP: &str = ".rootfs-stamp";

/// Fingerprint of the cached image contents used to decide rootfs reuse.
fn bundle_content_stamp(cache_dir: &Path, config_data: &str) -> Result<String, Error> {
    if let Ok(digest) = fs::read_to_string(cache_dir.join("digest")) {
        let digest = digest.trim();
        if !digest.is_empty() {
            let layers = fs::read_to_string(cache_dir.join("layers").join("digests"))
                .unwrap_or_default();
            let layers = layers
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            if !layers.is_empty() {
                return Ok(format!("{digest}\n{layers}\n"));
            }
            return Ok(format!("{digest}\n"));
        }
    }

    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    config_data.hash(&mut hasher);
    let layers_dir = cache_dir.join("layers");
    if layers_dir.is_dir() {
        let mut layers: Vec<_> = fs::read_dir(&layers_dir)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".tar.gz"))
            })
            .collect();
        layers.sort();
        for path in layers {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            name.hash(&mut hasher);
            len.hash(&mut hasher);
        }
    }
    Ok(format!("{:016x}", hasher.finish()))
}
