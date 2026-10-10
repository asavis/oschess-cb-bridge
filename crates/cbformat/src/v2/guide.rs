//! The body of a 2CBH guiding text (asavis/oschess-cb-bridge#324): a
//! `.2cbg` record under the tag `00 10` holding one HTML document per
//! language, as Morphy's `format/v2/2-moves.md` ("Guiding texts") describes.

use super::{Database, Record};
use crate::bytes::Cursor;
use crate::cbh::annotations::language_of;
use crate::codepage::utf8_or_legacy;
use crate::game::RecordKind;
use crate::game::guide::{Body, Content, GuidingText};
use crate::{Error, Result};

/// The tag of a guiding text's record, the bytes `00 10`.
pub const TEXT_TAG: u16 = 0x1000;

impl Database {
    /// The body of guiding text `record`, its `.2cbg` record read within
    /// `limit` bytes: the HTML of each language that has any, in stored order.
    /// An error for a record that is not a guiding text, and for one whose
    /// record does not hold its documents as the format says.
    pub fn guiding_text(&self, record: &Record, limit: usize) -> Result<GuidingText> {
        let id = record.id();
        if record.kind() != RecordKind::Text {
            return Err(Error::Format(format!("record {id} is not a guiding text")));
        }
        let data = self.moves_of_within(record, limit)?;
        if data.tag() != TEXT_TAG {
            return Err(Error::Format(format!("guiding text {id}: a record of tag {:#06x}", data.tag())));
        }
        read(data.content()).map_err(|what| Error::Format(format!("guiding text {id}: {what}")))
    }
}

/// The content of a text record: a `u16` version, the `u32` size of the rest,
/// the `u32` number of entries, and each entry a nation (as a text annotation
/// names its language), the `u32` size of the HTML and the HTML. ChessBase
/// writes an entry for each of its languages, empty where the text has none;
/// those are left out.
pub(crate) fn read(b: &[u8]) -> std::result::Result<GuidingText, &'static str> {
    let mut c = Cursor::new(b);
    c.le_u16().ok_or("no version")?;
    c.le_u32().ok_or("no size")?;
    let count = c.le_u32().ok_or("no entries")?;
    let mut contents = Vec::new();
    for _ in 0..count {
        let nation = c.le_i32().ok_or("an entry is cut")?;
        let n = usize::try_from(c.le_u32().ok_or("an entry is cut")?).map_err(|_| "an entry is cut")?;
        let html = c.take(n).ok_or("an entry is cut")?;
        if html.is_empty() {
            continue;
        }
        // A nation past a byte is none ChessBase names; it keeps a number
        // above every language, as `language_of` gives other nations.
        let language = u8::try_from(nation).map_or(0x100 + 0xff, language_of);
        contents.push(Content { language, body: Body::Html(utf8_or_legacy(html)) });
    }
    Ok(GuidingText { contents })
}
