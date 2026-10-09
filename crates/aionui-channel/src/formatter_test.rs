use super::*;

#[test]
fn slack_escapes_special_chars() {
    let out = format_text_for_platform("a < b & c > d", PluginType::Slack);
    assert_eq!(out, "a &lt; b &amp; c &gt; d");
}

#[test]
fn slack_converts_bold() {
    assert_eq!(format_text_for_platform("**bold**", PluginType::Slack), "*bold*");
    assert_eq!(format_text_for_platform("__bold__", PluginType::Slack), "*bold*");
}

#[test]
fn slack_converts_strikethrough() {
    assert_eq!(format_text_for_platform("~~gone~~", PluginType::Slack), "~gone~");
}

#[test]
fn slack_converts_link() {
    assert_eq!(
        format_text_for_platform("[docs](https://example.com)", PluginType::Slack),
        "<https://example.com|docs>"
    );
}

#[test]
fn slack_leaves_inline_code_untouched() {
    assert_eq!(format_text_for_platform("`code`", PluginType::Slack), "`code`");
}

#[test]
fn slack_combined() {
    let out = format_text_for_platform("See **[link](https://x.io)** now", PluginType::Slack);
    assert_eq!(out, "See *<https://x.io|link>* now");
}

// Regression guard: Slack must no longer fall through to the bare HTML-escape
// branch (which left markdown bold/links unconverted).
#[test]
fn slack_not_plain_escape_fallback() {
    let out = format_text_for_platform("**x**", PluginType::Slack);
    assert_ne!(out, "**x**");
    assert_eq!(out, "*x*");
}

// Discord renders markdown natively — text passes through unchanged (no HTML
// escaping of `<`, `>`, `&`; the old fallback would have mangled them).
#[test]
fn discord_passes_markdown_through_unchanged() {
    let input = "**bold** _i_ `code` <@123> a < b & c";
    assert_eq!(format_text_for_platform(input, PluginType::Discord), input);
}

// Telegram headings: HTML mode has no heading tag, so headings become bold
// standalone lines instead of leaking literal "##".
#[test]
fn telegram_h1_becomes_bold() {
    assert_eq!(
        format_text_for_platform("# Title", PluginType::Telegram),
        "<b>Title</b>"
    );
}

#[test]
fn telegram_h2_and_h3_become_bold() {
    assert_eq!(
        format_text_for_platform("## Section", PluginType::Telegram),
        "<b>Section</b>"
    );
    assert_eq!(format_text_for_platform("### Sub", PluginType::Telegram), "<b>Sub</b>");
}

#[test]
fn telegram_heading_strips_trailing_hashes() {
    assert_eq!(
        format_text_for_platform("## Section ##", PluginType::Telegram),
        "<b>Section</b>"
    );
}

#[test]
fn telegram_heading_preserves_inline_styles() {
    assert_eq!(
        format_text_for_platform("## See **bold**", PluginType::Telegram),
        "<b>See <b>bold</b></b>"
    );
}

#[test]
fn telegram_heading_only_when_line_starts_with_hash() {
    // A '#' mid-line is not a heading.
    assert_eq!(format_text_for_platform("issue #42", PluginType::Telegram), "issue #42");
}

// A shell/Python comment inside a fenced code block must stay literal, not be
// rewritten as a heading.
#[test]
fn telegram_code_block_comments_are_not_headings() {
    let out = format_text_for_platform("```\n# not a heading\nls -la\n```", PluginType::Telegram);
    assert_eq!(out, "<pre><code># not a heading\nls -la\n</code></pre>");
}

#[test]
fn telegram_multi_line_heading_then_body() {
    let out = format_text_for_platform("## Head\nbody **x**", PluginType::Telegram);
    assert_eq!(out, "<b>Head</b>\nbody <b>x</b>");
}
