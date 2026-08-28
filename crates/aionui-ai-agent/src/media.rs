//! Capability-aware partition of message attachments into native media
//! blocks vs path-text files.
//!
//! Applied at agent dispatch time — the only layer that knows the target
//! agent's prompt capabilities. The persisted/broadcast message keeps the
//! full `[[AION_FILES]]` form (UI chips and history are untouched); only
//! the agent-bound copy is rewritten. Mirrors the aionrs precedent in
//! `manager/aionrs/content.rs`: strip the trailing marker block when it
//! matches `files` exactly, then rebuild.

use std::{path::Path, time::Duration};

use aionui_api_types::{PromptAttachmentDelivery, PromptAttachmentV1};
use aionui_common::constants::AIONUI_FILES_MARKER;
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::types::PromptMediaCaps;

/// Max bytes for a single attachment sent as an inline base64 content block.
/// Above this the attachment degrades to a path (Claude's hard per-image API
/// limit, and a sane ceiling for one wire frame).
pub const MAX_MEDIA_BLOCK_BYTES: u64 = 5 * 1024 * 1024;
pub const MAX_IMAGE_PIXELS: u64 = 40_000_000;
const MEDIA_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Coarse media classification for prompt content blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Audio,
}

/// An attachment that should be delivered as a native content block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaAttachment {
    pub attachment_index: usize,
    /// Absolute path on the local filesystem.
    pub path: String,
    /// Full mime type (e.g. `image/png`, `audio/mpeg`).
    pub mime: String,
    pub kind: MediaKind,
    pub size: u64,
    pub sha256: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Result of [`partition_media`].
#[derive(Debug)]
pub struct MediaPartition {
    /// Agent-bound content: the user text with the `[[AION_FILES]]` block
    /// re-appended containing only the non-media paths. Byte-identical to the
    /// input when nothing partitions to media.
    pub content: String,
    /// Attachments that stay as path text / resource links, in order.
    pub path_files: Vec<String>,
    /// Attachments to send as native blocks, in order.
    pub media: Vec<MediaAttachment>,
    /// Final admission decision for each prepared descriptor, in input order.
    pub deliveries: Vec<PromptAttachmentV1>,
}

/// Split `files` into native-block media vs path attachments, honoring the
/// agent's declared capabilities, and rewrite `content`'s trailing marker
/// block to list only the path attachments.
///
/// Degradation rules (attachment stays a path): capability not declared,
/// non-media mime, SVG (vision APIs reject it), file missing/unreadable, or
/// larger than [`MAX_MEDIA_BLOCK_BYTES`]. With `caps == default()` the input
/// passes through byte-identical.
pub async fn partition_media(
    content: &str,
    files: &[String],
    prepared: &[PromptAttachmentV1],
    caps: PromptMediaCaps,
) -> MediaPartition {
    let mut deliveries = if prepared.len() == files.len() {
        prepared.to_vec()
    } else {
        Vec::new()
    };
    if files.is_empty() || caps == PromptMediaCaps::default() {
        for delivery in &mut deliveries {
            if delivery.delivery == PromptAttachmentDelivery::Pending {
                delivery.delivery = PromptAttachmentDelivery::PathFallback;
                delivery.reason = Some("capability_unsupported".to_owned());
            }
        }
        return MediaPartition {
            content: content.to_owned(),
            path_files: files.to_vec(),
            media: Vec::new(),
            deliveries,
        };
    }

    let mut path_files = Vec::new();
    let mut media = Vec::new();
    for (index, path) in files.iter().enumerate() {
        match classify(path, caps).await {
            Some(mut attachment) => {
                attachment.attachment_index = index;
                if let Some(delivery) = deliveries.get_mut(index)
                    && delivery.delivery == PromptAttachmentDelivery::Pending
                {
                    delivery.delivery = PromptAttachmentDelivery::Native;
                    delivery.reason = None;
                }
                media.push(attachment);
            }
            None => {
                if let Some(delivery) = deliveries.get_mut(index)
                    && delivery.delivery == PromptAttachmentDelivery::Pending
                {
                    delivery.delivery = PromptAttachmentDelivery::PathFallback;
                    delivery.reason = Some("native_admission_failed".to_owned());
                }
                path_files.push(path.clone());
            }
        }
    }

    let content = if media.is_empty() {
        content.to_owned()
    } else {
        append_files_marker(strip_files_marker(content, files), &path_files)
    };
    MediaPartition {
        content,
        path_files,
        media,
        deliveries,
    }
}

/// Classify one attachment; `Some` means "send as a native block".
async fn classify(path: &str, caps: PromptMediaCaps) -> Option<MediaAttachment> {
    let mime = mime_guess::from_path(path).first()?;
    let kind = match mime.type_().as_str() {
        // SVG is source text, not a raster image — vision APIs reject it.
        "image" if caps.image && mime.subtype() != "svg" => MediaKind::Image,
        "audio" if caps.audio => MediaKind::Audio,
        _ => return None,
    };
    let metadata = tokio::time::timeout(MEDIA_READ_TIMEOUT, tokio::fs::metadata(path)).await;
    match metadata {
        Err(_) => {
            warn!("media attachment metadata read timed out; sending as path");
            None
        }
        Ok(result) => match result {
            Ok(meta) if meta.is_file() && meta.len() <= MAX_MEDIA_BLOCK_BYTES => {
                let bytes = match tokio::time::timeout(MEDIA_READ_TIMEOUT, tokio::fs::read(path)).await {
                    Ok(Ok(bytes)) => bytes,
                    Ok(Err(error)) => {
                        warn!(error = %error, "media attachment admission read failed; sending as path");
                        return None;
                    }
                    Err(_) => {
                        warn!("media attachment admission read timed out; sending as path");
                        return None;
                    }
                };
                let (resolved_mime, width, height) = if kind == MediaKind::Image {
                    let Some((detected_mime, width, height)) = inspect_image(&bytes) else {
                        warn!("media attachment content is not a supported raster image; sending as path");
                        return None;
                    };
                    if detected_mime != mime.essence_str() {
                        warn!(
                            expected_mime = mime.essence_str(),
                            detected_mime, "media attachment extension does not match content; sending as path"
                        );
                        return None;
                    }
                    if u64::from(width).saturating_mul(u64::from(height)) > MAX_IMAGE_PIXELS {
                        warn!(
                            width,
                            height, "media attachment exceeds image pixel limit; sending as path"
                        );
                        return None;
                    }
                    (detected_mime.to_owned(), Some(width), Some(height))
                } else {
                    let Some(detected_mime) = inspect_audio(&bytes) else {
                        warn!("media attachment content is not supported audio; sending as path");
                        return None;
                    };
                    if !mime_matches(mime.essence_str(), detected_mime) {
                        warn!(
                            expected_mime = mime.essence_str(),
                            detected_mime, "media attachment extension does not match content; sending as path"
                        );
                        return None;
                    }
                    (detected_mime.to_owned(), None, None)
                };
                Some(MediaAttachment {
                    attachment_index: 0,
                    path: path.to_owned(),
                    mime: resolved_mime,
                    kind,
                    size: bytes.len() as u64,
                    sha256: hex::encode(Sha256::digest(&bytes)),
                    width,
                    height,
                })
            }
            Ok(meta) if meta.is_file() => {
                warn!(
                    bytes = meta.len(),
                    "media attachment exceeds block size limit; sending as path"
                );
                None
            }
            _ => {
                warn!("media attachment unreadable; sending as path");
                None
            }
        },
    }
}

fn inspect_image(bytes: &[u8]) -> Option<(&'static str, u32, u32)> {
    let format = image::guess_format(bytes).ok()?;
    let mime = match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        _ => return None,
    };
    let reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let (width, height) = reader.into_dimensions().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    if u64::from(width).saturating_mul(u64::from(height)) > MAX_IMAGE_PIXELS {
        return Some((mime, width, height));
    }
    image::load_from_memory_with_format(bytes, format).ok()?;
    Some((mime, width, height))
}

fn inspect_audio(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"ID3") || bytes.starts_with(b"\xff\xfb") || bytes.starts_with(b"\xff\xf3") {
        Some("audio/mpeg")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WAVE" {
        Some("audio/wav")
    } else if bytes.starts_with(b"OggS") {
        Some("audio/ogg")
    } else if bytes.starts_with(b"fLaC") {
        Some("audio/flac")
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        Some("audio/mp4")
    } else if bytes.starts_with(b"\xff\xf1") || bytes.starts_with(b"\xff\xf9") {
        Some("audio/aac")
    } else {
        None
    }
}

fn mime_matches(expected: &str, detected: &str) -> bool {
    expected == detected
        || matches!(
            (expected, detected),
            ("audio/x-wav", "audio/wav") | ("audio/x-m4a", "audio/mp4")
        )
}

/// Strip the trailing `[[AION_FILES]]` block iff its path list matches
/// `files` exactly (same validation as aionrs `strip_attachment_metadata`);
/// otherwise return `content` unchanged.
fn strip_files_marker<'a>(content: &'a str, files: &[String]) -> &'a str {
    let Some((user_text, metadata)) = content.rsplit_once(AIONUI_FILES_MARKER) else {
        return content;
    };
    let metadata_files = metadata.lines().map(str::trim).filter(|line| !line.is_empty());
    if metadata_files.eq(files.iter().map(String::as_str)) {
        user_text.strip_suffix("\n\n").unwrap_or(user_text)
    } else {
        content
    }
}

/// Re-append the marker block in the exact `resolve_chat_message` format.
fn append_files_marker(content: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        content.to_owned()
    } else {
        format!("{content}\n\n{AIONUI_FILES_MARKER}\n{}", paths.join("\n"))
    }
}

/// The agent-bound text with the `[[AION_FILES]]` block listing EVERY
/// attachment — media included — regardless of what [`partition_media`] moved
/// to a native block.
///
/// For callers whose only path-delivery channel is the text itself (the ACP
/// `session/prompt` path emits no resource links: a non-media attachment rides
/// solely as a marker line). A native media block carries bytes, not a path, so
/// dropping the media path from the marker leaves such an agent able to see the
/// image but unable to open the file.
///
/// Not the same as returning `content` untouched: when `content` carries no
/// marker at all, this appends one for the full list, so a caller can never
/// lose the non-media paths [`partition_media`] would have re-appended. When
/// `content` does carry the exact marker for `files`, the result is
/// byte-identical to `content`.
pub fn content_with_all_paths(content: &str, files: &[String]) -> String {
    append_files_marker(strip_files_marker(content, files), files)
}

/// Read a media attachment's bytes, degrading to `None` (caller falls back to
/// the path form) when the file vanished or grew past the limit between
/// classification and read.
pub async fn read_media_bytes(attachment: &MediaAttachment) -> Option<Vec<u8>> {
    let read = tokio::time::timeout(MEDIA_READ_TIMEOUT, tokio::fs::read(Path::new(&attachment.path))).await;
    match read {
        Err(_) => {
            warn!("media attachment read timed out; sending as path");
            None
        }
        Ok(result) => match result {
            Ok(bytes)
                if bytes.len() as u64 <= MAX_MEDIA_BLOCK_BYTES
                    && bytes.len() as u64 == attachment.size
                    && hex::encode(Sha256::digest(&bytes)) == attachment.sha256 =>
            {
                Some(bytes)
            }
            Ok(bytes) if bytes.len() as u64 <= MAX_MEDIA_BLOCK_BYTES => {
                warn!("media attachment changed after admission; sending as path");
                None
            }
            Ok(bytes) => {
                warn!(
                    bytes = bytes.len(),
                    "media attachment exceeds block size limit at read; sending as path"
                );
                None
            }
            Err(err) => {
                warn!(error = %err, "media attachment read failed; sending as path");
                None
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::{PromptAttachmentMediaType, PromptAttachmentSource};

    const CAPS_IMAGE: PromptMediaCaps = PromptMediaCaps {
        image: true,
        audio: false,
    };
    const CAPS_ALL: PromptMediaCaps = PromptMediaCaps {
        image: true,
        audio: true,
    };

    fn inline(content: &str, paths: &[&str]) -> String {
        format!("{content}\n\n{AIONUI_FILES_MARKER}\n{}", paths.join("\n"))
    }

    fn temp_file(name: &str, bytes: &[u8]) -> String {
        let dir = std::env::temp_dir().join("aionui-media-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn png() -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(1, 1)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    fn prepared_image(filename: &str) -> PromptAttachmentV1 {
        PromptAttachmentV1 {
            attachment_id: format!("attachment:{filename}"),
            source: PromptAttachmentSource::Project,
            filename: filename.to_owned(),
            mime_type: "image/png".to_owned(),
            size: 1,
            sha256: "abc".to_owned(),
            width: Some(1),
            height: Some(1),
            media_type: PromptAttachmentMediaType::Image,
            delivery: PromptAttachmentDelivery::Pending,
            reason: None,
        }
    }

    #[tokio::test]
    async fn no_caps_is_byte_identical_passthrough() {
        let img = temp_file("a.png", b"png");
        let content = inline("hello", &[&img]);
        let part = partition_media(&content, std::slice::from_ref(&img), &[], PromptMediaCaps::default()).await;
        assert_eq!(part.content, content);
        assert_eq!(part.path_files, vec![img]);
        assert!(part.media.is_empty());
    }

    #[tokio::test]
    async fn image_partitions_to_media_and_marker_is_removed() {
        let img = temp_file("b.png", &png());
        let content = inline("look at this", &[&img]);
        let part = partition_media(&content, std::slice::from_ref(&img), &[], CAPS_IMAGE).await;
        assert_eq!(part.content, "look at this");
        assert!(part.path_files.is_empty());
        assert_eq!(part.media.len(), 1);
        assert_eq!(part.media[0].mime, "image/png");
        assert_eq!(part.media[0].kind, MediaKind::Image);
        assert_eq!(part.media[0].width, Some(1));
        assert_eq!(part.media[0].height, Some(1));
        assert_eq!(part.media[0].sha256.len(), 64);
    }

    #[tokio::test]
    async fn prepared_image_records_native_delivery_after_admission() {
        let img = temp_file("delivery.png", &png());
        let content = inline("look", &[&img]);
        let part = partition_media(
            &content,
            std::slice::from_ref(&img),
            &[prepared_image("delivery.png")],
            CAPS_IMAGE,
        )
        .await;

        assert_eq!(part.deliveries[0].delivery, PromptAttachmentDelivery::Native);
        assert_eq!(part.media[0].attachment_index, 0);
    }

    #[tokio::test]
    async fn prepared_image_records_capability_fallback_without_decoding() {
        let img = temp_file("unsupported.png", &png());
        let content = inline("look", &[&img]);
        let part = partition_media(
            &content,
            std::slice::from_ref(&img),
            &[prepared_image("unsupported.png")],
            PromptMediaCaps::default(),
        )
        .await;

        assert_eq!(part.deliveries[0].delivery, PromptAttachmentDelivery::PathFallback);
        assert_eq!(part.deliveries[0].reason.as_deref(), Some("capability_unsupported"));
        assert!(part.media.is_empty());
    }

    #[tokio::test]
    async fn mixed_files_keep_non_media_in_marker() {
        let img = temp_file("c.png", &png());
        let doc = temp_file("c.pdf", b"pdf");
        let content = inline("mix", &[&img, &doc]);
        let part = partition_media(&content, &[img.clone(), doc.clone()], &[], CAPS_IMAGE).await;
        assert_eq!(part.content, inline("mix", &[&doc]));
        assert_eq!(part.path_files, vec![doc]);
        assert_eq!(part.media.len(), 1);
        assert_eq!(part.media[0].path, img);
    }

    #[tokio::test]
    async fn audio_needs_audio_cap() {
        let mp3 = temp_file("d.mp3", b"ID3\x04\x00\x00\x00\x00\x00\x00");
        let content = inline("song", &[&mp3]);
        let no_audio = partition_media(&content, std::slice::from_ref(&mp3), &[], CAPS_IMAGE).await;
        assert!(no_audio.media.is_empty());
        assert_eq!(no_audio.content, content);
        let with_audio = partition_media(&content, std::slice::from_ref(&mp3), &[], CAPS_ALL).await;
        assert_eq!(with_audio.media.len(), 1);
        assert_eq!(with_audio.media[0].kind, MediaKind::Audio);
        assert_eq!(with_audio.media[0].mime, "audio/mpeg");
    }

    #[tokio::test]
    async fn invalid_or_mismatched_audio_falls_back_to_paths() {
        let invalid = temp_file("invalid.mp3", b"not audio");
        let mismatch = temp_file("mismatch.wav", b"ID3\x04\x00\x00\x00\x00\x00\x00");
        let files = vec![invalid, mismatch];
        let content = inline("listen", &[&files[0], &files[1]]);
        let part = partition_media(&content, &files, &[], CAPS_ALL).await;
        assert!(part.media.is_empty());
        assert_eq!(part.path_files, files);
        assert_eq!(part.content, content);
    }

    #[tokio::test]
    async fn svg_and_missing_and_oversized_stay_paths() {
        let svg = temp_file("e.svg", b"<svg/>");
        let missing = "/nonexistent/aionui-media-test.png".to_owned();
        let big = temp_file("f.png", &vec![0u8; (MAX_MEDIA_BLOCK_BYTES + 1) as usize]);
        let files = vec![svg, missing, big];
        let content = inline("all degrade", &[&files[0], &files[1], &files[2]]);
        let part = partition_media(&content, &files, &[], CAPS_ALL).await;
        assert!(part.media.is_empty());
        assert_eq!(part.path_files, files);
        assert_eq!(part.content, content);
    }

    #[tokio::test]
    async fn all_paths_content_keeps_media_paths_in_the_marker() {
        let img = temp_file("h.png", &png());
        let doc = temp_file("h.pdf", b"pdf");
        let files = vec![img.clone(), doc.clone()];
        let content = inline("mix", &[&img, &doc]);
        // partition drops the image path from the marker...
        let part = partition_media(&content, &files, &[], CAPS_IMAGE).await;
        assert_eq!(part.content, inline("mix", &[&doc]));
        // ...while the all-paths form keeps both, byte-identical to the input.
        assert_eq!(content_with_all_paths(&content, &files), content);
    }

    #[test]
    fn all_paths_content_appends_a_marker_when_none_present() {
        // The trap this helper exists for: with no marker in `content`, falling
        // back to the raw text would lose EVERY path, and partition would have
        // appended a marker for the non-media ones. Rebuild the full list.
        let img = temp_file("i.png", &png());
        let doc = temp_file("i.pdf", b"pdf");
        let files = vec![img.clone(), doc.clone()];
        assert_eq!(content_with_all_paths("bare", &files), inline("bare", &[&img, &doc]));
    }

    #[test]
    fn all_paths_content_is_a_noop_without_files() {
        assert_eq!(content_with_all_paths("just text", &[]), "just text");
    }

    #[tokio::test]
    async fn content_without_marker_still_partitions() {
        let img = temp_file("g.png", &png());
        let part = partition_media("bare", std::slice::from_ref(&img), &[], CAPS_IMAGE).await;
        assert_eq!(part.content, "bare");
        assert_eq!(part.media.len(), 1);
        assert_eq!(part.media[0].mime, "image/png");
    }

    #[tokio::test]
    async fn invalid_or_mismatched_images_fall_back_to_paths() {
        let invalid = temp_file("invalid.png", b"not an image");
        let mismatch = temp_file("mismatch.jpg", &png());
        let files = vec![invalid, mismatch];
        let content = inline("inspect", &[&files[0], &files[1]]);
        let part = partition_media(&content, &files, &[], CAPS_IMAGE).await;
        assert!(part.media.is_empty());
        assert_eq!(part.path_files, files);
        assert_eq!(part.content, content);
    }
}
