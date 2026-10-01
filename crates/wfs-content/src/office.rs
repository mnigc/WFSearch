//! OOXML/ODF zip containers: pick the text-bearing members and strip the XML
//! down to document text.
//!
//! Member selection is deliberately minimal — the main text stream of each
//! format. Headers/footers, notes and chart parts are not searched (v1).

use std::io::Read;
use zip::ZipArchive;

/// Per-member read cap: a `document.xml` can be huge, and matches beyond
/// this point are lost rather than OOM-ing the server.
const MAX_MEMBER: u64 = 32 << 20;

/// What `try_extract` decided about the bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Office {
    /// the extension is not a container we handle — fall through to text sniffing
    NotOffice,
    /// container read; the extracted document text
    Text(String),
    /// container we know but could not read (bad zip, no text member, encryption)
    Broken,
}

/// Extensions handled as zip containers. `.xlsm` shares the xlsx text member.
/// Case-insensitive: the filesystem spells extensions however it likes.
pub fn is_office(ext: &str) -> bool {
    const OFFICE: &[&str] = &[
        "docx", "docm", "xlsx", "xlsm", "pptx", "pptm", "odt", "ods", "odp",
    ];
    OFFICE.iter().any(|o| ext.eq_ignore_ascii_case(o))
}

pub fn try_extract(bytes: &[u8], ext: &str) -> Office {
    if !is_office(ext) {
        return Office::NotOffice;
    }
    let Ok(mut ar) = ZipArchive::new(std::io::Cursor::new(bytes)) else {
        return Office::Broken;
    };

    let mut out = String::new();
    let ok = match ext.to_ascii_lowercase().as_str() {
        "docx" | "docm" => read_member(&mut ar, "word/document.xml", &mut out),
        "xlsx" | "xlsm" => read_member(&mut ar, "xl/sharedStrings.xml", &mut out),
        "odt" | "ods" | "odp" => read_member(&mut ar, "content.xml", &mut out),
        "pptx" | "pptm" => {
            // slides carry the text; numeric order matters for snippet context
            let mut slides: Vec<(u32, String)> = (0..ar.len())
                .filter_map(|i| {
                    let name = ar.name_for_index(i)?;
                    let rest = name.strip_prefix("ppt/slides/slide")?;
                    let n: u32 = rest.strip_suffix(".xml")?.parse().ok()?;
                    Some((n, name.to_string()))
                })
                .collect();
            slides.sort_unstable();
            slides
                .iter()
                .all(|(_, n)| read_member(&mut ar, n, &mut out))
        }
        _ => false,
    };
    if ok && !out.is_empty() {
        Office::Text(out)
    } else {
        Office::Broken
    }
}

fn read_member<R: std::io::Read + std::io::Seek>(
    ar: &mut ZipArchive<R>,
    name: &str,
    out: &mut String,
) -> bool {
    let Ok(mut f) = ar.by_name(name) else {
        return false;
    };
    if f.encrypted() {
        return false;
    }
    let mut buf = Vec::new();
    if f.by_ref().take(MAX_MEMBER).read_to_end(&mut buf).is_err() {
        return false;
    }
    strip_xml(&buf, out);
    true
}

/// Append `src` (XML bytes) with tags removed and entities decoded to `out`.
///
/// A tag boundary emits nothing. OOXML splits phrases across `<w:t>` runs
/// (spell-check, format changes) far more often than it fuses distinct words,
/// and for CJK an inserted separator would break matching almost every time —
/// so we glue, accepting that a rare ASCII phrase spanning two runs is missed.
fn strip_xml(src: &[u8], out: &mut String) {
    let s = String::from_utf8_lossy(src);
    const CDATA_OPEN: &str = "<![CDATA[";
    const CDATA_CLOSE: &str = "]]>";
    let mut text = String::new();
    let mut in_tag = false;
    let mut i = 0usize;
    while i < s.len() {
        let c = s[i..].chars().next().unwrap();
        if in_tag {
            if c == '>' {
                in_tag = false;
            }
            i += c.len_utf8();
        } else if c == '<' && s[i..].starts_with(CDATA_OPEN) {
            // CDATA is verbatim: no tag stripping, no entity decoding — a `<`
            // or `&` inside can neither flip the tag state nor fake markup
            let body = &s[i + CDATA_OPEN.len()..];
            match body.find(CDATA_CLOSE) {
                Some(end) => {
                    decode_entities_into(&text, out);
                    text.clear();
                    out.push_str(&body[..end]);
                    i += CDATA_OPEN.len() + end + CDATA_CLOSE.len();
                }
                None => {
                    // unterminated CDATA: the rest is literal
                    decode_entities_into(&text, out);
                    text.clear();
                    out.push_str(body);
                    i = s.len();
                }
            }
        } else if c == '<' {
            in_tag = true;
            i += 1;
        } else {
            text.push(c);
            i += c.len_utf8();
        }
    }
    decode_entities_into(&text, out);
}

fn decode_entities_into(s: &str, out: &mut String) {
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let Some(semi) = after.find(';') else {
            out.push('&');
            rest = &after[1..];
            continue;
        };
        let ent = &after[1..semi];
        if ent.len() <= 10 {
            let decoded = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                e if e.starts_with("#x") || e.starts_with("#X") => u32::from_str_radix(&e[2..], 16)
                    .ok()
                    .and_then(char::from_u32),
                e if e.starts_with('#') => e[1..].parse::<u32>().ok().and_then(char::from_u32),
                _ => None,
            };
            if let Some(c) = decoded {
                out.push(c);
                rest = &after[semi + 1..];
                continue;
            }
        }
        // not a well-known entity — keep the raw text
        out.push('&');
        rest = &after[1..];
    }
    out.push_str(rest);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn member(name: &str, xml: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts: zip::write::SimpleFileOptions = Default::default();
            w.start_file(name, opts).unwrap();
            w.write_all(xml).unwrap();
            w.finish().unwrap();
        }
        buf
    }

    #[test]
    fn non_office_falls_through_and_detection_folds_case() {
        assert_eq!(try_extract(b"anything", "txt"), Office::NotOffice);
        assert_eq!(try_extract(b"anything", "cPp"), Office::NotOffice);
        // extension case no longer matters — these bytes still are not a zip
        assert_eq!(try_extract(b"anything", "DOCX"), Office::Broken);
    }

    #[test]
    fn bad_zip_is_broken() {
        assert_eq!(try_extract(b"garbage", "docx"), Office::Broken);
        // valid zip, missing text member
        assert_eq!(
            try_extract(&member("other.xml", b"<x/>"), "docx"),
            Office::Broken
        );
    }

    #[test]
    fn strips_tags_and_entities() {
        let doc = member(
            "word/document.xml",
            b"<w:p><w:t>Tom &amp; Jerry</w:t><w:t>a&lt;b</w:t></w:p>",
        );
        let Office::Text(text) = try_extract(&doc, "docx") else {
            panic!("expected text");
        };
        assert_eq!(text, "Tom & Jerrya<b");
    }

    #[test]
    fn cdata_is_verbatim_not_markup() {
        let doc = member(
            "word/document.xml",
            b"<w:p><w:t>a<![CDATA[ <b>raw & x ]]>c</w:t></w:p>",
        );
        let Office::Text(text) = try_extract(&doc, "docx") else {
            panic!("expected text");
        };
        // the `<b>` inside CDATA stays literal text; `&` is not an entity;
        // the surrounding tags still strip
        assert_eq!(text, "a <b>raw & x c");
    }

    #[test]
    fn unknown_entity_and_stray_ampersand_survive() {
        let doc = member(
            "content.xml",
            b"<text:p>caf&eacute; &#x4E2D; x&ampzz</text:p>",
        );
        let Office::Text(text) = try_extract(&doc, "odt") else {
            panic!("expected text");
        };
        // &eacute; is unknown to us and stays literal; the numeric form still
        // decodes; `&ampzz` keeps its ampersand and the rest as plain text
        assert_eq!(text, "caf&eacute; 中 x&ampzz");
    }
}
