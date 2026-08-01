use std::sync::LazyLock;

use regex::Regex;

use crate::types::PluginType;

/// Convert text to the target IM platform format.
///
/// - Telegram: escape HTML, then convert markdown → HTML tags
/// - Lark/DingTalk: convert HTML tags → markdown
/// - Slack: convert common markdown → Slack `mrkdwn`
/// - WeChat/WeCom: strip all HTML
/// - Fallback: escape HTML special chars
pub fn format_text_for_platform(text: &str, platform: PluginType) -> String {
    match platform {
        PluginType::Telegram => markdown_to_telegram_html(text),
        PluginType::Lark | PluginType::Dingtalk => html_to_markdown(text),
        PluginType::Slack => markdown_to_slack_mrkdwn(text),
        PluginType::Weixin => strip_html(text),
        _ => escape_html(text),
    }
}

// ── Telegram ─────────────────────────────────────────────────────

static RE_CODE_BLOCK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"```(?:\w*)\n?([\s\S]*?)```").unwrap());
static RE_INLINE_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`]+)`").unwrap());
static RE_BOLD_STAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*(.+?)\*\*").unwrap());
static RE_BOLD_UNDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"__(.+?)__").unwrap());
static RE_ITALIC_STAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*(.+?)\*").unwrap());
static RE_ITALIC_UNDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"_(.+?)_").unwrap());
static RE_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap());

fn markdown_to_telegram_html(text: &str) -> String {
    let s = escape_html(text);
    let s = RE_CODE_BLOCK.replace_all(&s, "<pre><code>$1</code></pre>");
    let s = RE_INLINE_CODE.replace_all(&s, "<code>$1</code>");
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

// ── Slack mrkdwn ─────────────────────────────────────────────────
//
// Slack does not render standard Markdown. With `mrkdwn: true` it expects:
//   *bold*   _italic_   ~strike~   `code`   ```blocks```
//   <url|label> links
// Headers (##) are not supported — convert to bold lines.

static RE_SLACK_HEADER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^(#{1,6})\s+(.+)$").unwrap());
static RE_SLACK_STRIKE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"~~(.+?)~~").unwrap());
static RE_SLACK_BOLD_STAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*(.+?)\*\*").unwrap());
static RE_SLACK_BOLD_UNDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"__(.+?)__").unwrap());
// Single-asterisk italic only when not already Slack bold (*text*).
static RE_SLACK_ITALIC_STAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?P<pre>^|[^*])\*(?P<body>[^*\n]+?)\*(?P<post>$|[^*])").unwrap());

/// Convert common Markdown to Slack mrkdwn.
fn markdown_to_slack_mrkdwn(text: &str) -> String {
    // Protect fenced/inline code so formatting inside them is left alone.
    let mut blocks: Vec<String> = Vec::new();
    let s = RE_CODE_BLOCK.replace_all(text, |caps: &regex::Captures| {
        let idx = blocks.len();
        blocks.push(format!("```{}```", &caps[1]));
        format!("\u{E000}BLOCK{idx}\u{E001}")
    });
    let mut inlines: Vec<String> = Vec::new();
    let s = RE_INLINE_CODE.replace_all(&s, |caps: &regex::Captures| {
        let idx = inlines.len();
        inlines.push(format!("`{}`", &caps[1]));
        format!("\u{E000}CODE{idx}\u{E001}")
    });

    // Links before other markup so brackets don't get mangled.
    let s = RE_LINK.replace_all(&s, "<$2|$1>");

    // Headers → bold (protect placeholders so italic pass won't rewrite them).
    let mut bolds: Vec<String> = Vec::new();
    let s = RE_SLACK_HEADER.replace_all(&s, |caps: &regex::Captures| {
        let idx = bolds.len();
        bolds.push(caps[2].to_owned());
        format!("\u{E000}BOLD{idx}\u{E001}")
    });

    // Strikethrough ~~x~~ → ~x~
    let s = RE_SLACK_STRIKE.replace_all(&s, "~$1~");

    // Bold **x** / __x__ → placeholders (Slack bold is *x*, applied on restore)
    let s = RE_SLACK_BOLD_STAR.replace_all(&s, |caps: &regex::Captures| {
        let idx = bolds.len();
        bolds.push(caps[1].to_owned());
        format!("\u{E000}BOLD{idx}\u{E001}")
    });
    let s = RE_SLACK_BOLD_UNDER.replace_all(&s, |caps: &regex::Captures| {
        let idx = bolds.len();
        bolds.push(caps[1].to_owned());
        format!("\u{E000}BOLD{idx}\u{E001}")
    });

    // Italic *x* (remaining singles) → _x_
    let s = RE_SLACK_ITALIC_STAR.replace_all(&s, "${pre}_${body}_${post}");

    // Materialize bold as Slack *text*
    let mut s = s.into_owned();
    for (i, body) in bolds.iter().enumerate() {
        s = s.replace(&format!("\u{E000}BOLD{i}\u{E001}"), &format!("*{body}*"));
    }
    let s = s;

    // Escape & < > that are not already part of <url|label> or placeholders.
    // We escape ampersands first, then leave our intentional <url|label> alone by
    // temporarily protecting them.
    let mut links: Vec<String> = Vec::new();
    let s = {
        static RE_SLACK_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<(https?://[^|>]+)\|([^>]+)>").unwrap());
        RE_SLACK_LINK.replace_all(&s, |caps: &regex::Captures| {
            let idx = links.len();
            links.push(caps[0].to_owned());
            format!("\u{E000}LINK{idx}\u{E001}")
        })
    };
    let s = s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");

    // Restore protected segments (reverse order of replacement).
    let mut out = s;
    for (i, link) in links.iter().enumerate() {
        out = out.replace(&format!("\u{E000}LINK{i}\u{E001}"), link);
    }
    for (i, code) in inlines.iter().enumerate() {
        out = out.replace(&format!("\u{E000}CODE{i}\u{E001}"), code);
    }
    for (i, block) in blocks.iter().enumerate() {
        out = out.replace(&format!("\u{E000}BLOCK{i}\u{E001}"), block);
    }
    out
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
