//! Decode on-disk bytes as previewable / tool-readable text.
//!
//! `std::fs::read_to_string` only accepts UTF-8. Windows PowerShell 5.1
//! `Out-File` / `Set-Content` default to UTF-16 LE with BOM, so a `.json` file
//! written by `generate.ps1` is text and still fails that API. UTF-16 without a
//! BOM of ASCII JSON is *valid* UTF-8 (NUL between every character) and then
//! crashes CodeMirror, which cannot load U+0000.
//!
//! Decode order:
//! 1. UTF-8 / UTF-16 BOM (authoritative)
//! 2. UTF-16 LE heuristic (even length, high-byte NULs — typical ASCII as UTF-16)
//! 3. Strict UTF-8
//!
//! NULs are stripped so a leftover UTF-16-as-UTF-8 payload cannot reach an
//! editor. A UTF-8 BOM is stripped so JSON highlighters do not treat the file
//! as invalid.

use crate::error::FileError;

const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
const UTF16_LE_BOM: &[u8] = &[0xFF, 0xFE];
const UTF16_BE_BOM: &[u8] = &[0xFE, 0xFF];

/// Decode file bytes as UTF-8 or UTF-16 text.
pub(crate) fn decode_text_bytes(bytes: &[u8]) -> Result<String, FileError> {
    if bytes.starts_with(UTF8_BOM) {
        return utf8_to_editor_string(&bytes[UTF8_BOM.len()..]);
    }
    if bytes.starts_with(UTF16_LE_BOM) {
        return Ok(utf16_to_editor_string(&bytes[UTF16_LE_BOM.len()..], true));
    }
    if bytes.starts_with(UTF16_BE_BOM) {
        return Ok(utf16_to_editor_string(&bytes[UTF16_BE_BOM.len()..], false));
    }
    if looks_like_utf16_le(bytes) {
        return Ok(utf16_to_editor_string(bytes, true));
    }
    utf8_to_editor_string(bytes)
}

fn utf8_to_editor_string(bytes: &[u8]) -> Result<String, FileError> {
    let text = std::str::from_utf8(bytes).map_err(|_| FileError::InvalidTextEncoding)?;
    Ok(strip_nuls(text))
}

fn utf16_to_editor_string(bytes: &[u8], little_endian: bool) -> String {
    let even_end = bytes.len() - (bytes.len() % 2);
    let mut units = Vec::with_capacity(even_end / 2);
    let mut i = 0;
    while i < even_end {
        let unit = if little_endian {
            u16::from_le_bytes([bytes[i], bytes[i + 1]])
        } else {
            u16::from_be_bytes([bytes[i], bytes[i + 1]])
        };
        units.push(unit);
        i += 2;
    }
    strip_nuls(&String::from_utf16_lossy(&units))
}

/// ASCII / Latin text stored as UTF-16 LE has 0x00 in nearly every high byte.
fn looks_like_utf16_le(bytes: &[u8]) -> bool {
    if bytes.len() < 2 || !bytes.len().is_multiple_of(2) {
        return false;
    }
    let pairs = bytes.len() / 2;
    let odd_nuls = bytes.iter().skip(1).step_by(2).filter(|b| **b == 0).count();
    odd_nuls * 2 >= pairs
}

fn strip_nuls(text: &str) -> String {
    if !text.as_bytes().contains(&0) {
        return text.to_owned();
    }
    text.chars().filter(|c| *c != '\0').collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16_le(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn utf16_be(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_be_bytes).collect()
    }

    #[test]
    fn utf8_json_round_trips() {
        let json = r#"{"ok":true,"n":50}"#;
        assert_eq!(decode_text_bytes(json.as_bytes()).unwrap(), json);
    }

    #[test]
    fn utf8_bom_is_stripped() {
        let mut bytes = UTF8_BOM.to_vec();
        bytes.extend_from_slice(br#"{"ok":true}"#);
        assert_eq!(decode_text_bytes(&bytes).unwrap(), r#"{"ok":true}"#);
    }

    #[test]
    fn utf16_le_bom_powershell_json() {
        let mut bytes = UTF16_LE_BOM.to_vec();
        bytes.extend(utf16_le(r#"{"count":50}"#));
        assert_eq!(decode_text_bytes(&bytes).unwrap(), r#"{"count":50}"#);
    }

    #[test]
    fn utf16_be_bom_json() {
        let mut bytes = UTF16_BE_BOM.to_vec();
        bytes.extend(utf16_be(r#"{"count":50}"#));
        assert_eq!(decode_text_bytes(&bytes).unwrap(), r#"{"count":50}"#);
    }

    #[test]
    fn utf16_le_without_bom_ascii_json() {
        let bytes = utf16_le(r#"{"ok":true}"#);
        assert_eq!(decode_text_bytes(&bytes).unwrap(), r#"{"ok":true}"#);
    }

    #[test]
    fn utf16_le_bom_non_ascii() {
        let mut bytes = UTF16_LE_BOM.to_vec();
        bytes.extend(utf16_le("你好"));
        assert_eq!(decode_text_bytes(&bytes).unwrap(), "你好");
    }

    #[test]
    fn empty_file_is_empty_string() {
        assert_eq!(decode_text_bytes(b"").unwrap(), "");
    }

    #[test]
    fn nuls_are_stripped_from_utf8() {
        assert_eq!(decode_text_bytes(b"a\0b\0c").unwrap(), "abc");
    }

    #[test]
    fn binary_without_utf16_shape_is_rejected() {
        let err = decode_text_bytes(&[0x80, 0x81, 0x82]).unwrap_err();
        assert!(matches!(err, FileError::InvalidTextEncoding));
    }

    #[test]
    fn looks_like_utf16_le_requires_even_length() {
        assert!(!looks_like_utf16_le(&[0x7B, 0x00, 0x22]));
        assert!(looks_like_utf16_le(&[0x7B, 0x00, 0x22, 0x00]));
    }
}
