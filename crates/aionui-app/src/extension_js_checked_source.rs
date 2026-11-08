//! Conditional integrity against a host-supplied expected hash, not authority.
//!
//! The host must separately establish a trusted root, trusted /proc view and
//! correct trusted digest-to-handler binding. This API does not acquire those
//! facts from a manifest, registry label or the source's self-reported hash.
use crate::extension_js_source::SourceError;
use std::{fs::File, path::Path};

/// Fixed categories only; never include source, paths or expected-hash values.
#[derive(Debug, PartialEq, Eq)]
pub enum CheckedSourceError {
    Unavailable,
    InvalidIntegrity,
    IntegrityMismatch,
    Reader(SourceError),
}

/// The exact owned UTF-8 snapshot that matched the supplied expected SRI.
///
/// No unchecked constructor, Default, Deserialize or raw-source Debug formatter.
/// This is neither an execution permit nor signed-manifest/curation/owner proof.
pub struct IntegrityCheckedSource {
    source: String,
}

impl IntegrityCheckedSource {
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// Transfer the checked snapshot to a future consumer without rereading a path.
    /// Existing client invoke still requires separate host authorization.
    pub fn into_source(self) -> String {
        self.source
    }
}

/// Read once with the existing bounded reader and compare that exact snapshot.
///
/// The expected hash is a trusted HOST input precondition, not proof of its own
/// provenance. Optional `handlerIntegrity` may declare an expected SRI inside a
/// signed snapshot; Host composition (`load_checked_handler_source_from_signed`)
/// wires that binding to this API. Matching this parameter alone still does not
/// authorize execution or establish curation, owner or immutable files.
///
/// Disabled/non-Linux builds immediately return Unavailable regardless of inputs.
/// Enabled Linux validates canonical 95-byte SHA-512 SRI before file IO, then
/// calls read_bounded_source exactly once. Its trusted-root/proc assumptions and
/// 65536-byte UTF-8 limit remain unchanged; this is NOT an I/O wall-clock bound.
/// Nothing is automatically invoked, and there is no dispatcher/HTTP wiring.
pub fn read_checked_source(
    root: &File,
    handler: &Path,
    expected_sri: &str,
) -> Result<IntegrityCheckedSource, CheckedSourceError> {
    #[cfg(all(feature = "experimental-extension-js", target_os = "linux"))]
    {
        validate_integrity(expected_sri)?;
        let source =
            crate::extension_js_source::read_bounded_source(root, handler).map_err(CheckedSourceError::Reader)?;
        if aionui_extension::hub::sri_sha512(source.as_bytes()) != expected_sri {
            return Err(CheckedSourceError::IntegrityMismatch);
        }
        Ok(IntegrityCheckedSource { source })
    }
    #[cfg(not(all(feature = "experimental-extension-js", target_os = "linux")))]
    {
        let _ = (root, handler, expected_sri);
        Err(CheckedSourceError::Unavailable)
    }
}

#[cfg(all(feature = "experimental-extension-js", target_os = "linux"))]
fn validate_integrity(expected_sri: &str) -> Result<(), CheckedSourceError> {
    // Shared with extension handlerIntegrity validation; identical canonical rules.
    if aionui_extension::hub::is_canonical_sha512_sri(expected_sri) {
        Ok(())
    } else {
        Err(CheckedSourceError::InvalidIntegrity)
    }
}

#[cfg(all(test, feature = "experimental-extension-js", target_os = "linux"))]
mod enabled_tests {
    use super::*;
    use aionui_extension::hub::sri_sha512;
    use std::{
        ffi::CString,
        fs,
        os::unix::{ffi::OsStrExt, fs::symlink},
    };
    use tempfile::TempDir;

    fn fixture() -> (TempDir, File) {
        let temp = tempfile::tempdir().unwrap();
        let root = File::open(temp.path()).unwrap();
        (temp, root)
    }

    fn error(root: &File, path: &str, sri: &str) -> CheckedSourceError {
        read_checked_source(root, Path::new(path), sri)
            .err()
            .expect("must reject")
    }

    fn fifo(path: &Path) {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: valid NUL-terminated fixture-owned path and mode. We create
        // but never read-open this FIFO; the inherited reader rejects O_PATH REG check.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }

    #[test]
    fn utf8_crlf_bom_snapshot_and_consumption_preserve_exact_bytes() {
        let (temp, root) = fixture();
        let raw = "\u{feff}// 你好\r\nexport const x = 'é';\r\n";
        fs::write(temp.path().join("source.js"), raw.as_bytes()).unwrap();
        let checked = read_checked_source(&root, Path::new("source.js"), &sri_sha512(raw.as_bytes())).unwrap();
        assert_eq!(checked.as_str().as_bytes(), raw.as_bytes());
        assert_eq!(checked.into_source().into_bytes(), raw.as_bytes());
    }

    #[test]
    fn format_is_validated_before_reader_for_missing_fifo_and_wrong_root() {
        let (temp, root) = fixture();
        fifo(&temp.path().join("fifo"));
        let wrong_root = tempfile::tempfile().unwrap();
        let canonical = sri_sha512(b"");
        let mut noncanonical_bits = canonical.as_bytes().to_vec();
        noncanonical_bits[92] = b'B'; // lower padding bits must be zero
        let mut invalid_char = canonical.as_bytes().to_vec();
        invalid_char[7] = b'_'; // URL-safe alphabet is not standard base64
        let mut whitespace = canonical.as_bytes().to_vec();
        whitespace[7] = b' ';
        let invalid = [
            String::new(),
            "x".repeat(1024 * 1024),
            canonical.replacen("sha512-", "sha256-", 1),
            canonical.replacen("sha512-", "SHA512-", 1),
            canonical.trim_end_matches('=').to_owned(),
            format!("{canonical} "),
            format!(" {canonical}"),
            canonical.replace("==", "=A"),
            String::from_utf8(noncanonical_bits).unwrap(),
            String::from_utf8(invalid_char).unwrap(),
            String::from_utf8(whitespace).unwrap(),
            "sha512-".to_owned() + &"é".repeat(44), // 95 bytes, non-ASCII
        ];
        for sri in invalid {
            for path in ["missing", "fifo"] {
                assert_eq!(error(&root, path, &sri), CheckedSourceError::InvalidIntegrity);
            }
            assert_eq!(
                error(&wrong_root, "missing", &sri),
                CheckedSourceError::InvalidIntegrity
            );
        }
    }

    #[test]
    fn tampered_content_and_case_changed_digest_do_not_match() {
        let (temp, root) = fixture();
        let original = b"export const x = 1;";
        fs::write(temp.path().join("source"), original).unwrap();
        let sri = sri_sha512(original);
        let mut changed = sri.as_bytes().to_vec();
        // Avoid the final padding sextet so the case mutation stays canonical
        // base64 while encoding a DIFFERENT digest.
        let index = changed[7..92].iter().position(u8::is_ascii_alphabetic).unwrap() + 7;
        changed[index] ^= 0x20;
        let changed = String::from_utf8(changed).unwrap();
        assert_eq!(validate_integrity(&changed), Ok(()));
        assert_eq!(error(&root, "source", &changed), CheckedSourceError::IntegrityMismatch);
        fs::write(temp.path().join("source"), b"export const x = 2;").unwrap();
        assert_eq!(error(&root, "source", &sri), CheckedSourceError::IntegrityMismatch);
    }

    #[test]
    fn empty_exact_limit_overflow_and_invalid_utf8() {
        let (temp, root) = fixture();
        for raw in [Vec::new(), vec![b'a'; 65536]] {
            fs::write(temp.path().join("source"), &raw).unwrap();
            let checked = read_checked_source(&root, Path::new("source"), &sri_sha512(&raw)).unwrap();
            assert_eq!(checked.into_source().as_bytes(), raw);
        }
        for (raw, expected) in [
            (vec![b'a'; 65537], SourceError::TooLarge),
            (vec![0xff], SourceError::InvalidUtf8),
        ] {
            fs::write(temp.path().join("source"), &raw).unwrap();
            assert_eq!(
                error(&root, "source", &sri_sha512(&raw)),
                CheckedSourceError::Reader(expected)
            );
        }
    }

    #[test]
    fn replacement_and_in_place_write_after_check_cannot_change_owned_snapshots() {
        let (temp, root) = fixture();
        fs::write(temp.path().join("source"), "old").unwrap();
        let old = read_checked_source(&root, Path::new("source"), &sri_sha512(b"old")).unwrap();
        fs::write(temp.path().join("replacement"), "replacement").unwrap();
        fs::rename(temp.path().join("replacement"), temp.path().join("source")).unwrap();
        let replacement = read_checked_source(&root, Path::new("source"), &sri_sha512(b"replacement")).unwrap();
        fs::write(temp.path().join("source"), "in-place mutation").unwrap();
        // Both snapshots are consumed AFTER both modifications. There is no
        // reread. This does not assert the file was immutable while being read.
        assert_eq!(old.into_source(), "old");
        assert_eq!(replacement.into_source(), "replacement");
    }

    #[test]
    fn preserves_fixed_reader_errors_for_invalid_paths_roots_and_missing_files() {
        let (temp, root) = fixture();
        let sri = sri_sha512(b"");
        for path in ["", "../parent", "/absolute", "https://example/source", "nul\0path"] {
            assert_eq!(
                error(&root, path, &sri),
                CheckedSourceError::Reader(SourceError::InvalidPath)
            );
        }
        let wrong_root = tempfile::tempfile().unwrap();
        assert_eq!(
            error(&wrong_root, "source", &sri),
            CheckedSourceError::Reader(SourceError::WrongRootKind)
        );
        assert_eq!(
            error(&root, "missing", &sri),
            CheckedSourceError::Reader(SourceError::Resolve)
        );
        fs::create_dir(temp.path().join("directory")).unwrap();
        assert_eq!(
            error(&root, "directory", &sri),
            CheckedSourceError::Reader(SourceError::NotRegular)
        );
    }

    #[test]
    fn symlink_and_fifo_refusal_is_not_weakened() {
        let (temp, root) = fixture();
        let outside = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("file"), b"inside").unwrap();
        fs::write(outside.path().join("file"), b"outside").unwrap();
        symlink("file", temp.path().join("inside-link")).unwrap();
        symlink(outside.path().join("file"), temp.path().join("outside-link")).unwrap();
        fifo(&temp.path().join("fifo"));
        for path in ["inside-link", "outside-link"] {
            assert_eq!(
                error(&root, path, &sri_sha512(b"inside")),
                CheckedSourceError::Reader(SourceError::Resolve)
            );
        }
        assert_eq!(
            error(&root, "fifo", &sri_sha512(b"")),
            CheckedSourceError::Reader(SourceError::NotRegular)
        );
    }

    #[test]
    fn canonical_format_accepts_helper_outputs_and_checks_every_padding_sextet() {
        for byte in 0..=255 {
            assert_eq!(validate_integrity(&sri_sha512(&[byte])), Ok(()));
        }
        let sri = sri_sha512(b"source");
        let mut candidate = sri.into_bytes();
        for last in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/" {
            candidate[92] = *last;
            let result = validate_integrity(std::str::from_utf8(&candidate).unwrap());
            assert_eq!(result.is_ok(), matches!(*last, b'A' | b'Q' | b'g' | b'w'));
        }
    }
}

#[cfg(all(test, not(all(feature = "experimental-extension-js", target_os = "linux"))))]
mod disabled_tests {
    use super::*;

    #[test]
    fn immediately_unavailable_even_with_invalid_metadata_and_wrong_root() {
        let root = tempfile::tempfile().unwrap();
        for path in ["", "../parent", "nul\0path"] {
            for sri in ["", "not a digest"] {
                assert_eq!(
                    read_checked_source(&root, Path::new(path), sri).err(),
                    Some(CheckedSourceError::Unavailable)
                );
            }
        }
    }
}
