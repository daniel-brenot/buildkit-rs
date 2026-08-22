//! Image reference parsing.

use std::str::FromStr;

use oci_distribution::Reference;

use crate::Error;

/// Parse a user-facing image reference into an OCI [`Reference`].
///
/// Accepts Docker Hub short names (`hello-world`), fully qualified names
/// (`docker.io/library/alpine:3.19`), and scheme prefixes `docker://` /
/// `oci://`.
///
/// # Errors
///
/// Returns [`crate::Error::Reference`] when the string is not a valid OCI
/// reference.
pub fn parse_reference(image: &str) -> Result<Reference, Error> {
    let trimmed = image.trim();
    let without_scheme = trimmed
        .strip_prefix("docker://")
        .or_else(|| trimmed.strip_prefix("oci://"))
        .unwrap_or(trimmed);
    Reference::from_str(without_scheme)
        .map_err(|e| Error::Reference(format!("{without_scheme}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_docker_scheme() {
        let r = parse_reference("docker://hello-world").unwrap();
        assert_eq!(r.repository(), "library/hello-world");
    }

    #[test]
    fn bare_name_gets_library_prefix() {
        let r = parse_reference("alpine:3.19").unwrap();
        assert!(r.repository().contains("alpine"));
    }
}
