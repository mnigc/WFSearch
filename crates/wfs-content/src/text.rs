//! Plain-text decoding: BOM sniffing, UTF-16 conversion, binary rejection.

use memchr::memchr;

/// Head window scanned for NUL bytes when there is no BOM.
const SNIFF_LEN: usize = 8192;

/// Decode `bytes` to (lossy) UTF-8 text, or `None` when the content looks
/// binary. A NUL byte in the BOM-less head is the binary signal — UTF-16
/// without a BOM also lands there, which we accept: preferring "binary" over
/// decoding garbage keeps false matches near zero.
pub fn decode(bytes: &[u8]) -> Option<String> {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Some(String::from_utf8_lossy(&bytes[3..]).into_owned());
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return Some(from_utf16(&bytes[2..], true));
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return Some(from_utf16(&bytes[2..], false));
    }
    let head = &bytes[..bytes.len().min(SNIFF_LEN)];
    if memchr(0, head).is_some() {
        return None;
    }
    Some(String::from_utf8_lossy(bytes).into_owned())
}

fn from_utf16(bytes: &[u8], little: bool) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| {
            if little {
                u16::from_le_bytes(*c)
            } else {
                u16::from_be_bytes(*c)
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_binary_by_nul() {
        assert!(decode(b"plain text").is_some());
        assert!(decode(b"utf8\x00with nul").is_none());
        assert!(decode(b"").is_some());
    }

    #[test]
    fn bom_selects_encoding() {
        let utf8: Vec<u8> = [0xEF, 0xBB, 0xBF]
            .into_iter()
            .chain("héllo".bytes())
            .collect();
        assert_eq!(decode(&utf8).unwrap(), "héllo");
        let utf16le: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("预算".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(decode(&utf16le).unwrap(), "预算");
        let utf16be: Vec<u8> = [0xFE, 0xFF]
            .into_iter()
            .chain("预算".encode_utf16().flat_map(u16::to_be_bytes))
            .collect();
        assert_eq!(decode(&utf16be).unwrap(), "预算");
    }

    #[test]
    fn invalid_utf8_is_lossy_not_rejected() {
        // a lone 0xE4 lead byte is invalid UTF-8 but has no NUL — keep the
        // text (lossy) rather than calling it binary
        let decoded = decode(b"caf\xe4 menu").unwrap();
        assert!(decoded.contains("caf"));
    }
}
