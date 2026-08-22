//! Encode Linux path components so they can live on Windows filesystems.
//!
//! NTFS rejects `:<>"|?*`, reserved device names (`CON`, `NUL`, …), and trailing
//! dots/spaces. Debian multiarch files like `libfoo:amd64.list` are common in
//! container layers — unsafe components are hex-encoded as `#bk#` + uppercase hex
//! so unpack succeeds and the original names can be recovered.

use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};

const ESCAPE_PREFIX: &str = "#bk#";

/// True when this single path component is legal on Windows NTFS.
pub fn component_ok_on_windows(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return true;
    }
    if name.ends_with(' ') || name.ends_with('.') {
        return false;
    }
    if name.chars().any(|c| {
        matches!(
            c,
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '\0'..='\x1f'
        )
    }) {
        return false;
    }
    let stem = name.split('.').next().unwrap_or(name);
    !matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

/// Encode one path component for storage on the host.
pub fn encode_component(name: &str) -> Cow<'_, str> {
    #[cfg(not(windows))]
    {
        Cow::Borrowed(name)
    }
    #[cfg(windows)]
    {
        if component_ok_on_windows(name) {
            return Cow::Borrowed(name);
        }
        let mut out = String::with_capacity(ESCAPE_PREFIX.len() + name.len() * 2);
        out.push_str(ESCAPE_PREFIX);
        for b in name.as_bytes() {
            out.push_str(&format!("{b:02X}"));
        }
        Cow::Owned(out)
    }
}

/// Decode a host path component back to the Linux name.
#[allow(dead_code)]
pub fn decode_component(name: &str) -> Cow<'_, str> {
    #[cfg(not(windows))]
    {
        Cow::Borrowed(name)
    }
    #[cfg(windows)]
    {
        let Some(hex) = name.strip_prefix(ESCAPE_PREFIX) else {
            return Cow::Borrowed(name);
        };
        if hex.is_empty() || hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Cow::Borrowed(name);
        }
        let mut bytes = Vec::with_capacity(hex.len() / 2);
        let chars: Vec<char> = hex.chars().collect();
        for chunk in chars.chunks(2) {
            let s: String = chunk.iter().collect();
            match u8::from_str_radix(&s, 16) {
                Ok(b) => bytes.push(b),
                Err(_) => return Cow::Borrowed(name),
            }
        }
        match String::from_utf8(bytes) {
            Ok(s) => Cow::Owned(s),
            Err(_) => Cow::Borrowed(name),
        }
    }
}

/// Join `root` with a Linux-relative path, encoding each component on Windows.
pub fn join_root(root: &Path, guest_rel: &str) -> PathBuf {
    let mut out = root.to_path_buf();
    push_guest_rel(&mut out, guest_rel);
    out
}

/// Append a Linux-relative path onto an existing host path.
pub fn push_guest_rel(host: &mut PathBuf, guest_rel: &str) {
    for part in guest_rel.split(['/', '\\']) {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            let _ = host.pop();
            continue;
        }
        host.push(encode_component(part).as_ref());
    }
}

/// Convert a path relative to `root` back into a Linux-relative path string.
#[allow(dead_code)]
pub fn host_rel_to_guest(rel: &Path) -> String {
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => {
                parts.push(decode_component(&s.to_string_lossy()).into_owned());
            }
            Component::CurDir => {}
            Component::ParentDir => parts.push("..".into()),
            _ => {}
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names_unchanged() {
        assert_eq!(encode_component("passwd").as_ref(), "passwd");
        assert_eq!(decode_component("passwd").as_ref(), "passwd");
    }

    #[test]
    fn colon_roundtrip() {
        let name = "libapr1:amd64.list";
        let enc = encode_component(name);
        #[cfg(windows)]
        {
            assert!(enc.starts_with(ESCAPE_PREFIX));
            assert_ne!(enc.as_ref(), name);
        }
        assert_eq!(decode_component(enc.as_ref()).as_ref(), name);
    }

    #[test]
    fn reserved_con_encoded() {
        let enc = encode_component("CON");
        #[cfg(windows)]
        assert!(enc.starts_with(ESCAPE_PREFIX));
        assert_eq!(decode_component(enc.as_ref()).as_ref(), "CON");
    }
}
