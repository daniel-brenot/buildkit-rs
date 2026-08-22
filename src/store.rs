//! Storage for pulled and exported image configs and layer blobs.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use oci_distribution::config::ConfigFile;
use oci_distribution::Reference;

use crate::platform::Platform;
use crate::Error;

/// Storage for pulled and exported images.
///
/// [`Buildkit`](`crate::Buildkit`) reads and writes configs and layer blobs
/// through this trait. [`LocalImageStore`] is the default: Docker's data-root
/// layout on the local filesystem. Implement this to control how blobs are
/// stored (another directory layout, a content-addressed CAS, a remote store).
///
/// Instruction snapshots still use the overlay2 cache under [`Self::root`];
/// that path must be a writable host directory because `RUN` / `COPY` operate
/// on real files. Only image blobs and metadata go through these methods.
pub trait ImageStore: Send + Sync {
    /// Host directory for overlay2 instruction cache and temporary build work.
    fn root(&self) -> &Path;

    /// Whether config and layers for this reference and platform are present.
    fn is_cached(&self, reference: &Reference, platform: &Platform) -> bool;

    /// Manifest digest (pull) or layer digest (export), if stored.
    fn image_digest(&self, reference: &Reference, platform: &Platform) -> Option<String>;

    /// Image config for this reference and platform.
    fn image_config(&self, reference: &Reference, platform: &Platform)
        -> Result<ConfigFile, Error>;

    /// Layer digests in manifest order, when the store recorded them.
    fn layer_digests(&self, reference: &Reference, platform: &Platform) -> Vec<String>;

    /// Number of stored layer blobs.
    fn layer_count(&self, reference: &Reference, platform: &Platform) -> usize;

    /// Whether layer blob `index` (0-based) exists.
    fn has_layer(&self, reference: &Reference, platform: &Platform, index: usize) -> bool;

    /// Read layer blob `index` (0-based, manifest order).
    fn read_layer(
        &self,
        reference: &Reference,
        platform: &Platform,
        index: usize,
    ) -> Result<Vec<u8>, Error>;

    /// Size in bytes of layer blob `index` (0 if missing).
    fn layer_size(&self, reference: &Reference, platform: &Platform, index: usize) -> u64;

    /// Persist a pulled or exported image, replacing any previous copy.
    fn put_image(
        &self,
        reference: &Reference,
        platform: &Platform,
        image: StoredImage,
    ) -> Result<(), Error>;

    /// Persist a single-layer image by copying `layer_path` (avoids loading
    /// large blobs into memory). The default reads the file and calls
    /// [`Self::put_image`].
    fn put_image_layer_file(
        &self,
        reference: &Reference,
        platform: &Platform,
        digest: &str,
        config: &ConfigFile,
        layer_path: &Path,
    ) -> Result<(), Error> {
        let data = fs::read(layer_path)?;
        self.put_image(
            reference,
            platform,
            StoredImage {
                digest: Some(digest.to_string()),
                config: config.clone(),
                layers: vec![StoredLayer {
                    digest: Some(digest.to_string()),
                    data,
                }],
            },
        )
    }

    /// Whether the stored digest and layer blobs match `digest` / `layer_digests`.
    fn layers_match(
        &self,
        reference: &Reference,
        platform: &Platform,
        digest: &str,
        layer_digests: &[String],
    ) -> bool {
        if digest.is_empty() {
            return false;
        }
        let Some(local) = self.image_digest(reference, platform) else {
            return false;
        };
        if local.trim() != digest.trim() || !self.is_cached(reference, platform) {
            return false;
        }
        let stored = self.layer_digests(reference, platform);
        if stored.len() != layer_digests.len() {
            return false;
        }
        stored
            .iter()
            .zip(layer_digests)
            .enumerate()
            .all(|(i, (got, expected))| got == expected && self.has_layer(reference, platform, i))
    }

    /// Whether this tagged image already stores `digest` and at least one layer.
    fn has_digest(&self, reference: &Reference, platform: &Platform, digest: &str) -> bool {
        self.image_digest(reference, platform)
            .as_deref()
            .map(str::trim)
            == Some(digest.trim())
            && self.has_layer(reference, platform, 0)
    }

    /// Fingerprint of stored content, used to reuse a materialized rootfs.
    fn content_stamp(&self, reference: &Reference, platform: &Platform) -> Result<String, Error> {
        if let Some(digest) = self.image_digest(reference, platform) {
            let layers = self.layer_digests(reference, platform);
            if !layers.is_empty() {
                return Ok(format!("{digest}\n{}\n", layers.join("\n")));
            }
            return Ok(format!("{digest}\n"));
        }

        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let config = self.image_config(reference, platform)?;
        let config_data = serde_json::to_string(&config)?;
        let mut hasher = DefaultHasher::new();
        config_data.hash(&mut hasher);
        let n = self.layer_count(reference, platform);
        for i in 0..n {
            i.hash(&mut hasher);
            self.layer_size(reference, platform, i).hash(&mut hasher);
        }
        Ok(format!("{:016x}", hasher.finish()))
    }
}

/// An image to persist: config plus ordered layer blobs.
#[derive(Debug, Clone)]
pub struct StoredImage {
    /// Manifest digest (pull) or packed-layer digest (export).
    pub digest: Option<String>,
    /// OCI image config.
    pub config: ConfigFile,
    /// Layer blobs in manifest / export order.
    pub layers: Vec<StoredLayer>,
}

/// One compressed layer blob (`application/vnd.oci.image.layer.v1.tar+gzip`).
#[derive(Debug, Clone)]
pub struct StoredLayer {
    /// OCI compressed-layer digest (`sha256:…`) when known.
    pub digest: Option<String>,
    /// Gzip-compressed tar bytes.
    pub data: Vec<u8>,
}

/// Filesystem image store using Docker's data-root layout.
///
/// ```text
/// <root>/
///   images/<sanitized-ref>_<os>_<arch>/
///     config.json
///     digest
///     layers/
///   overlay2/  # instruction cache (not part of this trait)
///   work/      # temporary build directories
/// ```
#[derive(Debug, Clone)]
pub struct LocalImageStore {
    root: PathBuf,
}

impl LocalImageStore {
    /// Use `root` as the data directory. The path is not created until a write.
    pub fn new(root: PathBuf) -> Self {
        LocalImageStore { root }
    }

    /// Docker's data-root on this host (parent of `overlay2/`).
    ///
    /// Honors `DOCKER_DATA_ROOT` when set. Otherwise:
    ///
    /// - **Linux** — `/var/lib/docker` when running as root, else
    ///   `~/.local/share/docker` (rootless)
    /// - **macOS** — `/var/lib/docker` (Docker Desktop's Linux VM); if that
    ///   directory is missing, `~/Library/Application Support/docker`
    /// - **Windows** — `\\wsl$\docker-desktop-data\data\docker` when that
    ///   distro is present, else `%ProgramData%\Docker`
    pub fn default_root() -> PathBuf {
        docker_data_root()
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

    fn layers_dir(&self, reference: &Reference, platform: &Platform) -> PathBuf {
        self.image_dir(reference, platform).join("layers")
    }

    fn layer_path(&self, reference: &Reference, platform: &Platform, index: usize) -> PathBuf {
        self.layers_dir(reference, platform)
            .join(format!("{index}.tar.gz"))
    }
}

impl Default for LocalImageStore {
    fn default() -> Self {
        Self::new(Self::default_root())
    }
}

impl ImageStore for LocalImageStore {
    fn root(&self) -> &Path {
        &self.root
    }

    fn is_cached(&self, reference: &Reference, platform: &Platform) -> bool {
        self.image_dir(reference, platform)
            .join("config.json")
            .is_file()
    }

    fn image_digest(&self, reference: &Reference, platform: &Platform) -> Option<String> {
        fs::read_to_string(self.image_dir(reference, platform).join("digest"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn image_config(
        &self,
        reference: &Reference,
        platform: &Platform,
    ) -> Result<ConfigFile, Error> {
        let path = self.image_dir(reference, platform).join("config.json");
        let data = fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
        serde_json::from_str(&data).map_err(|e| Error::other(format!("cached image config: {e}")))
    }

    fn layer_digests(&self, reference: &Reference, platform: &Platform) -> Vec<String> {
        let path = self.layers_dir(reference, platform).join("digests");
        let Ok(text) = fs::read_to_string(path) else {
            return Vec::new();
        };
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn layer_count(&self, reference: &Reference, platform: &Platform) -> usize {
        let dir = self.layers_dir(reference, platform);
        let Ok(entries) = fs::read_dir(&dir) else {
            return 0;
        };
        entries
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.ends_with(".tar.gz"))
            })
            .count()
    }

    fn has_layer(&self, reference: &Reference, platform: &Platform, index: usize) -> bool {
        self.layer_path(reference, platform, index).is_file()
    }

    fn read_layer(
        &self,
        reference: &Reference,
        platform: &Platform,
        index: usize,
    ) -> Result<Vec<u8>, Error> {
        let path = self.layer_path(reference, platform, index);
        fs::read(&path).map_err(|e| Error::io(&path, e))
    }

    fn layer_size(&self, reference: &Reference, platform: &Platform, index: usize) -> u64 {
        fs::metadata(self.layer_path(reference, platform, index))
            .map(|m| m.len())
            .unwrap_or(0)
    }

    fn put_image(
        &self,
        reference: &Reference,
        platform: &Platform,
        image: StoredImage,
    ) -> Result<(), Error> {
        let dir = self.image_dir(reference, platform);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        let layers_dir = dir.join("layers");
        fs::create_dir_all(&layers_dir)?;
        let mut digest_lines = Vec::new();
        let mut all_digests = true;
        for (index, layer) in image.layers.iter().enumerate() {
            fs::write(layers_dir.join(format!("{index}.tar.gz")), &layer.data)?;
            match &layer.digest {
                Some(d) => digest_lines.push(d.clone()),
                None => all_digests = false,
            }
        }
        if all_digests && !digest_lines.is_empty() {
            fs::write(layers_dir.join("digests"), digest_lines.join("\n") + "\n")?;
        }
        fs::write(
            dir.join("config.json"),
            serde_json::to_string_pretty(&image.config)?,
        )?;
        fs::write(
            dir.join("platform"),
            format!("{}/{}", platform.os, platform.architecture),
        )?;
        if let Some(digest) = image.digest.filter(|d| !d.is_empty()) {
            fs::write(dir.join("digest"), digest)?;
        }
        Ok(())
    }

    fn put_image_layer_file(
        &self,
        reference: &Reference,
        platform: &Platform,
        digest: &str,
        config: &ConfigFile,
        layer_path: &Path,
    ) -> Result<(), Error> {
        let dir = self.image_dir(reference, platform);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        let layers_dir = dir.join("layers");
        fs::create_dir_all(&layers_dir)?;
        fs::copy(layer_path, layers_dir.join("0.tar.gz"))?;
        fs::write(layers_dir.join("digests"), format!("{digest}\n"))?;
        fs::write(
            dir.join("config.json"),
            serde_json::to_string_pretty(config)?,
        )?;
        fs::write(
            dir.join("platform"),
            format!("{}/{}", platform.os, platform.architecture),
        )?;
        fs::write(dir.join("digest"), digest)?;
        Ok(())
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

fn docker_data_root() -> PathBuf {
    if let Ok(root) = std::env::var("DOCKER_DATA_ROOT") {
        if !root.is_empty() {
            return PathBuf::from(root);
        }
    }
    #[cfg(target_os = "linux")]
    {
        if unix_is_root() {
            return PathBuf::from("/var/lib/docker");
        }
        return data_local_docker();
    }
    #[cfg(target_os = "macos")]
    {
        let vm = PathBuf::from("/var/lib/docker");
        if vm.is_dir() {
            return vm;
        }
        return data_local_docker();
    }
    #[cfg(windows)]
    {
        let wsl = PathBuf::from(r"\\wsl$\docker-desktop-data\data\docker");
        if wsl.is_dir() {
            return wsl;
        }
        let wsl_localhost = PathBuf::from(r"\\wsl.localhost\docker-desktop-data\data\docker");
        if wsl_localhost.is_dir() {
            return wsl_localhost;
        }
        return std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
            .join("Docker");
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        PathBuf::from("/var/lib/docker")
    }
}

#[cfg(target_os = "linux")]
fn unix_is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn data_local_docker() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.data_local_dir().join("docker"))
        .unwrap_or_else(|| PathBuf::from("/var/lib/docker"))
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

    #[test]
    fn default_root_is_docker_data_dir() {
        let root = LocalImageStore::default_root();
        assert!(root.is_absolute() || root.to_string_lossy().starts_with(r"\\"));
        let name = root.file_name().and_then(|n| n.to_str()).unwrap_or("");
        assert!(
            name.eq_ignore_ascii_case("docker"),
            "expected a Docker data-root, got {}",
            root.display()
        );
    }
}
