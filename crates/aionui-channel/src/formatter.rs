use std::sync::LazyLock;

use regex::Regex;

use crate::types::{ParseMode, PluginType};

/// Convert text to the target IM platform format.
///
/// - Telegram: escape HTML, then convert markdown → HTML tags
/// - Lark/DingTalk: convert HTML tags → markdown
/// - Slack: escape `&<>`, then convert markdown → Slack mrkdwn
/// - Discord: pass markdown through unchanged (Discord renders it natively;
///   accidental @mentions are suppressed at send time via `allowed_mentions`)
/// - WeChat: strip all HTML
pub fn format_text_for_platform(text: &str, platform: PluginType) -> String {
    match platform {
        PluginType::Telegram => markdown_to_telegram_html(text),
        PluginType::Lark | PluginType::Dingtalk => html_to_markdown(text),
        PluginType::Slack => markdown_to_slack_mrkdwn(text),
        PluginType::Discord => text.to_string(),
        PluginType::Weixin => strip_html(text),
    }
}

/// The parse mode that matches the output of [`format_text_for_platform`].
///
/// Telegram receives HTML (see `markdown_to_telegram_html`), so the outgoing
/// message must declare `parse_mode=HTML` or Telegram renders the tags as
/// literal text. Other platforms either take markdown directly or do not
/// support a parse mode.
pub fn parse_mode_for_platform(platform: PluginType) -> Option<ParseMode> {
    match platform {
        PluginType::Telegram => Some(ParseMode::HTML),
        _ => None,
    }
}

// ── Slack (mrkdwn) ───────────────────────────────────────────────

static RE_MD_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap());
static RE_MD_BOLD_STAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*(.+?)\*\*").unwrap());
static RE_MD_BOLD_UNDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"__(.+?)__").unwrap());
static RE_MD_STRIKE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"~~(.+?)~~").unwrap());

/// Convert standard markdown to Slack mrkdwn.
///
/// Slack requires `&`, `<`, `>` escaped in message text, uses single `*` for
/// bold, single `~` for strikethrough, and `<url|text>` for links. Single-`*`
/// italic is intentionally left untouched — converting it would clobber the
/// bold delimiter, and rendering markdown italic as Slack bold is a harmless
/// cosmetic difference.
fn markdown_to_slack_mrkdwn(text: &str) -> String {
    let s = escape_html(text);
    let s = RE_MD_LINK.replace_all(&s, "<$2|$1>");
    let s = RE_MD_BOLD_STAR.replace_all(&s, "*$1*");
    let s = RE_MD_BOLD_UNDER.replace_all(&s, "*$1*");
    let s = RE_MD_STRIKE.replace_all(&s, "~$1~");
    s.into_owned()
}

// ── Telegram ─────────────────────────────────────────────────────

static RE_CODE_BLOCK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"```(?:\w*)\n?([\s\S]*?)```").unwrap());
static RE_INLINE_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`]+)`").unwrap());
static RE_BOLD_STAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*(.+?)\*\*").unwrap());
static RE_BOLD_UNDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"__(.+?)__").unwrap());
static RE_ITALIC_STAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*(.+?)\*").unwrap());
static RE_ITALIC_UNDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"_(.+?)_").unwrap());
static RE_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap());

static RE_HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ {0,3}(#{1,6})[ \t]+(.+?)[ \t]*#*[ \t]*$").unwrap());

fn markdown_to_telegram_html(text: &str) -> String {
    // Telegram's HTML mode has no heading tag, so headings are rendered as
    // bold standalone lines. Fenced code blocks are emitted directly (and
    // never passed through the inline converters) so that comment lines such
    // as `# note` inside a code block are not mistaken for headings.
    let mut out = String::new();
    let mut last = 0usize;
    for cap in RE_CODE_BLOCK.captures_iter(text) {
        let whole = cap.get(0).unwrap();
        out.push_str(&telegram_plain_segment(&text[last..whole.start()]));
        out.push_str("<pre><code>");
        out.push_str(&escape_html(cap.get(1).map(|m| m.as_str()).unwrap_or("")));
        out.push_str("</code></pre>");
        last = whole.end();
    }
    out.push_str(&telegram_plain_segment(&text[last..]));
    out
}

/// Format a non-code segment: headings first (per line), then inline styles.
fn telegram_plain_segment(segment: &str) -> String {
    let mut out = String::new();
    for (idx, line) in segment.split('\n').enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        match RE_HEADING.captures(line) {
            Some(cap) => {
                let body = escape_html(cap.get(2).map(|m| m.as_str()).unwrap_or(""));
                out.push_str("<b>");
                out.push_str(&telegram_inline(&body));
                out.push_str("</b>");
            }
            None => out.push_str(&telegram_inline(&escape_html(line))),
        }
    }
    out
}

fn telegram_inline(text: &str) -> String {
    let s = RE_INLINE_CODE.replace_all(text, "<code>$1</code>");
    let s = RE_BOLD_STAR.replace_all(&s, "<b>$1</b>");
    let s = RE_BOLD_UNDER.replace_all(&s, "<b>$1</b>");
    let s = RE_ITALIC_STAR.replace_all(&s, "<i>$1</i>");
    let s = RE_ITALIC_UNDER.replace_all(&s, "<i>$1</i>");
    let s = RE_LINK.replace_all(&s, r#"<a href="$2">$1</a>"#);
    s.into_owned()
}

// ── Lark / DingTalk ──────────────────────────────────────────────

static RE_PRE_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<pre><code[^>]*>([\s\S]*?)</code></pre>").unwrap());
static RE_HTML_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<code>([^<]+)</code>").unwrap());
static RE_HTML_B: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<b>([\s\S]*?)</b>").unwrap());
static RE_HTML_STRONG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<strong>([\s\S]*?)</strong>").unwrap());
static RE_HTML_I: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<i>([\s\S]*?)</i>").unwrap());
static RE_HTML_EM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<em>([\s\S]*?)</em>").unwrap());
static RE_HTML_SAFE_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<a\s+href="((?:https?://|mailto:|/)[^"]*)"[^>]*>([^<]*)</a>"#).unwrap());
static RE_HTML_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());

fn html_to_markdown(text: &str) -> String {
    let s = decode_safe_entities(text);
    let s = RE_PRE_CODE.replace_all(&s, "```\n$1```");
    let s = RE_HTML_CODE.replace_all(&s, "`$1`");
    let s = RE_HTML_B.replace_all(&s, "**$1**");
    let s = RE_HTML_STRONG.replace_all(&s, "**$1**");
    let s = RE_HTML_I.replace_all(&s, "*$1*");
    let s = RE_HTML_EM.replace_all(&s, "*$1*");
    let s = RE_HTML_SAFE_LINK.replace_all(&s, "[$2]($1)");
    strip_tags_loop(s.as_ref())
}

// ── WeChat ───────────────────────────────────────────────────────

fn strip_html(text: &str) -> String {
    let s = strip_tags_loop(text);
    let s = decode_all_entities(&s);
    s.replace(['<', '>'], "")
}

// ── Helpers ──────────────────────────────────────────────────────

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn strip_tags_loop(text: &str) -> String {
    let mut result = text.to_owned();
    loop {
        let stripped = RE_HTML_TAG.replace_all(&result, "");
        if stripped == result {
            break;
        }
        result = stripped.into_owned();
    }
    result
}

/// Decode only safe entities (quotes, numeric). Never decode &lt;/&gt;/&amp;
/// to prevent tag injection in Lark/DingTalk output.
fn decode_safe_entities(text: &str) -> String {
    static RE_HEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&#x([0-9a-fA-F]+);").unwrap());
    static RE_DEC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&#(\d+);").unwrap());

    let s = text.replace("&quot;", "\"");
    let s = s.replace("&#39;", "'");
    let s = s.replace("&apos;", "'");
    let s = RE_HEX.replace_all(&s, |caps: &regex::Captures| {
        u32::from_str_radix(&caps[1], 16)
            .ok()
            .and_then(char::from_u32)
            .map(|c| c.to_string())
            .unwrap_or_else(|| caps[0].to_owned())
    });
    let s = RE_DEC.replace_all(&s, |caps: &regex::Captures| {
        caps[1]
            .parse::<u32>()
            .ok()
            .and_then(char::from_u32)
            .map(|c| c.to_string())
            .unwrap_or_else(|| caps[0].to_owned())
    });
    s.into_owned()
}

/// Decode all common HTML entities (for WeChat plain-text output).
fn decode_all_entities(text: &str) -> String {
    let s = decode_safe_entities(text);
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
}

#[cfg(test)]
#[path = "formatter_test.rs"]
mod formatter_test;
