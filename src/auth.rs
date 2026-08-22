//! Registry authentication from Docker-style `config.json` and environment variables.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::RwLock;

use base64::Engine;
use directories::UserDirs;
use oci_distribution::secrets::RegistryAuth;
use oci_distribution::Reference;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct DockerConfig {
    #[serde(default)]
    auths: HashMap<String, DockerAuthEntry>,
}

#[derive(Debug, Deserialize)]
struct DockerAuthEntry {
    auth: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

static CONFIG_DIR_OVERRIDE: RwLock<Option<PathBuf>> = RwLock::new(None);

/// Override the client config directory used for registry auth (`config.json`).
///
/// When `None`, the default search order is restored: `DOCKER_CONFIG`,
/// `BUILDKIT_CONFIG`, then `~/.docker`. Pass `Some` to force a directory
/// (tests, isolated clients). Docker itself does not need to be installed.
pub fn set_config_dir(path: Option<PathBuf>) {
    let mut guard = CONFIG_DIR_OVERRIDE
        .write()
        .unwrap_or_else(|e| e.into_inner());
    *guard = path;
}

fn docker_config_path() -> Option<PathBuf> {
    {
        let guard = CONFIG_DIR_OVERRIDE
            .read()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(dir) = guard.as_ref() {
            return Some(dir.join("config.json"));
        }
    }
    if let Ok(path) = env::var("DOCKER_CONFIG") {
        return Some(PathBuf::from(path).join("config.json"));
    }
    if let Ok(path) = env::var("BUILDKIT_CONFIG") {
        return Some(PathBuf::from(path).join("config.json"));
    }
    UserDirs::new().map(|dirs| dirs.home_dir().join(".docker").join("config.json"))
}

fn decode_basic(auth: &str) -> Option<(String, String)> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(auth)
        .ok()?;
    let pair = String::from_utf8(decoded).ok()?;
    let (user, pass) = pair.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

fn auth_from_docker_config(reference: &Reference) -> Option<RegistryAuth> {
    let path = docker_config_path()?;
    let data = fs::read_to_string(&path).ok()?;
    let config: DockerConfig = serde_json::from_str(&data).ok()?;
    let registry = reference.registry();
    let candidates = [
        format!("https://{registry}"),
        format!("https://{registry}/v1/"),
        format!("https://{registry}/v2/"),
        "https://index.docker.io/v1/".to_string(),
    ];
    for key in candidates {
        if let Some(entry) = config.auths.get(&key) {
            if let Some((user, pass)) = entry
                .auth
                .as_deref()
                .and_then(decode_basic)
                .or_else(|| entry.username.clone().zip(entry.password.clone()))
            {
                return Some(RegistryAuth::Basic(user, pass));
            }
        }
    }
    None
}

/// Resolve registry credentials for `reference`.
///
/// Search order:
/// 1. `config.json` under [`set_config_dir`], `DOCKER_CONFIG`, `BUILDKIT_CONFIG`,
///    or `~/.docker` (file only — Docker need not be installed)
/// 2. `BUILDKIT_REGISTRY_USER` / `BUILDKIT_REGISTRY_PASSWORD`
/// 3. Anonymous
pub fn auth_for_reference(reference: &Reference) -> RegistryAuth {
    if let Some(auth) = auth_from_docker_config(reference) {
        return auth;
    }
    if let (Ok(user), Ok(pass)) = (
        env::var("BUILDKIT_REGISTRY_USER"),
        env::var("BUILDKIT_REGISTRY_PASSWORD"),
    ) {
        return RegistryAuth::Basic(user, pass);
    }
    RegistryAuth::Anonymous
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_docker_basic_auth() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("alice:secret");
        let (user, pass) = decode_basic(&encoded).unwrap();
        assert_eq!(user, "alice");
        assert_eq!(pass, "secret");
    }
}
