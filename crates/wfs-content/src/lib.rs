//! Document-content extraction and matching for WFSearch.
//!
//! Pure logic — bytes in, verdict out, no file IO (the server reads the bytes,
//! bounded by config, and hands them over). Two shapes are understood:
//!
//! - **Plain text**: BOM-detected UTF-8/UTF-16, otherwise lossy UTF-8 with a
//!   NUL-byte sniff to reject binaries (`crates/wfs-content/src/text.rs`)
//! - **OOXML/ODF zip containers** (`.docx`/`.xlsx`/`.pptx`/`.xlsm` and the
//!   ODF family): the document XML members are unzipped and stripped to text
//!   (`crates/wfs-content/src/office.rs`)
//!
//! Matching reuses the engine's foldcase `Substr` (SIMD fast path, CJK
//! included), so `content:预算` behaves exactly like a name search would.

pub mod office;
pub mod text;

use wfs_core::Substr;

/// Window kept around the first hit when building a snippet.
const SNIPPET_RADIUS: usize = 64;
/// Cap on the reported per-file match count (counting keeps scanning the
/// whole text; this bounds the work for pathological files).
const MAX_COUNT: u32 = 10_000;

/// Verdict for one file's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scan {
    /// binary content — rejected before matching
    Binary,
    /// a container we know but could not read (corrupt zip, encrypted member)
    Error,
    /// text extracted, no needle matched
    NoMatch,
    Match {
        /// context around the first hit, whitespace collapsed, `…`-elided
        snippet: String,
        /// total hits across all terms
        count: u32,
    },
}

/// Compiled `content:` terms — foldcase substrings, AND semantics.
#[derive(Debug, Clone)]
pub struct ContentMatcher {
    needles: Vec<Substr>,
}

impl ContentMatcher {
    pub fn new(terms: &[String]) -> ContentMatcher {
        ContentMatcher {
            needles: terms.iter().map(|t| Substr::new(t)).collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.needles.is_empty()
    }

    /// Earliest (needle, byte-offset) hit in `text` — every needle must hit
    /// somewhere (AND); the earliest position provides the snippet context.
    fn first_hit(&self, text: &str) -> Option<usize> {
        let bytes = text.as_bytes();
        let mut best: Option<usize> = None;
        for n in &self.needles {
            let p = n.find(bytes)?;
            if best.is_none_or(|bp| p < bp) {
                best = Some(p);
            }
        }
        best
    }

    /// Total hits across all needles, capped at `MAX_COUNT`.
    fn count_hits(&self, text: &str) -> u32 {
        let bytes = text.as_bytes();
        let mut total = 0u32;
        for n in &self.needles {
            let mut off = 0usize;
            while total < MAX_COUNT {
                match n.find(&bytes[off..]) {
                    Some(p) => {
                        total += 1;
                        off += p + 1;
                    }
                    None => break,
                }
            }
            if total >= MAX_COUNT {
                break;
            }
        }
        total
    }

    /// Extract text from `bytes` (dispatching on the lowercase, dot-less file
    /// extension) and match the compiled terms against it.
    pub fn scan(&self, bytes: &[u8], ext: &str) -> Scan {
        if self.is_empty() {
            return Scan::NoMatch;
        }
        let text = match office::try_extract(bytes, ext) {
            office::Office::NotOffice => match text::decode(bytes) {
                Some(t) => t,
                None => return Scan::Binary,
            },
            office::Office::Text(t) => t,
            office::Office::Broken => return Scan::Error,
        };
        let Some(hit) = self.first_hit(&text) else {
            return Scan::NoMatch;
        };
        Scan::Match {
            snippet: snippet_around(&text, hit, SNIPPET_RADIUS),
            count: self.count_hits(&text),
        }
    }
}

/// Context around the first hit: up to `radius` bytes each side on char
/// boundaries, internal whitespace collapsed to single spaces, `…` marking
/// elided head/tail.
pub fn snippet_around(text: &str, hit: usize, radius: usize) -> String {
    let start = floor_boundary(text, hit.saturating_sub(radius));
    let end = ceil_boundary(text, (hit + radius).min(text.len()));
    let mut s = String::with_capacity(2 * radius + 1);
    if !text[..start].trim().is_empty() {
        s.push('…');
    }
    s.push_str(&collapse_ws(&text[start..end]));
    if !text[end..].trim().is_empty() {
        s.push('…');
    }
    s
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    out.trim().to_string()
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn matcher(terms: &[&str]) -> ContentMatcher {
        ContentMatcher::new(&terms.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn scan_plain_text_match_and_binary() {
        let m = matcher(&["预算"]);
        assert_eq!(
            m.scan("年度预算方案\n详细如下".as_bytes(), "txt"),
            Scan::Match {
                snippet: "年度预算方案 详细如下".into(),
                count: 1
            }
        );
        // executables carry NUL bytes and are rejected as binary
        assert_eq!(m.scan(b"MZ\x00\x01\x00\x00budget", "exe"), Scan::Binary);
    }

    #[test]
    fn scan_requires_every_term_and_counts_all_hits() {
        let m = matcher(&["预算", "Q3"]);
        assert_eq!(m.scan("只有预算".as_bytes(), "txt"), Scan::NoMatch);
        assert_eq!(
            m.scan("Q3预算\n预算 Q3".as_bytes(), "txt"),
            Scan::Match {
                snippet: "Q3预算 预算 Q3".into(),
                count: 4
            }
        );
    }

    #[test]
    fn utf16_and_bom_decoding() {
        let m = matcher(&["naïve"]);
        let utf16le: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("naïve plan".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert!(matches!(m.scan(&utf16le, "txt"), Scan::Match { .. }));
        let utf8bom: Vec<u8> = [0xEF, 0xBB, 0xBF]
            .into_iter()
            .chain("the naïve plan".bytes())
            .collect();
        assert!(matches!(m.scan(&utf8bom, "log"), Scan::Match { .. }));
    }

    #[test]
    fn snippet_elides_and_folds_whitespace() {
        let text = "lorem ipsum dolor sit amet 预算 consectetur adipiscing elit";
        let hit = text.find("预算").unwrap();
        // tiny radius keeps the hit and marks both sides as elided
        let s = snippet_around(text, hit, 8);
        assert!(s.starts_with('…') && s.ends_with('…'));
        assert!(s.contains("预算"));
        assert!(!s.contains('\n'));
        // hit at the very start: no leading ellipsis
        let s2 = snippet_around("预算 at top", 0, 8);
        assert!(!s2.starts_with('…'));
    }

    #[test]
    fn snippet_survives_multibyte_boundaries() {
        // the radius cut lands mid-character (each 中 is 3 bytes); the
        // boundary helpers must keep the snippet valid UTF-8
        let text = "中中中中中预算中中中中中";
        let hit = text.find("预算").unwrap();
        let s = snippet_around(text, hit, 4);
        assert!(s.contains("预算"));
    }

    #[test]
    fn docx_roundtrip() {
        let xml = br#"<?xml version="1.0"?><w:document><w:body><w:p><w:r><w:t>Q3 budget</w:t></w:r></w:p></w:body></w:document>"#;
        let bytes = zip_member("word/document.xml", xml);
        let m = matcher(&["budget"]);
        assert_eq!(
            m.scan(&bytes, "docx"),
            Scan::Match {
                snippet: "Q3 budget".into(),
                count: 1
            }
        );
        // same bytes under a non-container extension take the plain-text
        // path: the raw XML (tags included) is the text, so the needle in
        // the original bytes still matches — just with tag noise around it
        let Scan::Match { snippet, .. } = m.scan(xml, "xml") else {
            panic!("expected a text-path match");
        };
        assert!(snippet.contains('<'));
    }

    #[test]
    fn corrupt_container_is_an_error() {
        let m = matcher(&["预算"]);
        assert_eq!(m.scan(b"not a zip at all", "docx"), Scan::Error);
    }

    #[test]
    fn pptx_slides_are_ordered() {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts: zip::write::SimpleFileOptions = Default::default();
            w.start_file("ppt/slides/slide2.xml", opts).unwrap();
            w.write_all(b"<a:t>second</a:t>").unwrap();
            w.start_file("ppt/slides/slide10.xml", opts).unwrap();
            w.write_all(b"<a:t>tenth &amp; last</a:t>").unwrap();
            w.start_file("ppt/slides/slide1.xml", opts).unwrap();
            w.write_all(b"<a:t>first slide</a:t>").unwrap();
            w.finish().unwrap();
        }
        let m = matcher(&["tenth"]);
        assert_eq!(
            m.scan(&buf, "pptx"),
            Scan::Match {
                // slide1 → slide2 → slide10 in *numeric* order (dictionary
                // order would put slide10 first); run glue leaves no gaps
                snippet: "first slidesecondtenth & last".into(),
                count: 1
            }
        );
    }

    /// Build a single-member zip (the minimal shape of an OOXML document).
    fn zip_member(name: &str, xml: &[u8]) -> Vec<u8> {
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
}
