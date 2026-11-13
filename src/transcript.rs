use std::borrow::Cow;

use memchr::memmem;
use serde::Deserialize;

use crate::time::{SECONDS_PER_DAY, SECONDS_PER_HOUR, SECONDS_PER_MINUTE, days_from_civil};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Summary,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Message { role: Role, text: String },
    Title { text: String, custom: bool },
    PrLink { number: u64, repo: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Parsed {
    pub item: Option<Item>,
    pub ts: Option<i64>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub entrypoint: Option<String>,
    pub session_kind: Option<String>,
}

// Must name the same types as the arms of `item_of`: a type missing here or from `item_of` is
// silently dropped, with no error.
const WANTED_TYPES: [&[u8]; 5] = [
    br#""type":"user""#,
    br#""type":"assistant""#,
    br#""type":"ai-title""#,
    br#""type":"custom-title""#,
    br#""type":"pr-link""#,
];

/// Text that the harness injects into user turns; it is not something the user typed.
const INJECTED_PREFIXES: [&str; 9] = [
    "<task-notification>",
    "<local-command-",
    "<command-name>",
    "<command-message>",
    "<artifact-content-auth",
    "Another Claude session sent a message:",
    "Base directory for this skill:",
    "[Request interrupted",
    "Caveat:",
];

const REMINDER_OPEN: &str = "<system-reminder>";
const REMINDER_CLOSE: &str = "</system-reminder>";

/// Cheap pre-filter so large unwanted lines (tool blobs) are never JSON-parsed.
fn wanted(line: &[u8]) -> bool {
    WANTED_TYPES.iter().any(|needle| memmem::find(line, needle).is_some())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Raw {
    #[serde(rename = "type")]
    kind: String,
    timestamp: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "gitBranch")]
    git_branch: Option<String>,
    entrypoint: Option<String>,
    #[serde(rename = "sessionKind")]
    session_kind: Option<String>,
    #[serde(rename = "isMeta")]
    is_meta: bool,
    #[serde(rename = "isCompactSummary")]
    is_compact_summary: bool,
    #[serde(rename = "isSidechain")]
    is_sidechain: bool,
    #[serde(rename = "promptSource")]
    prompt_source: Option<String>,
    origin: Option<Origin>,
    message: Option<Message>,
    #[serde(rename = "customTitle")]
    custom_title: Option<String>,
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
    #[serde(rename = "prNumber")]
    pr_number: Option<u64>,
    #[serde(rename = "prRepository")]
    pr_repository: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Origin {
    kind: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Message {
    content: Content,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Blocks(Vec<Block>),
}

impl Default for Content {
    fn default() -> Self {
        Content::Blocks(Vec::new())
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

/// `None` means the line is not wanted or not valid JSON.
pub fn parse_line(line: &[u8]) -> Option<Parsed> {
    if !wanted(line) {
        return None;
    }
    // serde_json rejects invalid UTF-8 inside strings; lossy conversion keeps the rest of the line.
    let text = String::from_utf8_lossy(line);
    let raw: Raw = serde_json::from_str(&text).ok()?;
    let item = item_of(&raw);
    Some(Parsed {
        item,
        ts: raw.timestamp.as_deref().and_then(parse_timestamp),
        cwd: raw.cwd,
        branch: raw.git_branch,
        entrypoint: raw.entrypoint,
        session_kind: raw.session_kind,
    })
}

fn item_of(raw: &Raw) -> Option<Item> {
    match raw.kind.as_str() {
        "user" | "assistant" if raw.is_sidechain => None,
        "user" => user_item(raw),
        "assistant" => assistant_item(raw),
        "custom-title" => title(raw.custom_title.as_deref(), true),
        "ai-title" => title(raw.ai_title.as_deref(), false),
        "pr-link" => Some(Item::PrLink {
            number: raw.pr_number?,
            repo: raw.pr_repository.clone(),
        }),
        _ => None,
    }
}

fn title(text: Option<&str>, custom: bool) -> Option<Item> {
    let text = text?.trim();
    (!text.is_empty()).then(|| Item::Title { text: text.to_string(), custom })
}

fn user_item(raw: &Raw) -> Option<Item> {
    // A compaction recap is a summary whether or not it also carries `isMeta`; real recaps do not.
    // Any other meta record is harness text.
    let role = if raw.is_compact_summary {
        Role::Summary
    } else if raw.is_meta {
        return None;
    } else {
        Role::User
    };
    // Denylist, not allowlist: other origin kinds (slash commands, channels) still carry real prompts.
    let injected = matches!(
        raw.origin.as_ref().and_then(|o| o.kind.as_deref()),
        Some("task-notification" | "peer")
    );
    if injected || raw.prompt_source.as_deref() == Some("system") {
        return None;
    }
    let joined = match &raw.message.as_ref()?.content {
        Content::Text(s) => Cow::Borrowed(s.as_str()),
        Content::Blocks(blocks) => {
            // A tool result turn carries machine output, not a prompt.
            if blocks.iter().any(|b| b.kind == "tool_result") {
                return None;
            }
            Cow::Owned(join_text(blocks)?)
        }
    };
    let cleaned = strip_reminders(&joined);
    let cleaned = cleaned.trim();
    if cleaned.is_empty() || INJECTED_PREFIXES.iter().any(|p| cleaned.starts_with(p)) {
        return None;
    }
    Some(Item::Message { role, text: cleaned.to_string() })
}

fn assistant_item(raw: &Raw) -> Option<Item> {
    let text = match &raw.message.as_ref()?.content {
        Content::Text(s) => Cow::Borrowed(s.as_str()),
        Content::Blocks(blocks) => Cow::Owned(join_text(blocks)?),
    };
    let text = text.trim();
    (!text.is_empty()).then(|| Item::Message { role: Role::Assistant, text: text.to_string() })
}

fn join_text(blocks: &[Block]) -> Option<String> {
    let parts: Vec<&str> = blocks
        .iter()
        .filter(|b| b.kind == "text")
        .filter_map(|b| b.text.as_deref())
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn strip_reminders(text: &str) -> Cow<'_, str> {
    if !text.contains(REMINDER_OPEN) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(REMINDER_OPEN) {
        out.push_str(&rest[..start]);
        match rest[start..].find(REMINDER_CLOSE) {
            Some(end) => rest = &rest[start + end + REMINDER_CLOSE.len()..],
            // An unclosed reminder swallows the remainder rather than leaking injected text.
            None => return Cow::Owned(out),
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// Parses `YYYY-MM-DDTHH:MM:SS[.fff](Z|±HH:MM)` to epoch seconds.
fn parse_timestamp(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, min, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let mut rest = &s[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        rest = &frac[digits..];
    }
    let offset = match rest {
        "" | "Z" => 0,
        _ if rest.len() == 6 && rest.as_bytes()[3] == b':' => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let h = rest.get(1..3)?.parse::<i64>().ok()?;
            let m = rest.get(4..6)?.parse::<i64>().ok()?;
            sign * (h * SECONDS_PER_HOUR + m * SECONDS_PER_MINUTE)
        }
        _ => return None,
    };
    let time_of_day = hour * SECONDS_PER_HOUR + min * SECONDS_PER_MINUTE + sec;
    Some(days_from_civil(year, month, day) * SECONDS_PER_DAY + time_of_day - offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: Role, text: &str) -> Option<Item> {
        Some(Item::Message { role, text: text.to_string() })
    }

    fn user(extra: &str, content: &str) -> String {
        format!(r#"{{"type":"user"{extra},"message":{{"content":{content}}}}}"#)
    }

    fn item(line: &str) -> Option<Item> {
        parse_line(line.as_bytes()).and_then(|p| p.item)
    }

    #[test]
    fn wanted_matches_known_types_only() {
        for t in ["user", "assistant", "ai-title", "custom-title", "pr-link"] {
            let line = format!(r#"{{"type":"{t}","x":1}}"#);
            assert!(wanted(line.as_bytes()), "{t}");
        }
        assert!(!wanted(br#"{"type":"api-request-blob","x":"..."}"#));
        assert!(!wanted(br#"{"type":"attachment"}"#));
    }

    #[test]
    fn user_message_with_metadata() {
        let line = r#"{"type":"user","message":{"content":"fix the retry queue"},"timestamp":"2026-11-10T12:00:00.000Z","cwd":"/w/app","gitBranch":"main","entrypoint":"cli","sessionKind":"interactive"}"#;
        let p = parse_line(line.as_bytes()).unwrap();
        assert_eq!(p.item, msg(Role::User, "fix the retry queue"));
        assert_eq!(p.ts, Some(1794312000));
        assert_eq!(p.cwd.as_deref(), Some("/w/app"));
        assert_eq!(p.branch.as_deref(), Some("main"));
        assert_eq!(p.entrypoint.as_deref(), Some("cli"));
        assert_eq!(p.session_kind.as_deref(), Some("interactive"));
    }

    #[test]
    fn timestamp_with_offset_and_without_fraction() {
        let line = r#"{"type":"user","message":{"content":"x"},"timestamp":"2026-11-10T14:00:00+02:00"}"#;
        assert_eq!(parse_line(line.as_bytes()).unwrap().ts, Some(1794312000));
        let bad = r#"{"type":"user","message":{"content":"x"},"timestamp":"yesterday"}"#;
        assert_eq!(parse_line(bad.as_bytes()).unwrap().ts, None);
    }

    #[test]
    fn content_list_joins_text_blocks() {
        let line = user("", r#"[{"type":"text","text":"a"},{"type":"text","text":"b"}]"#);
        assert_eq!(item(&line), msg(Role::User, "a\nb"));
    }

    #[test]
    fn tool_result_block_drops_item_but_keeps_cwd() {
        let line = r#"{"type":"user","cwd":"/w/app","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#;
        let p = parse_line(line.as_bytes()).unwrap();
        assert_eq!(p.item, None);
        assert_eq!(p.cwd.as_deref(), Some("/w/app"));
    }

    #[test]
    fn meta_and_compact_summary() {
        assert_eq!(item(&user(r#","isMeta":true"#, r#""hello""#)), None);
        assert_eq!(
            item(&user(r#","isMeta":true,"isCompactSummary":true"#, r#""recap""#)),
            msg(Role::Summary, "recap")
        );
        assert_eq!(
            item(&user(r#","isCompactSummary":true"#, r#""recap""#)),
            msg(Role::Summary, "recap")
        );
    }

    #[test]
    fn sidechain_records_have_no_item() {
        assert_eq!(item(&user(r#","isSidechain":true"#, r#""hello""#)), None);
        let a = r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"hi"}]}}"#;
        assert_eq!(item(a), None);
    }

    #[test]
    fn origin_and_prompt_source_filters() {
        for extra in [
            r#","origin":{"kind":"task-notification"}"#,
            r#","origin":{"kind":"peer"}"#,
            r#","promptSource":"system""#,
        ] {
            assert_eq!(item(&user(extra, r#""hello""#)), None, "{extra}");
        }
        for kind in ["human", "other"] {
            let extra = format!(r#","origin":{{"kind":"{kind}"}}"#);
            assert_eq!(item(&user(&extra, r#""hello""#)), msg(Role::User, "hello"), "{kind}");
        }
    }

    #[test]
    fn system_reminders_are_stripped() {
        assert_eq!(
            item(&user("", r#""<system-reminder>x</system-reminder>fix it""#)),
            msg(Role::User, "fix it")
        );
        assert_eq!(item(&user("", r#""<system-reminder>x</system-reminder>""#)), None);
    }

    #[test]
    fn injected_prefixes_are_dropped() {
        for prefix in [
            "<task-notification>",
            "<local-command-stdout>",
            "<command-name>",
            "<command-message>",
            "<artifact-content-authored>",
            "Another Claude session sent a message:",
            "Base directory for this skill:",
            "[Request interrupted by user]",
            "Caveat: the messages below",
        ] {
            let content = format!("\"{prefix} rest\"");
            assert_eq!(item(&user("", &content)), None, "{prefix}");
        }
    }

    #[test]
    fn assistant_keeps_text_blocks_only() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hm"},{"type":"text","text":"done"},{"type":"tool_use","name":"Bash"},{"type":"text","text":"next"}]}}"#;
        assert_eq!(item(line), msg(Role::Assistant, "done\nnext"));
        let none = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#;
        assert_eq!(item(none), None);
    }

    #[test]
    fn titles() {
        assert_eq!(
            item(r#"{"type":"custom-title","customTitle":"Grid filter"}"#),
            Some(Item::Title { text: "Grid filter".into(), custom: true })
        );
        assert_eq!(
            item(r#"{"type":"ai-title","aiTitle":"Fix grid"}"#),
            Some(Item::Title { text: "Fix grid".into(), custom: false })
        );
    }

    #[test]
    fn pr_link() {
        assert_eq!(
            item(r#"{"type":"pr-link","prNumber":42,"prRepository":"acme/widgets"}"#),
            Some(Item::PrLink { number: 42, repo: Some("acme/widgets".into()) })
        );
    }

    #[test]
    fn malformed_input_does_not_panic() {
        let mut bytes = br#"{"type":"user","message":{"content":"ab"#.to_vec();
        bytes.push(0xff);
        bytes.extend_from_slice(br#"cd"}}"#);
        let p = parse_line(&bytes).unwrap();
        assert_eq!(p.item, msg(Role::User, "ab\u{fffd}cd"));

        let lone = br#"{"type":"user","message":{"content":"x\ud83dy"}}"#;
        let _ = parse_line(lone);

        assert_eq!(parse_line(b"{not json"), None);
        assert_eq!(parse_line(br#"{"type":"attachment"}"#), None);
    }
}
