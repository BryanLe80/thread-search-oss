use serde::Serialize;

use crate::search::{Hit, SearchResult};
use crate::time::{SECONDS_PER_DAY, SECONDS_PER_HOUR, SECONDS_PER_MINUTE, utc};
use crate::update::UpdateReport;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    NoIndex,
    Building,
    Ok,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            State::NoIndex => "no_index",
            State::Building => "building",
            State::Ok => "ok",
        }
    }
}

// Field order is the JSON key order of the contract with the desktop overlay.
#[derive(Serialize)]
pub struct JsonOutput<'a> {
    state: &'static str,
    matched_all: u32,
    matched_some: u32,
    results: Vec<JsonHit<'a>>,
}

#[derive(Serialize)]
struct JsonHit<'a> {
    agent: &'a str,
    session_id: &'a str,
    title: &'a str,
    last_active: i64,
    folder: &'a str,
    folder_state: &'static str,
    folder_name: &'a str,
    branch: Option<&'a str>,
    keys: &'a [String],
    prompts: u32,
    desk_job: bool,
    #[serde(rename = "match")]
    matched: JsonMatch<'a>,
}

#[derive(Serialize)]
struct JsonMatch<'a> {
    role: &'a str,
    segments: Vec<JsonSegment<'a>>,
}

#[derive(Serialize)]
struct JsonSegment<'a> {
    text: &'a str,
    hit: bool,
}

pub fn json_text(state: State, result: &SearchResult) -> Result<String, serde_json::Error> {
    serde_json::to_string(&json_output(state, result)).map(|text| escape_unsafe(&text))
}

pub fn json_output(state: State, result: &SearchResult) -> JsonOutput<'_> {
    JsonOutput {
        state: state.name(),
        matched_all: result.matched_all,
        matched_some: result.matched_some,
        results: result.results.iter().map(json_hit).collect(),
    }
}

/// The JSON `folder_state` and the place the human output shows.
fn folder_state(hit: &Hit) -> (&'static str, &str) {
    if hit.folder_gone { ("gone", "folder gone") } else { ("ok", &hit.folder_name) }
}

fn json_hit(hit: &Hit) -> JsonHit<'_> {
    JsonHit {
        agent: &hit.agent,
        session_id: &hit.session_id,
        title: &hit.title,
        last_active: hit.last_active,
        folder: &hit.folder,
        folder_state: folder_state(hit).0,
        folder_name: &hit.folder_name,
        branch: hit.branch.as_deref(),
        keys: &hit.keys,
        prompts: hit.prompts,
        desk_job: hit.desk_job,
        matched: JsonMatch {
            role: &hit.match_role,
            segments: hit
                .segments
                .iter()
                .map(|s| JsonSegment { text: &s.text, hit: s.hit })
                .collect(),
        },
    }
}

pub fn human_search(state: State, result: &SearchResult, now: i64) -> String {
    match state {
        State::NoIndex => return "no index yet: run threads update\n".to_string(),
        State::Building => return "index is building\n".to_string(),
        State::Ok => {}
    }
    if result.results.is_empty() {
        return "no thread matches\n".to_string();
    }
    let blocks: Vec<String> = result.results.iter().map(|h| human_hit(h, now)).collect();
    format!(
        "{} all words, {} some\n\n{}",
        count(result.matched_all, "thread matches", "threads match"),
        count(result.matched_some, "matches", "match"),
        blocks.join("\n")
    )
}

fn count(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Indexed chat text is untrusted for a terminal: it can quote escape sequences from a web page.
/// Control characters and bidi overrides would be interpreted or reorder the line, so they print
/// as U+FFFD. Line and paragraph separators break the line, so they are unsafe too.
fn is_unsafe(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{2028}' | '\u{2029}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        )
}

/// Characters that only separate text print as one space so a title such as "Fix<TAB>grid" stays
/// readable; any other unsafe character becomes U+FFFD.
pub fn display_safe(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\t' | '\n' | '\r' | '\u{2028}' | '\u{2029}' => ' ',
            c if is_unsafe(c) => '\u{FFFD}',
            c => c,
        })
        .collect()
}

/// An unsafe character can occur only inside a JSON string (serde_json escapes none of them), so
/// replacing it by its `\uXXXX` escape keeps the text valid JSON and the value unchanged.
fn escape_unsafe(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if is_unsafe(c) {
            debug_assert!(u32::from(c) <= 0xFFFF, "unsafe set is BMP-only");
            out.push_str(&format!("\\u{:04x}", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    out
}

fn human_hit(hit: &Hit, now: i64) -> String {
    let mut out = format!("{}\n", display_safe(&hit.title));
    let mut facts = vec![age(now - hit.last_active), display_safe(folder_state(hit).1)];
    if let Some(branch) = &hit.branch {
        facts.push(display_safe(branch));
    }
    facts.push(count(hit.prompts, "prompt", "prompts"));
    out.push_str(&facts.join(" · "));
    out.push('\n');
    if !hit.keys.is_empty() {
        let keys: Vec<String> = hit.keys.iter().map(|k| display_safe(k)).collect();
        out.push_str(&format!("keys: {}\n", keys.join(" ")));
    }
    let snippet: String = hit
        .segments
        .iter()
        .map(|s| if s.hit { format!("[{}]", display_safe(&s.text)) } else { display_safe(&s.text) })
        .collect();
    out.push_str(&format!("{}: {}\n", display_safe(&hit.match_role), snippet));
    out.push_str(&resume_line(&hit.folder, &hit.session_id));
    out
}

/// Quotes do not stop a word that starts with `-` from reading as an option, so such a folder
/// follows `--` and such a session id is joined to `--resume` with `=`.
fn resume_line(folder: &str, session_id: &str) -> String {
    let cd = if folder.starts_with('-') { "cd -- " } else { "cd " };
    let resume = if session_id.starts_with('-') { "--resume=" } else { "--resume " };
    format!("{cd}{} && claude {resume}{}\n", shell_quote(folder), shell_quote(session_id))
}

fn age(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < SECONDS_PER_HOUR {
        format!("{}m", (seconds / SECONDS_PER_MINUTE).max(1))
    } else if seconds < SECONDS_PER_DAY {
        format!("{}h", seconds / SECONDS_PER_HOUR)
    } else {
        format!("{}d", seconds / SECONDS_PER_DAY)
    }
}

/// Pasted into a shell, so anything beyond plain path characters needs quoting. The session id
/// comes from a file name, which may hold any of them. A leading `~` (tilde expansion) or `=`
/// (zsh command expansion) changes the word even among plain characters. A word with a control or
/// bidi character uses ANSI-C quoting (zsh and bash 3.2) so the printed line carries no raw
/// terminal escape.
fn shell_quote(word: &str) -> String {
    if word.chars().any(is_unsafe) {
        return ansi_c_quote(word);
    }
    let plain = !word.is_empty()
        && !word.starts_with(['~', '=', '-'])
        && word.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+=,:@%~".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

fn ansi_c_quote(word: &str) -> String {
    let mut out = String::from("$'");
    for c in word.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '\'' => out.push_str(r"\'"),
            '!' => out.push_str(r"\x21"),
            c if is_unsafe(c) => {
                let mut buf = [0u8; 4];
                for byte in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("\\x{byte:02x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

pub fn update_line(report: &UpdateReport) -> String {
    if report.busy {
        return "busy: another update runs".to_string();
    }
    format!(
        "scanned {}, unchanged {}, appended {}, reindexed {}, removed {}, scripted {}",
        report.scanned, report.unchanged, report.appended, report.reindexed, report.removed, report.scripted
    )
}

pub fn stats_text(threads: i64, messages: i64, index_bytes: u64, last_update: Option<i64>) -> String {
    let last = last_update.map_or_else(|| "never".to_string(), |t| format!("{} UTC", utc(t)));
    format!(
        "threads: {threads}\nmessages: {messages}\nindex size: {:.1} MB\nlast update: {last}\n",
        index_bytes as f64 / 1_048_576.0
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snippet::Segment;

    #[test]
    fn ages_round_down_with_a_one_minute_floor() {
        assert_eq!(age(5), "1m");
        assert_eq!(age(59 * 60 + 59), "59m");
        assert_eq!(age(3600), "1h");
        assert_eq!(age(86_399), "23h");
        assert_eq!(age(86_400 * 3), "3d");
        assert_eq!(age(-10), "1m");
    }

    #[test]
    fn folders_with_special_characters_are_quoted() {
        assert_eq!(shell_quote("/a/widgets"), "/a/widgets");
        assert_eq!(shell_quote("/a/my widgets"), "'/a/my widgets'");
        assert_eq!(shell_quote("/a/it's"), r"'/a/it'\''s'");
        assert_eq!(shell_quote("/a/$HOME"), "'/a/$HOME'");
        assert_eq!(shell_quote("~x"), "'~x'");
        assert_eq!(shell_quote("=x"), "'=x'");
        assert_eq!(shell_quote("-x"), "'-x'");
        assert_eq!(shell_quote("a~b=c-d"), "a~b=c-d");
    }

    fn hit(session_id: &str, folder: &str, prompts: u32) -> Hit {
        Hit {
            agent: "claude".into(),
            session_id: session_id.into(),
            title: "demo".into(),
            last_active: 0,
            folder: folder.into(),
            folder_gone: false,
            folder_name: "widgets".into(),
            branch: None,
            keys: Vec::new(),
            prompts,
            desk_job: false,
            match_role: "user".into(),
            segments: Vec::new(),
        }
    }

    #[test]
    fn resume_line_quotes_the_session_id() {
        let text = human_hit(&hit("x; echo x", "/a/widgets", 1), 60);
        assert!(text.ends_with("cd /a/widgets && claude --resume 'x; echo x'\n"), "{text}");
    }

    #[test]
    fn resume_line_keeps_a_leading_dash_a_value() {
        let text = human_hit(&hit("-rf", "-w", 1), 60);
        assert!(text.ends_with("cd -- '-w' && claude --resume='-rf'\n"), "{text}");
    }

    const HOSTILE: &str = "a\x1b[31m\x07\x7f\u{9b}\u{61c}\u{200e}\u{202e}\u{2066}\u{2028}b";
    const HOSTILE_CHARS: [char; 9] =
        ['\x1b', '\x07', '\x7f', '\u{9b}', '\u{61c}', '\u{200e}', '\u{202e}', '\u{2066}', '\u{2028}'];

    #[test]
    fn json_text_escapes_unsafe_characters_and_stays_lossless() {
        let mut h = hit("s", "/a/widgets", 1);
        h.title = "a\u{9b}b\u{202e}c\x7fd\u{61c}e\u{2028}f\x1bg".into();
        let result = SearchResult { matched_all: 1, matched_some: 1, results: vec![h.clone()] };
        let text = json_text(State::Ok, &result).unwrap();
        let bad: Vec<char> = text.chars().filter(|&c| is_unsafe(c)).collect();
        assert!(bad.is_empty(), "{bad:?} in {text:?}");
        assert!(text.contains("a\\u009bb\\u202ec\\u007fd\\u061ce\\u2028f\\u001bg"), "{text}");
        let back: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(back["results"][0]["title"], h.title.as_str());
    }

    #[test]
    fn whitespace_controls_print_as_one_space_in_display_fields() {
        assert_eq!(display_safe("Fix\tgrid"), "Fix grid");
        assert_eq!(display_safe("a\r\nb\u{2028}c\u{2029}d"), "a  b c d");
        assert_eq!(display_safe("a\x1bb"), "a\u{FFFD}b");
    }

    #[test]
    fn display_fields_hold_no_terminal_control_characters() {
        let mut h = hit("s", "/a/widgets", 1);
        h.title = HOSTILE.into();
        h.folder_name = HOSTILE.into();
        h.branch = Some(HOSTILE.into());
        h.keys = vec![HOSTILE.into()];
        h.match_role = HOSTILE.into();
        h.segments = vec![
            Segment { text: HOSTILE.into(), hit: false },
            Segment { text: HOSTILE.into(), hit: true },
        ];
        let result = SearchResult { matched_all: 1, matched_some: 1, results: vec![h] };
        let text = human_search(State::Ok, &result, 60);
        let bad: Vec<char> = text.chars().filter(|c| HOSTILE_CHARS.contains(c)).collect();
        assert!(bad.is_empty(), "{bad:?} in {text:?}");
        let cleaned = format!("a\u{FFFD}[31m{} b", "\u{FFFD}".repeat(7));
        assert!(text.contains(&format!("{cleaned}\n")), "{text:?}");
        assert!(text.contains(&format!("[{cleaned}]")), "{text:?}");
    }

    #[test]
    fn a_tab_in_a_title_prints_as_a_space() {
        let mut h = hit("s", "/a/widgets", 1);
        h.title = "Fix\tgrid".into();
        let text = human_hit(&h, 60);
        assert!(text.starts_with("Fix grid\n"), "{text:?}");
    }

    #[test]
    fn resume_line_escapes_control_characters_in_the_folder() {
        let text = human_hit(&hit("s", "/a/\x1bx", 1), 60);
        assert!(text.ends_with("cd $'/a/\\x1bx' && claude --resume s\n"), "{text:?}");
        assert!(!text.contains('\x1b'), "{text:?}");
    }

    #[test]
    fn ansi_c_quoting_escapes_backslash_quote_and_multibyte_characters() {
        assert_eq!(shell_quote("a\\b'c\u{9b}\u{202e}"), "$'a\\\\b\\'c\\xc2\\x9b\\xe2\\x80\\xae'");
        assert_eq!(shell_quote("-\x07"), "$'-\\x07'");
        assert_eq!(shell_quote("a!b\x07"), "$'a\\x21b\\x07'");
    }

    #[test]
    fn counts_of_one_are_singular() {
        let one = SearchResult { matched_all: 1, matched_some: 1, results: vec![hit("s", "/a/widgets", 1)] };
        let text = human_search(State::Ok, &one, 60);
        assert!(text.starts_with("1 thread matches all words, 1 matches some\n"), "{text}");
        assert!(text.contains(" · 1 prompt\n"), "{text}");
        let two = SearchResult { matched_all: 2, matched_some: 0, results: vec![hit("s", "/a/widgets", 2)] };
        let text = human_search(State::Ok, &two, 60);
        assert!(text.starts_with("2 threads match all words, 0 match some\n"), "{text}");
        assert!(text.contains(" · 2 prompts\n"), "{text}");
    }

    #[test]
    fn stats_without_an_update_say_never() {
        let text = stats_text(0, 0, 0, None);
        assert!(text.contains("last update: never\n"), "{text}");
        assert!(text.contains("threads: 0\n"), "{text}");
    }
}
