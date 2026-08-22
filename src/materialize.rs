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
pub fn materialize_rootfs<S: ImageStore>(
    store: &S,
    reference: &oci_distribution::Reference,
    platform: &crate::platform::Platform,
    dest: &Path,
) -> Result<(), Error> {
    materialize_rootfs_with_progress(store, reference, platform, dest, &mut NullProgress, false)
}

/// Like [`materialize_rootfs`], optionally emitting Docker-style extract progress.
pub fn materialize_rootfs_with_progress<S: ImageStore>(
    store: &S,
    reference: &oci_distribution::Reference,
    platform: &crate::platform::Platform,
    dest: &Path,
    progress: &mut dyn PullProgress,
    report_extract: bool,
) -> Result<(), Error> {
    if !store.is_cached(reference, platform) {
        return Err(Error::other(format!(
            "image {reference} is not cached; pull it first"
        )));
    }

    let image_config = store.image_config(reference, platform)?;
    check_runnable_platform(&image_config)?;

    let stamp = store.content_stamp(reference, platform)?;
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

    let n = store.layer_count(reference, platform);
    let digests = store.layer_digests(reference, platform);

    if report_extract && n > 0 {
        progress.event(PullEvent::Unpacking {
            reference: reference.to_string(),
            layers: n,
        });
    }

    for index in 0..n {
        let id = digests
            .get(index)
            .map(|d| short_layer_id(d))
            .unwrap_or_else(|| format!("{index:012}"));
        let total = store.layer_size(reference, platform, index);
        if report_extract {
            progress.event(PullEvent::LayerProgress {
                id: id.clone(),
                status: format!("Extracting ({}/{})", index + 1, n),
                current: 0,
                total,
            });
        }
        tracing::debug!(layer = index, "unpacking layer");
        let data = store.read_layer(reference, platform, index)?;
        apply_layer(dest, &data)
            .map_err(|e| Error::other(format!("failed to unpack layer {index}: {e}")))?;
        if report_extract {
            progress.event(PullEvent::LayerProgress {
                id: id.clone(),
                status: format!("Extracting ({}/{})", index + 1, n),
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

const ROOTFS_STAMP: &str = ".rootfs-stamp";
