//! OCI platform selection for pulls and builds.

use oci_distribution::manifest::ImageIndexEntry;

/// OCI platform selector, for example `linux/amd64`.
///
/// Only Linux images can be unpacked. [`Self::parse`] rejects `windows/*` and
/// `darwin/*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    /// OS name from the image index (`linux`).
    pub os: String,
    /// CPU architecture (`amd64`, `arm64`, …).
    pub architecture: String,
}

impl Platform {
    /// `linux/amd64`.
    pub fn linux_amd64() -> Self {
        Platform {
            os: "linux".into(),
            architecture: "amd64".into(),
        }
    }

    /// Parse `os/architecture` (case-insensitive).
    ///
    /// # Errors
    ///
    /// Returns a message if the string is not `os/arch` or if `os` is not
    /// `linux`.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        let (os, arch) = spec
            .split_once('/')
            .ok_or_else(|| format!("expected os/architecture, got {spec:?}"))?;
        if os.is_empty() || arch.is_empty() {
            return Err(format!("invalid platform {spec:?}"));
        }
        let os = os.to_lowercase();
        let architecture = arch.to_lowercase();
        if os != "linux" {
            return Err(format!(
                "only linux/* platforms are supported (got {os}/{architecture})"
            ));
        }
        Ok(Platform { os, architecture })
    }
}

/// Default platform for pulls and builds.
///
/// `linux/arm64` on Apple Silicon; `linux/amd64` on every other host
/// (including Windows, where images still run through a Linux ABI layer).
pub fn default_pull_platform() -> Platform {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Platform {
            os: "linux".into(),
            architecture: "arm64".into(),
        }
    }
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        Platform::linux_amd64()
    }
}

/// Build an `oci-distribution` platform resolver that picks a manifest for
/// `platform` from an image index.
pub fn platform_resolver(
    platform: Platform,
) -> Box<dyn Fn(&[ImageIndexEntry]) -> Option<String> + Send + Sync> {
    Box::new(move |manifests: &[ImageIndexEntry]| {
        manifests
            .iter()
            .find(|entry| {
                entry.platform.as_ref().map_or(false, |p| {
                    p.os == platform.os && p.architecture == platform.architecture
                })
            })
            .map(|entry| entry.digest.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_linux_amd64() {
        let p = Platform::parse("linux/amd64").unwrap();
        assert_eq!(p.os, "linux");
        assert_eq!(p.architecture, "amd64");
    }

    #[test]
    fn rejects_non_linux_platforms() {
        assert!(Platform::parse("windows/amd64").is_err());
        assert!(Platform::parse("darwin/arm64").is_err());
    }
}
