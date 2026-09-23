//! The bridge's data folder and its pairing token.

use std::io;
use std::path::{Path, PathBuf};

/// Characters in a token: 256 bits as unpadded base64url.
pub const TOKEN_LEN: usize = 43;
const TOKEN_FILE: &str = "token";

/// Where the bridge keeps its settings and token: `OSCHESS_BRIDGE_HOME` when
/// set, else `%APPDATA%\oschess-bridge` on Windows and
/// `$XDG_CONFIG_HOME/oschess-bridge` or `~/.config/oschess-bridge` elsewhere.
pub fn data_dir() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(dir) = var("OSCHESS_BRIDGE_HOME") {
        return Some(dir);
    }
    let base = if cfg!(windows) {
        var("APPDATA")
    } else {
        var("XDG_CONFIG_HOME").or_else(|| var("HOME").map(|h| h.join(".config")))
    };
    base.map(|b| b.join("oschess-bridge"))
}

/// Whether `dir` holds a token already; it does not on the bridge's first run.
pub fn exists(dir: &Path) -> bool {
    dir.join(TOKEN_FILE).exists()
}

/// The token stored in `dir`, created on first use.
pub fn load_or_create(dir: &Path) -> io::Result<String> {
    match std::fs::read_to_string(dir.join(TOKEN_FILE)) {
        Ok(text) if is_valid(text.trim()) => Ok(text.trim().to_string()),
        Ok(_) => Err(io::Error::new(io::ErrorKind::InvalidData, "the stored pairing token is malformed")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => replace(dir),
        Err(e) => Err(e),
    }
}

/// Writes a new token to `dir`, replacing any earlier one, and returns it.
pub fn replace(dir: &Path) -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(format!("no operating-system randomness: {e}")))?;
    let token = base64url(&bytes);
    std::fs::create_dir_all(dir)?;
    write_private(&dir.join(TOKEN_FILE), token.as_bytes())?;
    Ok(token)
}

/// Writes `bytes` to a file only its owner can read. `%APPDATA%` is already
/// private to the user on Windows; elsewhere the file is created mode 0600.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    io::Write::write_all(&mut options.open(path)?, bytes)
}

pub fn is_valid(token: &str) -> bool {
    token.len() == TOKEN_LEN && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | (u32::from(b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_without_padding() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(base64url(&[0u8; 32]).len(), TOKEN_LEN);
    }

    #[test]
    fn token_is_created_once_and_kept() {
        let dir = std::env::temp_dir().join(format!("bridge-token-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!exists(&dir));
        let first = load_or_create(&dir).unwrap();
        assert!(exists(&dir));
        assert!(is_valid(&first));
        assert_eq!(load_or_create(&dir).unwrap(), first);
        let second = replace(&dir).unwrap();
        assert_ne!(second, first);
        assert_eq!(load_or_create(&dir).unwrap(), second);
        std::fs::write(dir.join(TOKEN_FILE), "short").unwrap();
        assert!(load_or_create(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
