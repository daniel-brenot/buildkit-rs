//! On-disk layout for pulled images and exported builds.

use std::path::Path;
use std::path::PathBuf;

use oci_distribution::Reference;

use crate::platform::Platform;

/// On-disk layout for pulled images, exported builds, and (via [`crate::LayerCache`])
/// the instruction cache.
///
/// Create with [`Self::new`] and pass to [`crate::Buildkit::new`]. The directory
/// is created lazily as images are pulled or built:
///
/// ```text
/// <root>/
///   images/<sanitized-ref>_<os>_<arch>/
///     config.json
///     digest
///     layers/
///   cache/     # managed by LayerCache
///   work/      # temporary build directories
/// ```
#[derive(Debug, Clone)]
pub struct ImageStore {
    root: PathBuf,
}

impl ImageStore {
    /// Use `root` as the data directory. The path is not created until a write.
    pub fn new(root: PathBuf) -> Self {
        ImageStore { root }
    }

    /// Root directory passed to [`Self::new`].
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory that stores config, digest, and layer blobs for this image.
    pub fn image_dir(&self, reference: &Reference, platform: &Platform) -> PathBuf {
        self.root.join("images").join(sanitize(reference, platform))
    }

    /// Directory reserved for unpacked OCI runtime bundles of this image.
    pub fn bundle_dir(&self, reference: &Reference, platform: &Platform) -> PathBuf {
        self.root
            .join("bundles")
            .join(sanitize(reference, platform))
    }

    /// Whether `config.json` exists for this reference and platform.
    pub fn is_cached(&self, reference: &Reference, platform: &Platform) -> bool {
        self.image_dir(reference, platform)
            .join("config.json")
            .is_file()
    }
}

fn sanitize(reference: &Reference, platform: &Platform) -> String {
    let base = reference
        .to_string()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>();
    format!("{base}_{}_{}", platform.os, platform.architecture)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Platform;
    use crate::reference::parse_reference;

    #[test]
    fn sanitize_includes_platform() {
        let r = parse_reference("hello-world:latest").unwrap();
        let key = sanitize(&r, &Platform::linux_amd64());
        assert!(key.contains("hello_world"));
        assert!(key.ends_with("linux_amd64"));
    }
}
