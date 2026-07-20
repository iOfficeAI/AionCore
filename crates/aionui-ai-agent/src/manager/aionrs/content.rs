use aion_types::message::ContentBlock;
use aionui_common::constants::AIONUI_FILES_MARKER;

const ATTACHED_FILES_HEADER: &str = "[Attached files]";

/// Build provider-independent user input from the message and its attachments.
///
/// Attachments remain local paths in the conversation snapshot. Aionrs's
/// `ViewImage` tool loads an image only when a vision-capable model requests
/// it, so a text-only leader can still receive the turn and delegate the path.
pub(super) fn build_content_blocks(content: &str, files: &[String]) -> Vec<ContentBlock> {
    let mut text = strip_attachment_metadata(content, files).trim().to_owned();
    if !files.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(ATTACHED_FILES_HEADER);
        for file_path in files {
            text.push('\n');
            text.push_str(file_path);
        }
    }

    if text.is_empty() {
        Vec::new()
    } else {
        vec![ContentBlock::Text { text }]
    }
}

fn strip_attachment_metadata<'a>(content: &'a str, files: &[String]) -> &'a str {
    if files.is_empty() {
        return content;
    }
    let Some((user_text, metadata)) = content.rsplit_once(AIONUI_FILES_MARKER) else {
        return content;
    };
    let metadata_files = metadata.lines().map(str::trim).filter(|line| !line.is_empty());
    if metadata_files.eq(files.iter().map(String::as_str)) {
        user_text
    } else {
        content
    }
}

#[cfg(test)]
#[path = "content_test.rs"]
mod content_test;
