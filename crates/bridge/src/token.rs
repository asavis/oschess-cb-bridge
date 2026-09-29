//! The bridge's pairing token, kept in its data folder
//! ([`crate::folders::data_dir`]).

use std::io;
use std::path::Path;

/// Characters in a token: 256 bits as unpadded base64url.
pub const TOKEN_LEN: usize = 43;
const TOKEN_FILE: &str = "token";

/// Whether `dir` holds a token already; it does not on the bridge's first run.
pub fn exists(dir: &Path) -> bool {
    dir.join(TOKEN_FILE).exists()
}

/// The token stored in `dir`, created on first use.
pub fn load_or_create(dir: &Path) -> io::Result<String> {
    match load(dir)? {
        Some(token) => Ok(token),
        None => replace(dir),
    }
}

/// The token stored in `dir`, or `None` when there is none yet. It never
/// creates one: that is left to the bridge's first start (#183).
pub fn load(dir: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(dir.join(TOKEN_FILE)) {
        Ok(text) if is_valid(text.trim()) => Ok(Some(text.trim().to_string())),
        Ok(_) => Err(io::Error::new(io::ErrorKind::InvalidData, "the stored pairing token is malformed")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Writes a new token to `dir`, replacing any earlier one, and returns it.
pub fn replace(dir: &Path) -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(format!("no operating-system randomness: {e}")))?;
    let token = base64url(&bytes);
    std::fs::create_dir_all(dir)?;
    // Replaced whole: a stop halfway leaves the old token, never a cut one
    // that would keep the bridge from starting (#62).
    crate::files::write_private_atomic(&dir.join(TOKEN_FILE), token.as_bytes())?;
    Ok(token)
}

pub fn is_valid(token: &str) -> bool {
    token.len() == TOKEN_LEN && is_base64url(token)
}

/// Whether `s` holds only the characters of base64url: `A-Z`, `a-z`, `0-9`,
/// `-` and `_`. A client's stream name is made of the same (`docs/api.md`,
/// "Cancellation").
pub(crate) fn is_base64url(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
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
        assert_eq!(load(&dir).unwrap(), None);
        assert!(!exists(&dir), "reading creates no token");
        let first = load_or_create(&dir).unwrap();
        assert!(exists(&dir));
        assert!(is_valid(&first));
        assert_eq!(load(&dir).unwrap().as_deref(), Some(first.as_str()));
        assert_eq!(load_or_create(&dir).unwrap(), first);
        let second = replace(&dir).unwrap();
        assert_ne!(second, first);
        assert_eq!(load_or_create(&dir).unwrap(), second);
        // Replaced whole, still private, and nothing else left beside it (#62).
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, [TOKEN_FILE]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(TOKEN_FILE)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(dir.join(TOKEN_FILE), "short").unwrap();
        assert!(load_or_create(&dir).is_err());
        assert!(load(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
