//! Capability-aware partition of message attachments into native media
//! blocks vs path-text files.
//!
//! Applied at agent dispatch time — the only layer that knows the target
//! agent's prompt capabilities. The persisted/broadcast message keeps the
//! full `[[AION_FILES]]` form (UI chips and history are untouched); only
//! the agent-bound copy is rewritten. Mirrors the aionrs precedent in
//! `manager/aionrs/content.rs`: strip the trailing marker block when it
//! matches `files` exactly, then rebuild.

use std::path::Path;

use aionui_common::constants::AIONUI_FILES_MARKER;
use tracing::warn;

use crate::types::PromptMediaCaps;

/// Max bytes for a single attachment sent as an inline base64 content block.
/// Above this the attachment degrades to a path (Claude's hard per-image API
/// limit, and a sane ceiling for one wire frame).
pub const MAX_MEDIA_BLOCK_BYTES: u64 = 5 * 1024 * 1024;

/// Coarse media classification for prompt content blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Audio,
}

/// An attachment that should be delivered as a native content block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaAttachment {
    /// Absolute path on the local filesystem.
    pub path: String,
    /// Full mime type (e.g. `image/png`, `audio/mpeg`).
    pub mime: String,
    pub kind: MediaKind,
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
}

/// Split `files` into native-block media vs path attachments, honoring the
/// agent's declared capabilities, and rewrite `content`'s trailing marker
/// block to list only the path attachments.
///
/// Degradation rules (attachment stays a path): capability not declared,
/// non-media mime, SVG (vision APIs reject it), file missing/unreadable, or
/// larger than [`MAX_MEDIA_BLOCK_BYTES`]. With `caps == default()` the input
/// passes through byte-identical.
pub fn partition_media(content: &str, files: &[String], caps: PromptMediaCaps) -> MediaPartition {
    if files.is_empty() || caps == PromptMediaCaps::default() {
        return MediaPartition {
            content: content.to_owned(),
            path_files: files.to_vec(),
            media: Vec::new(),
        };
    }

    let mut path_files = Vec::new();
    let mut media = Vec::new();
    for path in files {
        match classify(path, caps) {
            Some(attachment) => media.push(attachment),
            None => path_files.push(path.clone()),
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
    }
}

/// Classify one attachment; `Some` means "send as a native block".
fn classify(path: &str, caps: PromptMediaCaps) -> Option<MediaAttachment> {
    let mime = mime_guess::from_path(path).first()?;
    let kind = match mime.type_().as_str() {
        // SVG is source text, not a raster image — vision APIs reject it.
        "image" if caps.image && mime.subtype() != "svg" => MediaKind::Image,
        "audio" if caps.audio => MediaKind::Audio,
        _ => return None,
    };
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() && meta.len() <= MAX_MEDIA_BLOCK_BYTES => Some(MediaAttachment {
            path: path.to_owned(),
            mime: mime.essence_str().to_owned(),
            kind,
        }),
        Ok(meta) if meta.is_file() => {
            warn!(
                path,
                bytes = meta.len(),
                "media attachment exceeds block size limit; sending as path"
            );
            None
        }
        _ => {
            warn!(path, "media attachment unreadable; sending as path");
            None
        }
    }
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

/// Read a media attachment's bytes, degrading to `None` (caller falls back to
/// the path form) when the file vanished or grew past the limit between
/// classification and read.
pub async fn read_media_bytes(attachment: &MediaAttachment) -> Option<Vec<u8>> {
    match tokio::fs::read(Path::new(&attachment.path)).await {
        Ok(bytes) if bytes.len() as u64 <= MAX_MEDIA_BLOCK_BYTES => Some(bytes),
        Ok(bytes) => {
            warn!(path = %attachment.path, bytes = bytes.len(), "media attachment exceeds block size limit at read; sending as path");
            None
        }
        Err(err) => {
            warn!(path = %attachment.path, error = %err, "media attachment read failed; sending as path");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn no_caps_is_byte_identical_passthrough() {
        let img = temp_file("a.png", b"png");
        let content = inline("hello", &[&img]);
        let part = partition_media(&content, std::slice::from_ref(&img), PromptMediaCaps::default());
        assert_eq!(part.content, content);
        assert_eq!(part.path_files, vec![img]);
        assert!(part.media.is_empty());
    }

    #[test]
    fn image_partitions_to_media_and_marker_is_removed() {
        let img = temp_file("b.png", b"png");
        let content = inline("look at this", &[&img]);
        let part = partition_media(&content, std::slice::from_ref(&img), CAPS_IMAGE);
        assert_eq!(part.content, "look at this");
        assert!(part.path_files.is_empty());
        assert_eq!(part.media.len(), 1);
        assert_eq!(part.media[0].mime, "image/png");
        assert_eq!(part.media[0].kind, MediaKind::Image);
    }

    #[test]
    fn mixed_files_keep_non_media_in_marker() {
        let img = temp_file("c.jpg", b"jpg");
        let doc = temp_file("c.pdf", b"pdf");
        let content = inline("mix", &[&img, &doc]);
        let part = partition_media(&content, &[img.clone(), doc.clone()], CAPS_IMAGE);
        assert_eq!(part.content, inline("mix", &[&doc]));
        assert_eq!(part.path_files, vec![doc]);
        assert_eq!(part.media.len(), 1);
        assert_eq!(part.media[0].path, img);
    }

    #[test]
    fn audio_needs_audio_cap() {
        let mp3 = temp_file("d.mp3", b"mp3");
        let content = inline("song", &[&mp3]);
        let no_audio = partition_media(&content, std::slice::from_ref(&mp3), CAPS_IMAGE);
        assert!(no_audio.media.is_empty());
        assert_eq!(no_audio.content, content);
        let with_audio = partition_media(&content, std::slice::from_ref(&mp3), CAPS_ALL);
        assert_eq!(with_audio.media.len(), 1);
        assert_eq!(with_audio.media[0].kind, MediaKind::Audio);
        assert_eq!(with_audio.media[0].mime, "audio/mpeg");
    }

    #[test]
    fn svg_and_missing_and_oversized_stay_paths() {
        let svg = temp_file("e.svg", b"<svg/>");
        let missing = "/nonexistent/aionui-media-test.png".to_owned();
        let big = temp_file("f.png", &vec![0u8; (MAX_MEDIA_BLOCK_BYTES + 1) as usize]);
        let files = vec![svg, missing, big];
        let content = inline("all degrade", &[&files[0], &files[1], &files[2]]);
        let part = partition_media(&content, &files, CAPS_ALL);
        assert!(part.media.is_empty());
        assert_eq!(part.path_files, files);
        assert_eq!(part.content, content);
    }

    #[test]
    fn content_without_marker_still_partitions() {
        let img = temp_file("g.webp", b"webp");
        let part = partition_media("bare", std::slice::from_ref(&img), CAPS_IMAGE);
        assert_eq!(part.content, "bare");
        assert_eq!(part.media.len(), 1);
        assert_eq!(part.media[0].mime, "image/webp");
    }
}
