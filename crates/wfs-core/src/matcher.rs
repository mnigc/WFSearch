//! Query parsing and case-insensitive name matching.
//!
//! Names are stored as original UTF-8; matching folds case on the fly. ASCII
//! patterns take a SIMD (`memchr`) fast path. UTF-8 never embeds ASCII bytes
//! inside multi-byte sequences, so byte-level scanning is safe and only the
//! first-byte hint is used to seed position candidates before a char-wise
//! verify.

use memchr::memchr2_iter;
use serde::{Deserialize, Serialize};

/// Sort order for search results.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortKind {
    /// Index order (fastest, deterministic).
    #[default]
    None,
    /// Byte-wise name order.
    Name,
    /// Byte-wise full-path order (slower: paths are built on demand).
    Path,
}

/// Case-fold a single char. ASCII is branchless; non-ASCII uses the first
/// char of the Unicode lowercase mapping (multi-char expansions keep the
/// original char so both query and data fold identically).
pub fn fold_char(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    let mut it = c.to_lowercase();
    let first = it.next().unwrap_or(c);
    if it.next().is_none() {
        first
    } else {
        c
    }
}

#[inline]
fn utf8_seq_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >= 0xF0 {
        4
    } else if b >= 0xE0 {
        3
    } else if b >= 0xC0 {
        2
    } else {
        1 // continuation byte (shouldn't start a char); skip 1 to keep moving
    }
}

// ---------------------------------------------------------------- substring

/// Case-folded substring term.
#[derive(Debug, Clone)]
pub struct Substr {
    pat: Vec<char>,
    /// non-empty iff every pattern char is ASCII — enables the byte fast path
    ascii: Vec<u8>,
    /// first UTF-8 byte of the folded first char (position candidates seed)
    hint: u8,
}

impl Substr {
    pub fn new(s: &str) -> Substr {
        let pat: Vec<char> = s.chars().map(fold_char).collect();
        let ascii: Vec<u8> = if pat.iter().all(|c| c.is_ascii()) {
            pat.iter().map(|c| *c as u8).collect()
        } else {
            Vec::new()
        };
        let hint = pat
            .first()
            .map(|c| {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf);
                buf[0]
            })
            .unwrap_or(0);
        Substr { pat, ascii, hint }
    }

    pub fn matches(&self, name: &[u8]) -> bool {
        self.find(name).is_some()
    }

    /// Byte offset of the first match, or `None`. `name` must be valid UTF-8
    /// (NamePool bytes and extracted document text both are); the returned
    /// offset is always a char boundary.
    pub fn find(&self, name: &[u8]) -> Option<usize> {
        if self.pat.is_empty() {
            return Some(0);
        }
        if !self.ascii.is_empty() {
            self.find_ascii(name)
        } else {
            self.find_hinted(name)
        }
    }

    /// All-ASCII pattern: seed candidate positions with both cases of the
    /// first byte (memchr2), then compare the window case-insensitively.
    fn find_ascii(&self, name: &[u8]) -> Option<usize> {
        let pat = &self.ascii[..];
        let plen = pat.len();
        let first = pat[0];
        for s in memchr2_iter(first, first.to_ascii_uppercase(), name) {
            if s + plen > name.len() {
                return None;
            }
            if name[s..s + plen]
                .iter()
                .zip(pat)
                .all(|(a, b)| a.to_ascii_lowercase() == *b)
            {
                return Some(s);
            }
        }
        None
    }

    /// Non-ASCII-led pattern: seed with the first UTF-8 byte (always a char
    /// boundary — UTF-8 continuation bytes are < 0xC0), then folded char
    /// verify. ASCII lead bytes seed with both cases.
    fn find_hinted(&self, name: &[u8]) -> Option<usize> {
        let first = self.hint;
        let alt = first.to_ascii_uppercase();
        memchr2_iter(first, alt, name).find(|&s| self.match_at(name, s))
    }

    fn match_at(&self, name: &[u8], start: usize) -> bool {
        // SAFETY: NamePool only ever stores bytes produced from a valid
        // `String`, and `start` is a char boundary (see above).
        let hay = unsafe { std::str::from_utf8_unchecked(&name[start..]) };
        let mut hi = 0usize;
        for &pc in &self.pat {
            match hay[hi..].chars().next() {
                Some(hc) if fold_char(hc) == pc => hi += hc.len_utf8(),
                _ => return false,
            }
        }
        true
    }
}

// ----------------------------------------------------------------- wildcard

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Atom {
    Star,
    AnyOne,
    Lit(char),
}

/// Compiled `*` / `?` wildcard pattern, matched case-insensitively.
/// All-ASCII patterns run a byte-level greedy matcher (no allocation).
#[derive(Debug, Clone)]
pub struct Wildcard {
    atoms: Vec<Atom>,
    ascii: bool,
}

impl Wildcard {
    pub fn new(pat: &str) -> Wildcard {
        let mut atoms = Vec::with_capacity(pat.len());
        let mut ascii = true;
        for c in pat.chars() {
            match c {
                '*' => atoms.push(Atom::Star),
                '?' => atoms.push(Atom::AnyOne),
                _ => {
                    let f = fold_char(c);
                    if !f.is_ascii() {
                        ascii = false;
                    }
                    atoms.push(Atom::Lit(f));
                }
            }
        }
        Wildcard { atoms, ascii }
    }

    pub fn matches(&self, name: &[u8]) -> bool {
        if self.ascii {
            self.match_bytes(name)
        } else {
            self.match_chars(name)
        }
    }

    /// Classic greedy two-pointer matcher with single-star backtracking.
    /// `?` consumes exactly one UTF-8 char (computed from the lead byte).
    fn match_bytes(&self, t: &[u8]) -> bool {
        let mut ti = 0usize;
        let mut pi = 0usize;
        let mut star_p = usize::MAX;
        let mut star_t = 0usize;
        while ti < t.len() {
            match self.atoms.get(pi) {
                Some(Atom::Lit(c)) if t[ti].to_ascii_lowercase() == *c as u8 => {
                    ti += 1;
                    pi += 1;
                }
                Some(Atom::AnyOne) => {
                    ti += utf8_seq_len(t[ti]);
                    pi += 1;
                }
                Some(Atom::Star) => {
                    star_p = pi;
                    star_t = ti;
                    pi += 1;
                }
                _ => {
                    if star_p == usize::MAX {
                        return false;
                    }
                    pi = star_p + 1;
                    star_t += utf8_seq_len(t[star_t]);
                    ti = star_t;
                }
            }
        }
        while matches!(self.atoms.get(pi), Some(Atom::Star)) {
            pi += 1;
        }
        pi == self.atoms.len()
    }

    /// Fallback for patterns containing non-ASCII literals: decode both sides
    /// on the fly instead of materializing the name as `Vec<char>`. This runs
    /// once per file name, so the allocation it avoids is the whole cost of a
    /// CJK wildcard query (`项目*`) over a large volume.
    fn match_chars(&self, t: &[u8]) -> bool {
        // SAFETY: invariant from NamePool.
        let hay = unsafe { std::str::from_utf8_unchecked(t) };
        let mut ti = 0usize;
        let mut pi = 0usize;
        let mut star_p = usize::MAX;
        let mut star_t = 0usize;
        while ti < hay.len() {
            let (hc, hnext) = folded_char_at(hay, ti);
            let matched = match self.atoms.get(pi) {
                Some(Atom::Lit(c)) => hc == *c,
                Some(Atom::AnyOne) => true,
                Some(Atom::Star) => {
                    star_p = pi;
                    star_t = ti;
                    pi += 1;
                    continue;
                }
                None => false,
            };
            if matched {
                ti = hnext;
                pi += 1;
            } else if star_p == usize::MAX {
                return false;
            } else {
                // let the last star swallow one more char and retry
                if star_t >= hay.len() {
                    return false;
                }
                pi = star_p + 1;
                star_t = folded_char_at(hay, star_t).1;
                ti = star_t;
            }
        }
        while matches!(self.atoms.get(pi), Some(Atom::Star)) {
            pi += 1;
        }
        pi == self.atoms.len()
    }
}

/// Case-folded char at byte offset `i` (a char boundary), plus the offset just
/// past it.
#[inline]
fn folded_char_at(s: &str, i: usize) -> (char, usize) {
    let c = s[i..].chars().next().unwrap_or('\0');
    (fold_char(c), i + c.len_utf8())
}

// -------------------------------------------------------------------- query

/// A single name-matching term.
#[derive(Debug, Clone)]
pub enum Term {
    Sub(Substr),
    Wild(Wildcard),
}

impl Term {
    pub fn matches(&self, name: &[u8]) -> bool {
        match self {
            Term::Sub(s) => s.matches(name),
            Term::Wild(w) => w.matches(name),
        }
    }
}

/// Parsed query: whitespace-separated terms AND-ed together, plus optional
/// `c:` drive filters.
///
/// A term containing a path separator (`\` or `/`) is matched against the full
/// path; every other term matches the file name only. Materializing paths is
/// far more expensive than scanning names, so path terms are always applied
/// after the name terms have already filtered the candidate down.
///
/// A `content:xxx` term (case-insensitive prefix) does not constrain file
/// names at all: it requests a document-content scan of whatever candidates
/// the name/path/drive terms let through. Content terms are foldcase
/// substrings — wildcards are not supported there.
#[derive(Debug, Clone, Default)]
pub struct Query {
    /// matched against the file name
    pub terms: Vec<Term>,
    /// matched against the full path
    pub path_terms: Vec<Term>,
    /// matched against the file content (triggers a content scan)
    pub content_terms: Vec<String>,
    pub drives: Vec<char>,
}

fn is_path_token(tok: &str) -> bool {
    tok.contains('\\') || tok.contains('/')
}

/// `content:` prefix, case-insensitive (matching Windows' general
/// case-insensitivity); the rest of the token is the content term.
fn content_term_of(tok: &str) -> Option<&str> {
    const PREFIX: &str = "content:";
    if tok.len() > PREFIX.len()
        && tok.as_bytes()[PREFIX.len() - 1] == b':'
        && tok[..PREFIX.len() - 1].eq_ignore_ascii_case("content")
    {
        Some(&tok[PREFIX.len()..])
    } else {
        None
    }
}

impl Query {
    pub fn parse(input: &str) -> Query {
        let mut q = Query::default();
        for tok in input.split_whitespace() {
            let b = tok.as_bytes();
            if b.len() == 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
                q.drives.push((b[0] as char).to_ascii_lowercase());
                continue;
            }
            if let Some(term) = content_term_of(tok) {
                q.content_terms.push(term.to_string());
                continue;
            }
            if tok.is_empty() {
                continue;
            }
            let term = if tok.contains('*') || tok.contains('?') {
                Term::Wild(Wildcard::new(tok))
            } else {
                Term::Sub(Substr::new(tok))
            };
            if is_path_token(tok) {
                q.path_terms.push(term);
            } else {
                q.terms.push(term);
            }
        }
        q
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.path_terms.is_empty() && self.drives.is_empty()
    }
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(s: &str) -> Substr {
        Substr::new(s)
    }

    fn wild(s: &str) -> Wildcard {
        Wildcard::new(s)
    }

    #[test]
    fn substring_ascii_case_insensitive() {
        let p = sub("Report");
        assert!(p.matches(b"Q3 report final.docx"));
        assert!(p.matches(b"REPORT.TXT"));
        assert!(p.matches(b"report"));
        assert!(!p.matches(b"rep.txt"));
        assert!(p.matches(b"areporte")); // contains "report"
        assert!(p.matches(b"areported"));
    }

    #[test]
    fn substring_cjk() {
        let p = sub("文件夹");
        assert!(p.matches("项目文件夹.txt".as_bytes()));
        assert!(!p.matches("项目文档.txt".as_bytes()));
        // query with ASCII prefix + CJK body
        let p2 = sub("a文");
        assert!(p2.matches("xA文b.txt".as_bytes()));
        assert!(!p2.matches("文a.txt".as_bytes()));
    }

    #[test]
    fn substring_mixed_case_fold() {
        // Turkish-style expansion folds conservatively; plain accented chars match
        let p = sub("café");
        assert!(p.matches("CAFÉ notes.txt".as_bytes()));
        assert!(p.matches("naïve café.md".as_bytes()));
    }

    #[test]
    fn wildcard_basic() {
        assert!(wild("*.docx").matches(b"Report.DOCX"));
        assert!(!wild("*.docx").matches(b"report.docx1"));
        assert!(wild("a?c").matches(b"ABC"));
        assert!(wild("a?c").matches("a中c".as_bytes())); // ? matches one CJK char
        assert!(!wild("a?c").matches(b"ac"));
        assert!(wild("*").matches(b"anything"));
        assert!(wild("*").matches(b""));
        assert!(wild("ab*").matches(b"ab"));
        assert!(wild("*ab").matches(b"ab"));
        assert!(wild("a*b*c").matches(b"a-x-b-y-c"));
        assert!(wild("a*b*c").matches(b"abc"));
        assert!(!wild("a*b*c").matches(b"a-b-x-d")); // no a…b…c subsequence
        assert!(!wild("a*b*c").matches(b"b-a-c")); // must start with 'a'
        assert!(wild("文*夹").matches("文件夹".as_bytes()));
        assert!(wild("文*夹").matches("文件/子文件夹".as_bytes()));
    }

    #[test]
    fn wildcard_suffix_backtrack() {
        // needs backtracking: "*ab" where first 'a' doesn't lead to "ab" end
        assert!(wild("*abc").matches(b"xabxabc"));
        assert!(wild("a*bc").matches(b"ababc"));
    }

    #[test]
    fn query_parse() {
        let q = Query::parse("report C: *.docx");
        assert_eq!(q.drives, vec!['c']);
        assert_eq!(q.terms.len(), 2);
        let q2 = Query::parse("　全角空格　test");
        assert_eq!(q2.terms.len(), 2); // U+3000 splits too
        assert!(Query::parse("").is_empty());
    }

    #[test]
    fn query_parse_content_terms() {
        let q = Query::parse("*.docx content:预算");
        assert_eq!(q.content_terms, vec!["预算"]);
        assert_eq!(q.terms.len(), 1); // the wildcard stays a name term
                                      // prefix is case-insensitive; like every term, a content term is a
                                      // single whitespace-separated token (no phrase syntax)
        assert_eq!(Query::parse("Content:Q3").content_terms, vec!["Q3"]);
        // multiple content terms AND together
        assert_eq!(Query::parse("content:a content:b").content_terms.len(), 2);
        // a bare `content:` carries no term and is dropped
        assert!(Query::parse("content:").content_terms.is_empty());
        // the old behavior is preserved: without the prefix this is a name term
        let old = Query::parse("content");
        assert!(old.content_terms.is_empty() && old.terms.len() == 1);
        // a content term does not make the name-level query non-empty
        assert!(Query::parse("content:x").is_empty());
    }

    #[test]
    fn substr_find_returns_hit_offset() {
        let p = sub("Report");
        assert_eq!(p.find(b"Q3 Report final"), Some(3));
        assert_eq!(p.find(b"REPORT.TXT"), Some(0));
        assert_eq!(p.find(b"rep.txt"), None);
        // offset lands on a char boundary inside CJK text
        let cjk = sub("文件夹");
        assert_eq!(cjk.find("项目文件夹.txt".as_bytes()), Some(6));
        assert_eq!(cjk.find("项目文档.txt".as_bytes()), None);
        // empty pattern matches at offset 0
        assert_eq!(sub("").find(b"anything"), Some(0));
    }

    #[test]
    fn query_routes_path_terms() {
        let q = Query::parse(r"report sub\a.txt C:\work c:");
        assert_eq!(q.terms.len(), 1);
        assert!(q.terms[0].matches(b"report.docx"));
        assert_eq!(q.path_terms.len(), 2); // `sub\a.txt`, `C:\work`
        assert_eq!(q.drives, vec!['c']);
        assert!(Query::parse(r"D:\media").path_terms.len() == 1);
        assert!(Query::parse("a/b").path_terms.len() == 1); // forward slash too
    }

    #[test]
    fn wildcard_cjk_char_path() {
        // non-ASCII patterns take the char-wise matcher: `?` consumes exactly
        // one char and `*` must still backtrack
        assert!(wild("文*件").matches("文x件".as_bytes()));
        assert!(wild("文*件").matches("文档文件".as_bytes()));
        assert!(!wild("文*件").matches("文".as_bytes()));
        assert!(!wild("文*件").matches("文abc".as_bytes()));
        // 文件夹 is 文/件/夹 — it does not *end* with 件, so this must fail
        assert!(!wild("文*件").matches("文件夹".as_bytes()));
        assert!(wild("文*夹").matches("文件夹".as_bytes()));
        assert!(wild("文?件").matches("文中件".as_bytes()));
        assert!(!wild("文?件").matches("文件".as_bytes()));
        assert!(!wild("文?件").matches("文中文件".as_bytes())); // `?` is one char
        assert!(wild("*件").matches("文件".as_bytes()));
        assert!(!wild("*件").matches("文件夹".as_bytes()));
        assert!(wild("中*文").matches("中abc文".as_bytes()));
        assert!(!wild("中*文").matches("中xyz".as_bytes()));
    }

    #[test]
    fn utf8_seq_len_table() {
        assert_eq!(utf8_seq_len(b'a'), 1);
        assert_eq!(utf8_seq_len(0xE4), 3); // 中 lead byte
        assert_eq!(utf8_seq_len(0x80), 1);
    }
}
