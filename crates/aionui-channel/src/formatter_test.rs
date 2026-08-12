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
