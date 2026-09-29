//! Searching everything a session could have written for a value that must be recorded nowhere,
//! in every encoding a careless writer could have put it in.

use std::{
    fs,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

/// `text` as lowercase hex, two characters per byte.
pub fn hex(text: &str) -> String {
    text.bytes().map(|byte| format!("{byte:02x}")).collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// `bytes` in base64 over `alphabet`, without padding: a padded encoding contains the unpadded
/// one, so searching for this finds both.
fn base64_with(bytes: &[u8], alphabet: &[u8; 64]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(alphabet[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
    }
    out
}

const STANDARD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL_SAFE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Every form of `secret` a leak search looks for: the value, its hex, its base64 in both
/// alphabets, and its SHA-256 hex.
pub fn leak_forms(secret: &str) -> Vec<String> {
    vec![
        secret.to_string(),
        hex(secret),
        base64_with(secret.as_bytes(), STANDARD),
        base64_with(secret.as_bytes(), URL_SAFE),
        sha256_hex(secret.as_bytes()),
    ]
}

/// Every regular file under `root` whose bytes contain `needle`.
pub fn files_containing(root: &Path, needle: &[u8]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file()
                && fs::read(&path)
                    .is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle))
            {
                found.push(path);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_with(b"foobar", STANDARD), "Zm9vYmFy");
        assert_eq!(base64_with(b"fooba", STANDARD), "Zm9vYmE");
        assert_eq!(base64_with(&[0xfb, 0xff], STANDARD), "+/8");
        assert_eq!(base64_with(&[0xfb, 0xff], URL_SAFE), "-_8");
    }
}
