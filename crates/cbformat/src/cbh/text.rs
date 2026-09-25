//! The title of a guiding text. Unlike 2CBH, whose text header names its title
//! as an entity, the classic format keeps the titles in the text's own `.cbg`
//! record, one per language, before the text itself.

use super::bytes::text;
use super::{Database, Record};
use crate::Result;
use crate::game::RecordKind;

/// The bytes before the first title: flags, size, version and title count.
const HEAD: usize = 8;

impl Database {
    /// The title of guiding text `record`: its first title that is not blank,
    /// whatever its language; empty when it has none, and for a game. At most
    /// `limit` bytes of the record are read, and a title that ends past them
    /// ends the list.
    pub fn text_title(&self, record: &Record, limit: usize) -> Result<String> {
        if record.kind() != RecordKind::Text {
            return Ok(String::new());
        }
        let (at, size) = self.move_extent(record)?;
        let mut buf = vec![0u8; size.min(limit.max(HEAD))];
        self.moves.read_into(at, &mut buf)?;
        Ok(first_title(&buf))
    }
}

/// The first title of a text record's head that is not blank.
fn first_title(b: &[u8]) -> String {
    let Some(count) = b.get(6..HEAD).map(|c| u16::from_le_bytes([c[0], c[1]])) else { return String::new() };
    let mut at = HEAD;
    for _ in 0..count {
        let Some(len) = b.get(at + 2..at + 4).map(|l| u16::from_le_bytes([l[0], l[1]]) as usize) else { break };
        let Some(title) = b.get(at + 4..at + 4 + len) else { break };
        let t = text(title);
        if !t.trim().is_empty() {
            return t;
        }
        at += 4 + len;
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(titles: &[(u16, &[u8])]) -> Vec<u8> {
        let mut r = vec![0x80, 0, 0, 0, 1, 0];
        r.extend((titles.len() as u16).to_le_bytes());
        for (language, t) in titles {
            r.extend(language.to_le_bytes());
            r.extend((t.len() as u16).to_le_bytes());
            r.extend(*t);
        }
        r
    }

    #[test]
    fn the_first_title_that_is_not_blank() {
        assert_eq!(first_title(&record(&[(1, b"Schach"), (0, b"Chess")])), "Schach");
        assert_eq!(first_title(&record(&[(0, b" "), (1, b"Er\xf6ffnung")])), "Eröffnung");
        assert_eq!(first_title(&record(&[])), "");
        assert_eq!(first_title(&[0x80, 0, 0]), "");
        // A title cut by the read ends the list.
        let whole = record(&[(0, b""), (1, b"Endgame")]);
        assert_eq!(first_title(&whole[..whole.len() - 1]), "");
    }
}
