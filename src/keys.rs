#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrKey {
    pub number: u64,
    pub repo: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyQuery {
    Ticket(String),
    Pr(u64),
    RepoPr { repo: String, number: u64 },
}

/// Standards and product names that look like `PREFIX-123` but are not tickets.
const DENIED_PREFIXES: &[&str] = &[
    "UTF", "SHA", "ISO", "GPT", "RFC", "AES", "MD", "TLS", "SSL", "HTTP", "HTTPS", "CVE", "ES",
    "UTC", "GMT", "WCAG", "BASE", "RSA", "HMAC", "PKCS", "IEEE", "CWE", "BCP", "OWASP", "CLAUDE",
    "OPUS", "SONNET", "HAIKU", "FABLE", "ARM", "AMD", "PY", "TS", "CP", "EUC", "COVID", "ECMA",
    "ANSI", "ASCII", "IPV", "OAUTH", "WIN", "MAC", "IOS", "NET", "CSS", "HTML", "X86", "M1", "RP",
];

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_repo_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')
}

fn walk_back(bytes: &[u8], mut end: usize, keep: fn(u8) -> bool) -> usize {
    while end > 0 && keep(bytes[end - 1]) {
        end -= 1;
    }
    end
}

fn walk_forward(bytes: &[u8], mut start: usize, keep: fn(u8) -> bool) -> usize {
    while start < bytes.len() && keep(bytes[start]) {
        start += 1;
    }
    start
}

fn is_ticket_prefix(prefix: &[u8]) -> bool {
    prefix.len() >= 2
        && prefix[0].is_ascii_uppercase()
        && prefix.iter().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        && !DENIED_PREFIXES.iter().any(|d| d.as_bytes() == prefix)
}

/// `str::parse` alone would also take a leading `+`.
fn all_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

fn parse_digits(digits: &str) -> Option<u64> {
    if !all_digits(digits) {
        return None;
    }
    digits.parse().ok()
}

pub(crate) fn push_new<T: PartialEq>(list: &mut Vec<T>, value: T) {
    if !list.contains(&value) {
        list.push(value);
    }
}

pub fn ticket_keys(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut keys: Vec<String> = Vec::new();
    for dash in (0..bytes.len()).filter(|&i| bytes[i] == b'-') {
        let start = walk_back(bytes, dash, is_word_byte);
        let end = walk_forward(bytes, dash + 1, |b| b.is_ascii_digit());
        let trailing_word = bytes.get(end).is_some_and(|&b| is_word_byte(b));
        if end == dash + 1 || trailing_word || !is_ticket_prefix(&bytes[start..dash]) {
            continue;
        }
        push_new(&mut keys, text[start..end].to_string());
    }
    keys
}

/// Upper-case keys by the `ticket_keys` rule, plus words of the narrower shape `ab-123` (2 to 6
/// letters in any case, 3 or more digits), stored in upper case. A branch is lower case by habit,
/// so `word-N` is common text there (`add-2-columns`, `release-2024`), not a ticket.
pub fn branch_ticket_keys(branch: &str) -> Vec<String> {
    let mut keys = ticket_keys(branch);
    let bytes = branch.as_bytes();
    for dash in (0..bytes.len()).filter(|&i| bytes[i] == b'-') {
        let start = walk_back(bytes, dash, |b| b.is_ascii_alphanumeric());
        let end = walk_forward(bytes, dash + 1, |b| b.is_ascii_digit());
        let letters = &bytes[start..dash];
        let shaped = (2..=6).contains(&letters.len())
            && letters.iter().all(u8::is_ascii_alphabetic)
            && end - (dash + 1) >= 3
            && !bytes.get(end).is_some_and(u8::is_ascii_alphanumeric);
        if !shaped {
            continue;
        }
        let key = branch[start..end].to_ascii_uppercase();
        if is_ticket_prefix(&key.as_bytes()[..letters.len()]) {
            push_new(&mut keys, key);
        }
    }
    keys
}

/// Reads the digits at `start`; a digit run glued to letters (`#12abc`, a hex colour) is not a number.
fn number_at(text: &str, start: usize) -> Option<u64> {
    let bytes = text.as_bytes();
    let end = walk_forward(bytes, start, |b| b.is_ascii_digit());
    if bytes.get(end).is_some_and(|&b| is_word_byte(b)) {
        return None;
    }
    parse_digits(&text[start..end])
}

/// The lower-case `owner/repo` that ends at `end`, if the text there has that shape.
fn repo_before(text: &str, end: usize) -> Option<String> {
    let bytes = text.as_bytes();
    let repo_start = walk_back(bytes, end, is_repo_byte);
    if repo_start == end || repo_start == 0 || bytes[repo_start - 1] != b'/' {
        return None;
    }
    let owner_end = repo_start - 1;
    let owner_start = walk_back(bytes, owner_end, is_repo_byte);
    if owner_start == owner_end {
        return None;
    }
    Some(text[owner_start..end].to_ascii_lowercase())
}

pub fn pr_keys(text: &str) -> Vec<PrKey> {
    let bytes = text.as_bytes();
    let mut found: Vec<PrKey> = Vec::new();
    for i in 0..bytes.len() {
        let key = match bytes[i] {
            b'#' => hash_pr(text, i),
            b'P' if bytes[i..].starts_with(b"PR ") && (i == 0 || !is_word_byte(bytes[i - 1])) => {
                number_at(text, i + 3).map(|number| PrKey { number, repo: None })
            }
            b'/' if bytes[i..].starts_with(b"/pull/") => {
                let repo = repo_before(text, i);
                number_at(text, i + 6).zip(repo).map(|(number, repo)| PrKey {
                    number,
                    repo: Some(repo),
                })
            }
            _ => None,
        };
        if let Some(key) = key {
            push_new(&mut found, key);
        }
    }
    found
}

/// The word `PR` in any case, which is never a repo name before a `#`.
fn is_pr_word(word: &str) -> bool {
    word.eq_ignore_ascii_case("pr")
}

/// `owner/repo#N`, `repo#N`, `PR#N`, or a bare `#N`. A `#N` after `&` is an HTML entity.
/// `PR#N` names no repo, so `PR` in any case is never stored as one; as with `PR N`, only the
/// upper-case word gives a key.
fn hash_pr(text: &str, hash: usize) -> Option<PrKey> {
    let number = number_at(text, hash + 1)?;
    if let Some(repo) = repo_before(text, hash) {
        return Some(PrKey { number, repo: Some(repo) });
    }
    let bytes = text.as_bytes();
    let start = walk_back(bytes, hash, is_repo_byte);
    let name = &text[start..hash];
    if name.is_empty() {
        let entity = hash > 0 && bytes[hash - 1] == b'&';
        return (!entity).then_some(PrKey { number, repo: None });
    }
    if name == "PR" {
        return Some(PrKey { number, repo: None });
    }
    (!is_pr_word(name)).then(|| PrKey { number, repo: Some(name.to_ascii_lowercase()) })
}

/// `PR#N` in any case is a plain PR number, because the index stores `PR#N` with no repo.
pub fn key_query(word: &str) -> Option<KeyQuery> {
    if let Some((left, right)) = word.split_once('#') {
        let number = parse_digits(right)?;
        if left.is_empty() || is_pr_word(left) {
            return Some(KeyQuery::Pr(number));
        }
        let valid_repo = left.bytes().all(|b| is_repo_byte(b) || b == b'/')
            && left.matches('/').count() <= 1
            && !left.starts_with('/')
            && !left.ends_with('/');
        return valid_repo.then(|| KeyQuery::RepoPr {
            repo: left.to_ascii_lowercase(),
            number,
        });
    }
    if word.len() >= 2
        && let Some(number) = parse_digits(word)
    {
        return Some(KeyQuery::Pr(number));
    }
    let upper = word.to_ascii_uppercase();
    let (prefix, digits) = upper.split_once('-')?;
    let is_ticket = is_ticket_prefix(prefix.as_bytes()) && all_digits(digits);
    is_ticket.then_some(KeyQuery::Ticket(upper))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u64, repo: Option<&str>) -> PrKey {
        PrKey { number, repo: repo.map(String::from) }
    }

    #[test]
    fn ticket_keys_dedupe_in_order_and_skip_denied_prefixes() {
        assert_eq!(
            ticket_keys("fix DEMO-101, then DEMO-7 and DEMO-101; see UTF-8, SHA-256"),
            ["DEMO-101", "DEMO-7"]
        );
    }

    #[test]
    fn ticket_keys_are_case_sensitive() {
        assert_eq!(ticket_keys("demo-101"), Vec::<String>::new());
    }

    #[test]
    fn branch_keys_take_upper_case_keys_and_lower_case_ticket_shapes() {
        for (branch, expected) in [
            ("dev/demo-202-x", &["DEMO-202"][..]),
            ("dev/abc-4812-fix", &["ABC-4812"]),
            ("feat/fix-abc-4812", &["ABC-4812"]),
            ("feat/ABC-12", &["ABC-12"]),
            ("demo-101", &["DEMO-101"]),
            ("dev/Abc-4812-fix", &["ABC-4812"]),
            ("ABC-4812_x", &["ABC-4812"]),
            ("abc-4812_x", &["ABC-4812"]),
            ("feat/DEMO-101-demo-101", &["DEMO-101"]),
        ] {
            assert_eq!(branch_ticket_keys(branch), expected, "{branch}");
        }
    }

    #[test]
    fn branch_words_outside_the_ticket_shape_give_no_key() {
        for branch in [
            "feat/add-2-columns",
            "dependabot/bump-lodash-4",
            "release-2024",
            "v2-123",
            "fix/demo-12",
            "x-123",
            "abcdefg-123",
            "fix/demo-123abc",
            "fix/xdemo1-123",
            "bump/sha-256",
        ] {
            assert_eq!(branch_ticket_keys(branch), Vec::<String>::new(), "{branch}");
        }
    }

    #[test]
    fn ticket_keys_need_word_boundaries() {
        assert_eq!(ticket_keys("xDEMO-101 DEMO-101abc"), Vec::<String>::new());
    }

    #[test]
    fn pr_keys_cover_every_form() {
        let prs = pr_keys(
            "see #1268 and PR 77, https://github.com/Acme/Widgets/pull/9, acme/widgets#12, color #fff",
        );
        assert_eq!(
            prs,
            [
                pr(1268, None),
                pr(77, None),
                pr(9, Some("acme/widgets")),
                pr(12, Some("acme/widgets")),
            ]
        );
    }

    #[test]
    fn pr_keys_dedupe_and_reject_non_pr_hashes() {
        assert_eq!(pr_keys("#5 #5 &#123; #12abc"), [pr(5, None)]);
    }

    #[test]
    fn pr_keys_take_a_bare_repo_and_pr_glued_to_the_hash() {
        assert_eq!(pr_keys("work orca#7,PR#3 +1"), [pr(7, Some("orca")), pr(3, None)]);
        assert_eq!(pr_keys("review Widgets#194"), [pr(194, Some("widgets"))]);
        assert_eq!(pr_keys("acme/widgets#12"), [pr(12, Some("acme/widgets"))]);
        assert_eq!(pr_keys("PR #5 and PR 6"), [pr(5, None), pr(6, None)]);
        assert_eq!(pr_keys("&#35; and #fff"), []);
    }

    #[test]
    fn pr_in_another_case_is_neither_a_repo_nor_a_pr() {
        assert_eq!(pr_keys("pr#4 Pr#5"), []);
    }

    #[test]
    fn key_query_ticket() {
        assert_eq!(key_query("demo-101"), Some(KeyQuery::Ticket("DEMO-101".into())));
        assert_eq!(key_query("utf-8"), None);
        assert_eq!(key_query("retry"), None);
    }

    #[test]
    fn key_query_pr_numbers() {
        assert_eq!(key_query("#1268"), Some(KeyQuery::Pr(1268)));
        assert_eq!(key_query("1268"), Some(KeyQuery::Pr(1268)));
        assert_eq!(key_query("7"), None);
    }

    #[test]
    fn key_query_pr_hash_in_any_case_is_a_plain_pr() {
        for word in ["PR#42", "pr#42", "Pr#42"] {
            assert_eq!(key_query(word), Some(KeyQuery::Pr(42)), "{word}");
        }
    }

    #[test]
    fn key_query_repo_pr() {
        assert_eq!(
            key_query("Widgets#12"),
            Some(KeyQuery::RepoPr { repo: "widgets".into(), number: 12 })
        );
        assert_eq!(
            key_query("acme/widgets#12"),
            Some(KeyQuery::RepoPr { repo: "acme/widgets".into(), number: 12 })
        );
    }
}
