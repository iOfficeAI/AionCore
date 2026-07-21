use aion_types::message::ContentBlock;
use aionui_common::constants::AIONUI_FILES_MARKER;

use super::build_content_blocks;

#[test]
fn keeps_image_as_path_for_on_demand_viewing() {
    let image_path = "/tmp/image.png".to_owned();
    let content = format!("look at this\n\n{AIONUI_FILES_MARKER}\n{image_path}");

    let blocks = build_content_blocks(&content, std::slice::from_ref(&image_path));

    assert_eq!(blocks.len(), 1);
    assert!(matches!(
        &blocks[0],
        ContentBlock::Text { text }
            if text == "look at this\n\n[Attached files]\n/tmp/image.png"
    ));
}

#[test]
fn preserves_literal_marker_when_suffix_does_not_match_files() {
    let literal = format!("discuss {AIONUI_FILES_MARKER}\nnot-the-attached-path");

    let blocks = build_content_blocks(&literal, &["/tmp/image.png".to_owned()]);

    assert!(matches!(
        &blocks[0],
        ContentBlock::Text { text }
            if text.starts_with(&literal) && text.ends_with("[Attached files]\n/tmp/image.png")
    ));
}

#[test]
fn appends_all_authoritative_attachment_paths() {
    let files = vec!["/tmp/notes.txt".to_owned(), "/tmp/image.png".to_owned()];

    let blocks = build_content_blocks("see attachments", &files);

    assert!(matches!(
        &blocks[0],
        ContentBlock::Text { text }
            if text == "see attachments\n\n[Attached files]\n/tmp/notes.txt\n/tmp/image.png"
    ));
}
