use std::sync::LazyLock;

use regex::Regex;

use crate::types::{ParseMode, PluginType};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormattedText {
    pub text: String,
    pub parse_mode: Option<ParseMode>,
}

/// Convert text to the target IM platform format.
///
/// - Telegram: escape HTML, then convert markdown → HTML tags
/// - Lark/DingTalk: convert HTML tags → markdown
/// - WeChat/WeCom: strip all HTML
/// - Fallback: escape HTML special chars
pub fn format_text_for_platform(text: &str, platform: PluginType) -> String {
    format_outgoing_text_for_platform(text, platform).text
}

pub fn format_outgoing_text_for_platform(text: &str, platform: PluginType) -> FormattedText {
    match platform {
        PluginType::Telegram => FormattedText {
            text: markdown_to_telegram_html(text),
            parse_mode: Some(ParseMode::HTML),
        },
        PluginType::Lark | PluginType::Dingtalk => FormattedText {
            text: html_to_markdown(text),
            parse_mode: None,
        },
        PluginType::Weixin => FormattedText {
            text: strip_html(text),
            parse_mode: None,
        },
        _ => FormattedText {
            text: escape_html(text),
            parse_mode: None,
        },
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
    let text = markdown_tables_to_cards(text);
    let s = escape_html(&text);
    let s = RE_CODE_BLOCK.replace_all(&s, "<pre><code>$1</code></pre>");
    let s = RE_INLINE_CODE.replace_all(&s, "<code>$1</code>");
    let s = RE_BOLD_STAR.replace_all(&s, "<b>$1</b>");
    let s = RE_BOLD_UNDER.replace_all(&s, "<b>$1</b>");
    let s = RE_ITALIC_STAR.replace_all(&s, "<i>$1</i>");
    let s = RE_ITALIC_UNDER.replace_all(&s, "<i>$1</i>");
    let s = RE_LINK.replace_all(&s, r#"<a href="$2">$1</a>"#);
    s.into_owned()
}

fn markdown_tables_to_cards(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut output = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        if i + 1 < lines.len() && is_table_row(lines[i]) && is_separator_row(lines[i + 1]) {
            let headers = split_table_row(lines[i]);
            i += 2;

            let mut rows = Vec::new();
            while i < lines.len() && is_table_row(lines[i]) {
                rows.push(split_table_row(lines[i]));
                i += 1;
            }

            output.push(render_table_cards(&headers, &rows));
            continue;
        }

        output.push(lines[i].to_owned());
        i += 1;
    }

    output.join("\n")
}

fn is_table_row(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.matches('|').count() >= 2
}

fn is_separator_row(line: &str) -> bool {
    if !is_table_row(line) {
        return false;
    }
    split_table_row(line)
        .iter()
        .all(|cell| !cell.is_empty() && cell.chars().all(|ch| ch == '-' || ch == ':' || ch.is_whitespace()))
}

fn split_table_row(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(str::trim)
        .map(ToOwned::to_owned)
        .collect()
}

fn render_table_cards(headers: &[String], rows: &[Vec<String>]) -> String {
    if headers.is_empty() || rows.is_empty() {
        return String::new();
    }
    if headers.len() > 5 || rows.len() > 12 {
        return format!(
            "**表格摘要：** 共 {} 列、{} 行。Telegram 不适合展示大表格，请在 WebUI 查看完整表格。",
            headers.len(),
            rows.len()
        );
    }

    rows.iter()
        .enumerate()
        .map(|(idx, row)| {
            let mut card = format!("**表格记录 {}**", idx + 1);
            for (col_idx, header) in headers.iter().enumerate() {
                let value = row.get(col_idx).map(String::as_str).unwrap_or("");
                if !value.trim().is_empty() {
                    card.push_str(&format!("\n**{}:** {}", header, value));
                }
            }
            card
        })
        .collect::<Vec<_>>()
        .join("\n\n")
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
