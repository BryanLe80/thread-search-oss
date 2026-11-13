use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, ToSql, params};

use crate::folder::{Dirs, FolderCache, FolderState};
use crate::keys::{KeyQuery, key_query, push_new};
pub use crate::snippet::Segment;
use crate::snippet::{Snippet, best_snippets};
use crate::store::{KEY_PR, KEY_TICKET, MESSAGE_ROWID_SHIFT, Store};
use crate::time::{NANOS_PER_SECOND, SECONDS_PER_DAY};

const TITLE_SCALE: f64 = 3.0;
const TITLE_BONUS: f64 = 4.0;
const KEY_BONUS: f64 = 12.0;
const ALL_IN_ONE_BONUS: f64 = 6.0;
const PHRASE_BONUS: f64 = 12.0;
const RECENCY_BOOST: f64 = 0.5;
const RECENCY_DAYS: f64 = 30.0;
/// A factor, not an added bonus, because the recency boost is a factor: an added bonus beats it
/// only for scores below twice the bonus. Above `1 + RECENCY_BOOST`, so at an equal score a thread
/// that holds a typed word whole ranks above any newer thread that holds only longer words.
const EXACT_WORD_FACTOR: f64 = 1.6;
const DESK_JOB_FACTOR: f64 = 0.6;
const K1: f64 = 1.2;
const NO_TITLE: &str = "(no title)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    pub text: String,
    pub quoted: bool,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub agent: String,
    pub session_id: String,
    pub title: String,
    pub last_active: i64,
    pub folder: String,
    pub folder_gone: bool,
    pub folder_name: String,
    pub branch: Option<String>,
    pub keys: Vec<String>,
    pub prompts: u32,
    pub desk_job: bool,
    /// `"user"`, `"assistant"`, or `"summary"`.
    pub match_role: String,
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone, Default)]
pub struct SearchResult {
    pub matched_all: u32,
    pub matched_some: u32,
    pub results: Vec<Hit>,
}

/// The words are joined first because a caller may pass the whole query as one argument.
pub fn parse_query(words: &[String]) -> Vec<Term> {
    let joined = words.join(" ");
    let mut terms = Vec::new();
    let mut rest = joined.as_str();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return terms;
        }
        if let Some(inner) = rest.strip_prefix('"') {
            let (group, after) = inner.split_once('"').unwrap_or((inner, ""));
            push_term(&mut terms, group.split_whitespace().collect::<Vec<_>>().join(" "), true);
            rest = after;
        } else {
            let end = rest.find(|c: char| c.is_whitespace() || c == '"').unwrap_or(rest.len());
            push_term(&mut terms, rest[..end].to_string(), false);
            rest = &rest[end..];
        }
    }
}

fn push_term(terms: &mut Vec<Term>, text: String, quoted: bool) {
    if text.chars().any(char::is_alphanumeric) {
        terms.push(Term { text, quoted });
    }
}

/// A quoted FTS5 string, so `-`, `:`, `AND`, and `(` in the query are text, not operators.
fn fts_string(text: &str, prefix: bool) -> String {
    let star = if prefix { "*" } else { "" };
    format!("\"{}\"{star}", text.replace('"', "\"\""))
}

/// A prefix of 1 or 2 letters matches most messages of the index, and each such word adds a full
/// read of that list to the search. The last word is exempt: it is the word being typed.
const MIN_PREFIX_LETTERS: usize = 3;

/// A longer query is most often pasted text, which holds whole words. An earlier prefix word of
/// four or more letters is past the prefix index, so each one merges the lists of every word it
/// starts, and a pasted sentence with a prefix for each word exceeds the 30 ms target.
const MAX_PREFIX_WORDS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Position {
    Last,
    Earlier,
    EarlierInLongQuery,
}

fn is_prefix(term: &Term, position: Position) -> bool {
    !term.quoted
        && match position {
            Position::Last => true,
            Position::Earlier => prefix_letters(&term.text).is_some_and(|n| n >= MIN_PREFIX_LETTERS),
            Position::EarlierInLongQuery => false,
        }
}

fn prefix_flags(terms: &[Term]) -> Vec<bool> {
    let earlier = if terms.len() > MAX_PREFIX_WORDS { Position::EarlierInLongQuery } else { Position::Earlier };
    let last = terms.len().saturating_sub(1);
    terms.iter().enumerate().map(|(i, t)| is_prefix(t, if i == last { Position::Last } else { earlier })).collect()
}

fn term_expressions(terms: &[Term], prefixes: &[bool]) -> Vec<String> {
    terms.iter().zip(prefixes).map(|(t, &prefix)| fts_string(&t.text, prefix)).collect()
}

/// The letters of the last part of a word, where FTS5 puts the prefix mark. `None` when that part
/// holds a character whose `unicode61` class the standard library cannot tell: outside ASCII and
/// U+00C0 to U+024F, `char::is_alphanumeric` also accepts marks and symbols that end a token, so
/// a short token could get a prefix. Such a word matches whole. A removed diacritic stays inside
/// the token but is no letter of it.
fn prefix_letters(text: &str) -> Option<usize> {
    let part = text.rsplit(|c: char| c.is_ascii() && !c.is_ascii_alphanumeric()).find(|p| !p.is_empty())?;
    let mut letters = 0;
    for c in part.chars() {
        if c.is_ascii_alphanumeric() || (('\u{C0}'..='\u{24F}').contains(&c) && c.is_alphabetic()) {
            letters += 1;
        } else if !is_removed_diacritic(c) {
            return None;
        }
    }
    Some(letters)
}

/// The combining marks that `unicode61` with `remove_diacritics` drops from a token (SQLite's
/// `sqlite3Fts5UnicodeIsdiacritic`). Every other combining mark ends the token.
fn is_removed_diacritic(c: char) -> bool {
    matches!(
        c,
        '\u{300}'..='\u{304}'
            | '\u{306}'..='\u{30C}'
            | '\u{30F}'
            | '\u{311}'
            | '\u{31B}'
            | '\u{323}'..='\u{328}'
            | '\u{32D}'..='\u{32E}'
            | '\u{330}'..='\u{331}'
    )
}

#[derive(Debug)]
struct Candidate {
    id: i64,
    matched: usize,
    score: f64,
    /// The last message timestamp, or the transcript's mtime when no message had a readable one.
    /// Ranking, the recency boost, and the shown time all read this one value, so a thread never
    /// shows a recent age while it sorts as the oldest.
    last_active: i64,
    desk_job: bool,
    session_id: String,
    title: String,
    stored_title: String,
    first_prompt: String,
}

/// Every statement runs in one deferred read transaction, so all of them read the same WAL
/// snapshot: an update that commits mid-search could otherwise remove a thread between the
/// candidate fetch and the hit, or give counts from two states. It takes no write lock.
pub fn search(
    store: &Store,
    terms: &[Term],
    limit: usize,
    now: i64,
    home: &Path,
    dirs: &dyn Dirs,
) -> rusqlite::Result<SearchResult> {
    let mut result = SearchResult::default();
    if terms.is_empty() {
        return Ok(result);
    }
    let snapshot = store.conn().unchecked_transaction()?;
    let conn: &Connection = &snapshot;
    let prefixes = prefix_flags(terms);
    let exprs = term_expressions(terms, &prefixes);
    let any_expr = exprs.join(" OR ");

    let threads = threads_with_messages(conn)?;
    let mut totals: HashMap<i64, (usize, f64)> = HashMap::new();
    let mut in_messages: HashSet<i64> = HashSet::new();
    // The messages that hold every term, kept from the per-term scans: a separate `AND` query
    // would decode each term's doclist a second time.
    let mut all_rows: Option<Vec<i64>> = None;
    let mut exact_words: HashMap<i64, i32> = HashMap::new();
    for ((term, expr), &prefix) in terms.iter().zip(&exprs).zip(&prefixes) {
        // A whole-word term that matches in a message, a title, or the keys is an exact word
        // already, so it needs no second scan; a quoted group gets no factor at all. The second
        // scan of a prefix term is a cost the rank accepts: it is in the measured search times.
        let whole_word = !prefix && !term.quoted;
        let mut exact = if prefix { exact_threads(conn, &fts_string(&term.text, false))? } else { HashSet::new() };
        let mut term_scores: HashMap<i64, f64> = HashMap::new();
        let rows = message_rows(conn, expr)?;
        let hits = thread_counts(&rows);
        let df = hits.len();
        for (id, count) in hits {
            *term_scores.entry(id).or_default() += message_score(threads, df, count);
            in_messages.insert(id);
            if whole_word {
                exact.insert(id);
            }
        }
        match &mut all_rows {
            Some(all) => keep_common(all, &rows),
            None => all_rows = Some(rows),
        }
        for (id, bm25, in_title) in title_matches(conn, expr)? {
            let bonus = if in_title { TITLE_BONUS } else { 0.0 };
            *term_scores.entry(id).or_default() += TITLE_SCALE * -bm25 + bonus;
            if whole_word && in_title {
                exact.insert(id);
            }
        }
        if let Some(query) = key_query(&term.text) {
            for id in key_matches(conn, &query)? {
                *term_scores.entry(id).or_default() += KEY_BONUS;
                if !term.quoted {
                    exact.insert(id);
                }
            }
        }
        for id in exact {
            *exact_words.entry(id).or_default() += 1;
        }
        for (id, score) in term_scores {
            let total = totals.entry(id).or_default();
            total.0 += 1;
            total.1 += score;
        }
    }

    let (phrase_expr, all_expr) = (phrase_expression(&exprs), exprs.join(" AND "));
    let mut in_phrase = HashSet::new();
    let mut in_all = HashSet::new();
    if terms.len() > 1 {
        in_all = thread_ids(&all_rows.unwrap_or_default());
        in_phrase = thread_ids(&message_rows(conn, &phrase_expr)?);
        for id in &in_all {
            totals.entry(*id).or_default().1 += ALL_IN_ONE_BONUS;
        }
        for id in &in_phrase {
            totals.entry(*id).or_default().1 += PHRASE_BONUS;
        }
    }

    let mut candidates = Vec::with_capacity(totals.len());
    for (id, total) in totals {
        let exact = exact_words.get(&id).copied().unwrap_or(0);
        if let Some(candidate) = candidate(conn, id, total, exact, now)? {
            candidates.push(candidate);
        }
    }
    let ranked = rank(candidates);    let matched_all = ranked.iter().filter(|c| c.matched == terms.len()).count();
    result.matched_all = matched_all as u32;
    result.matched_some = (ranked.len() - matched_all) as u32;

    let shown = &ranked[..ranked.len().min(limit)];
    let mut tiers: [(&str, Vec<i64>); 3] =
        [(&phrase_expr, Vec::new()), (&all_expr, Vec::new()), (&any_expr, Vec::new())];
    for c in shown {
        let tier = [&in_phrase, &in_all, &in_messages].iter().position(|set| set.contains(&c.id));
        if let Some(tier) = tier {
            tiers[tier].1.push(c.id);
        }
    }
    let mut snippets = HashMap::new();
    for (expr, ids) in &tiers {
        snippets.extend(best_snippets(conn, expr, ids)?);
    }
    let mut folders = FolderCache::new(home, dirs);
    for c in shown {
        if let Some(found) = hit(conn, c, snippets.remove(&c.id), &mut folders)? {
            result.results.push(found);
        }
    }
    Ok(result)
}

/// FTS5's `"a b"*` makes only the last token a prefix; `+` joins the terms into one phrase in
/// which each term keeps its own prefix mark.
fn phrase_expression(exprs: &[String]) -> String {
    exprs.join(" + ")
}

fn threads_with_messages(conn: &Connection) -> rusqlite::Result<usize> {
    let count: i64 = conn.query_row("select count(*) from threads where messages > 0", [], |r| r.get(0))?;
    Ok(count as usize)
}

/// Rowids only: reading `thread_id` or calling `bm25()` costs a content or size read per matched
/// row, and a common word matches most of the index. `order by rowid` is FTS5's own scan order,
/// so it adds no sort, and it lets the callers group and intersect the rows in one pass.
fn message_rows(conn: &Connection, expr: &str) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare_cached("select rowid from messages where messages match ?1 order by rowid")?;
    stmt.query_map([expr], |r| r.get(0))?.collect()
}

/// Each thread with the count of its rows in `rows`, which must be sorted: a thread's rows are
/// adjacent because the rowid holds the thread id in its high bits.
fn thread_counts(rows: &[i64]) -> Vec<(i64, u32)> {
    let mut counts: Vec<(i64, u32)> = Vec::new();
    for row in rows {
        let id = row >> MESSAGE_ROWID_SHIFT;
        match counts.last_mut() {
            Some((last, count)) if *last == id => *count += 1,
            _ => counts.push((id, 1)),
        }
    }
    counts
}

fn thread_ids(rows: &[i64]) -> HashSet<i64> {
    rows.iter().map(|row| row >> MESSAGE_ROWID_SHIFT).collect()
}

/// Both lists must be sorted.
fn keep_common(all: &mut Vec<i64>, rows: &[i64]) {
    let mut next = 0;
    all.retain(|row| {
        while next < rows.len() && rows[next] < *row {
            next += 1;
        }
        rows.get(next) == Some(row)
    });
}

/// BM25 with the thread as the document: `threads` threads hold messages, `df` of them match the
/// term, and `hits` of this thread's messages match it.
fn message_score(threads: usize, df: usize, hits: u32) -> f64 {
    let (n, df, hits) = (threads as f64, df as f64, f64::from(hits));
    let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
    idf * ((K1 + 1.0) * hits) / (hits + K1)
}

/// The threads that hold the whole word `exact` in a message or in the title; the caller adds
/// the threads whose keys match the word. The folder name and the branch do not count.
fn exact_threads(conn: &Connection, exact: &str) -> rusqlite::Result<HashSet<i64>> {
    let mut ids = thread_ids(&message_rows(conn, exact)?);
    ids.extend(user_title_ids(conn, exact)?);
    Ok(ids)
}

/// Desk-job titles are generated and repeat across jobs, so a desk job's title never counts as a
/// match in the title.
fn user_title_ids(conn: &Connection, expr: &str) -> rusqlite::Result<HashSet<i64>> {
    conn.prepare_cached(
        "select titles.rowid from titles join threads on threads.id = titles.rowid
         where titles match ?1 and threads.desk_job = 0",
    )?
    .query_map([format!("{{title}} : ({expr})")], |r| r.get(0))?
    .collect()
}

/// For a desk job only `place` may match (see `user_title_ids`). The flag says the term is in the
/// title itself: a match only in `place` gets no title bonus, because branch words such as `main`
/// or `fix` would then outrank a match in the chat.
fn title_matches(conn: &Connection, expr: &str) -> rusqlite::Result<Vec<(i64, f64, bool)>> {
    let mut stmt = conn.prepare_cached(
        "select titles.rowid, bm25(titles, 6.0, 1.0) from titles join threads on threads.id = titles.rowid
         where titles match ?1 and threads.desk_job = ?2",
    )?;
    let in_title = user_title_ids(conn, expr)?;
    let place_only = format!("{{place}} : ({expr})");
    let mut found: Vec<(i64, f64, bool)> = Vec::new();
    for (query, desk_job) in [(expr, false), (place_only.as_str(), true)] {
        for row in stmt.query_map(params![query, desk_job], |r| Ok((r.get(0)?, r.get(1)?)))? {
            let (id, bm25) = row?;
            found.push((id, bm25, in_title.contains(&id)));
        }
    }
    Ok(found)
}

fn key_matches(conn: &Connection, query: &KeyQuery) -> rusqlite::Result<Vec<i64>> {
    let ids = |sql: &str, values: &[&dyn ToSql]| -> rusqlite::Result<Vec<i64>> {
        conn.prepare_cached(sql)?.query_map(values, |r| r.get(0))?.collect()
    };
    let by_value = "select distinct thread_id from keys where kind = ?1 and value = ?2";
    match query {
        KeyQuery::Ticket(key) => ids(by_value, &[&KEY_TICKET, key]),
        KeyQuery::Pr(number) => ids(by_value, &[&KEY_PR, &number.to_string()]),
        // `like` would treat `_` in a repo name as a wildcard, so suffixes are compared with
        // `substr`. A bare `repo#N` names no owner, so it matches a query for any `owner/repo`.
        KeyQuery::RepoPr { repo, number } => ids(
            "select distinct thread_id from keys where kind = ?1 and value = ?2
               and (repo = ?3 or substr(repo, -length(?3) - 1) = '/' || ?3
                 or (repo <> '' and instr(repo, '/') = 0 and substr(?3, -length(repo) - 1) = '/' || repo))",
            &[&KEY_PR, &number.to_string(), repo],
        ),
    }
}

/// `None` for a thread with no message yet: an update keeps its row, but it has nothing to resume.
fn candidate(
    conn: &Connection,
    id: i64,
    (matched, raw): (usize, f64),
    exact_words: i32,
    now: i64,
) -> rusqlite::Result<Option<Candidate>> {
    let mut stmt = conn.prepare_cached(
        "select t.session_id, t.desk_job, t.title, t.first_prompt, t.last_ts,
           (select f.mtime_ns from files f where f.path = t.path)
         from threads t where t.id = ?1 and t.messages > 0",
    )?;
    stmt.query_row([id], |r| {
        let desk_job: bool = r.get(1)?;
        let last_ts: Option<i64> = r.get(4)?;
        let mtime_ns: Option<i64> = r.get(5)?;
        let stored_title: String = r.get(2)?;
        let first_prompt: String = r.get(3)?;
        let last_active = last_ts.or(mtime_ns.map(|ns| ns.div_euclid(NANOS_PER_SECOND)));
        Ok(Candidate {
            id,
            matched,
            score: adjusted_score(raw, exact_words, last_active, desk_job, now),
            last_active: last_active.unwrap_or(0),
            desk_job,
            session_id: r.get(0)?,
            title: display_title(&stored_title, &first_prompt),
            stored_title,
            first_prompt,
        })
    })
    .optional()
}

fn display_title(stored_title: &str, first_prompt: &str) -> String {
    if !stored_title.is_empty() {
        stored_title.to_string()
    } else if !first_prompt.is_empty() {
        first_prompt.to_string()
    } else {
        NO_TITLE.to_string()
    }
}

fn adjusted_score(raw: f64, exact_words: i32, last_active: Option<i64>, desk_job: bool, now: i64) -> f64 {
    let recency = last_active.map_or(0.0, |ts| {
        let age_days = (now - ts).max(0) as f64 / SECONDS_PER_DAY as f64;
        (-age_days / RECENCY_DAYS).exp()
    });
    let score = raw * EXACT_WORD_FACTOR.powi(exact_words) * (1.0 + RECENCY_BOOST * recency);
    if desk_job { score * DESK_JOB_FACTOR } else { score }
}

/// One session can have a file under two project folders (a resumed thread in another folder);
/// the newest file holds the latest state, whatever its score. Desk jobs that share a stored title
/// are runs of one job: the newest run is shown, at the place and with the counts of the group's
/// best-ranked run, so the list order and `matched_all` stay consistent. An empty stored title
/// names no job, so those desk jobs never collapse.
fn rank(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut by_session: HashMap<String, Candidate> = HashMap::new();
    for c in candidates {
        match by_session.entry(c.session_id.clone()) {
            Entry::Occupied(mut e) if (c.last_active, c.id) > (e.get().last_active, e.get().id) => {
                e.insert(c);
            }
            Entry::Occupied(_) => {}
            Entry::Vacant(e) => {
                e.insert(c);
            }
        }
    }
    let mut ranked: Vec<Candidate> = by_session.into_values().collect();
    ranked.sort_by(|a, b| {
        b.matched
            .cmp(&a.matched)
            .then(b.score.total_cmp(&a.score))
            .then(b.last_active.cmp(&a.last_active))
            .then(b.id.cmp(&a.id))
    });
    let in_group = |c: &Candidate| c.desk_job && !c.stored_title.is_empty();
    let mut newest: HashMap<String, usize> = HashMap::new();
    for (i, c) in ranked.iter().enumerate().filter(|(_, c)| in_group(c)) {
        let kept = newest.entry(c.stored_title.clone()).or_insert(i);
        if (c.last_active, c.id) > (ranked[*kept].last_active, ranked[*kept].id) {
            *kept = i;
        }
    }
    let mut slots: Vec<Option<Candidate>> = ranked.into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(slots.len());
    for i in 0..slots.len() {
        let Some(best) = slots[i].take() else { continue };
        if !in_group(&best) {
            out.push(best);
            continue;
        }
        match newest.remove(&best.stored_title) {
            Some(j) if j != i => {
                let mut shown = slots[j].take().expect("a group's newest member is taken only here");
                shown.matched = best.matched;
                shown.score = best.score;
                out.push(shown);
            }
            Some(_) => out.push(best),
            None => {}
        }
    }
    out
}

struct ThreadRow {
    agent: String,
    project_folder: String,
    last_cwd: Option<String>,
    branch: Option<String>,
    prompts: u32,
    folder: String,
    folder_gone: bool,
}

fn hit(
    conn: &Connection,
    c: &Candidate,
    snippet: Option<Snippet>,
    folders: &mut FolderCache,
) -> rusqlite::Result<Option<Hit>> {
    let mut stmt = conn.prepare_cached(
        "select agent, project_folder, last_cwd, branch, prompts, folder, folder_gone
         from threads where id = ?1",
    )?;
    let found = stmt.query_row([c.id], |r| {
        Ok(ThreadRow {
            agent: r.get(0)?,
            project_folder: r.get(1)?,
            last_cwd: r.get(2)?,
            branch: r.get(3)?,
            prompts: r.get(4)?,
            folder: r.get(5)?,
            folder_gone: r.get(6)?,
        })
    });
    // The search's snapshot keeps the row; this guards a read outside one.
    let Some(row) = found.optional()? else {
        return Ok(None);
    };

    // A gone thread stores its nearest existing parent, which is a directory, so only the flag
    // tells that the thread's own folder may have come back.
    let (folder, folder_gone) = if !row.folder_gone && folders.is_dir(Path::new(&row.folder)) {
        (row.folder, false)
    } else {
        let found = folders.resolve(&row.project_folder, row.last_cwd.as_deref());
        (found.path.to_string_lossy().into_owned(), found.state == FolderState::Gone)
    };
    let folder_name = Path::new(&folder)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let (match_role, segments) = snippet.unwrap_or_else(|| {
        let segments = if c.first_prompt.is_empty() {
            Vec::new()
        } else {
            vec![Segment { text: c.first_prompt.clone(), hit: false }]
        };
        ("user".to_string(), segments)
    });

    Ok(Some(Hit {
        agent: row.agent,
        session_id: c.session_id.clone(),
        title: c.title.clone(),
        last_active: c.last_active,
        folder,
        folder_gone,
        folder_name,
        branch: row.branch,
        keys: thread_keys(conn, c.id)?,
        prompts: row.prompts,
        desk_job: c.desk_job,
        match_role,
        segments,
    }))
}

fn thread_keys(conn: &Connection, id: i64) -> rusqlite::Result<Vec<String>> {
    let mut stmt =
        conn.prepare_cached("select kind, value from keys where thread_id = ?1 order by kind = ?2, rowid")?;
    let mut keys: Vec<String> = Vec::new();
    for row in stmt.query_map(params![id, KEY_PR], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (kind, value) = row?;
        let key = if kind == KEY_PR { format!("#{value}") } else { value };
        push_new(&mut keys, key);
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: i64, matched: usize, score: f64, last_ts: i64) -> Candidate {
        Candidate {
            id,
            matched,
            score,
            last_active: last_ts,
            desk_job: false,
            session_id: format!("s{id}"),
            title: format!("t{id}"),
            stored_title: String::new(),
            first_prompt: String::new(),
        }
    }

    fn ids(ranked: &[Candidate]) -> Vec<i64> {
        ranked.iter().map(|c| c.id).collect()
    }

    #[test]
    fn more_matched_terms_beat_a_higher_score() {
        let ranked = rank(vec![cand(1, 1, 100.0, 5), cand(2, 2, 1.0, 5)]);
        assert_eq!(ids(&ranked), [2, 1]);
    }

    #[test]
    fn equal_scores_order_by_last_active() {
        let ranked = rank(vec![cand(1, 1, 2.0, 5), cand(2, 1, 2.0, 9), cand(3, 1, 3.0, 1)]);
        assert_eq!(ids(&ranked), [3, 2, 1]);
    }

    #[test]
    fn same_session_keeps_the_newest_even_when_it_scores_lower() {
        let mut older = cand(1, 2, 50.0, 5);
        let mut newer = cand(2, 1, 1.0, 9);
        older.session_id = "s".into();
        newer.session_id = "s".into();
        assert_eq!(ids(&rank(vec![older, newer])), [2]);
    }

    #[test]
    fn desk_jobs_with_one_title_show_the_newest_at_the_best_place() {
        let mut jobs: Vec<Candidate> = (1..=3).map(|i| cand(i, 1, i as f64 * 10.0, 10 - i)).collect();
        for job in &mut jobs {
            job.desk_job = true;
            job.stored_title = "pr-review".into();
        }
        let mut own = cand(4, 1, 15.0, 5);
        own.stored_title = "pr-review".into();
        jobs.push(own);
        let ranked = rank(jobs);
        assert_eq!(ids(&ranked), [1, 4]);
        assert_eq!(ranked[0].score, 30.0);
    }

    #[test]
    fn hit_for_a_vanished_thread_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        drop(Store::open(&path).unwrap());
        let store = Store::open_existing(&path).unwrap().unwrap();
        let dirs = crate::folder::RealDirs;
        let mut folders = FolderCache::new(dir.path(), &dirs);
        let gone = hit(store.conn(), &cand(42, 1, 1.0, 5), None, &mut folders);
        assert!(gone.unwrap().is_none());
    }

    #[test]
    fn a_rare_term_outscores_a_common_term_with_many_hits() {
        assert!(message_score(1000, 5, 1) > message_score(1000, 900, 50));
        assert!(message_score(1000, 5, 3) > message_score(1000, 5, 1));
    }

    #[test]
    fn recency_and_desk_job_scale_the_score() {
        let now = 100 * SECONDS_PER_DAY;
        assert!((adjusted_score(2.0, 0, Some(now), false, now) - 3.0).abs() < 1e-9);
        assert!((adjusted_score(2.0, 0, None, false, now) - 2.0).abs() < 1e-9);
        assert!((adjusted_score(2.0, 0, Some(now), true, now) - 1.8).abs() < 1e-9);
        let month_old = adjusted_score(2.0, 0, Some(now - 30 * SECONDS_PER_DAY), false, now);
        assert!((month_old - 2.0 * (1.0 + 0.5 / std::f64::consts::E)).abs() < 1e-9);
    }

    #[test]
    fn one_exact_word_outweighs_the_largest_recency_boost() {
        let now = 100 * SECONDS_PER_DAY;
        let old_exact = adjusted_score(2.0, 1, None, false, now);
        let new_longer = adjusted_score(2.0, 0, Some(now), false, now);
        assert!(old_exact > new_longer);
        assert!((adjusted_score(2.0, 2, None, false, now) - 2.0 * EXACT_WORD_FACTOR.powi(2)).abs() < 1e-9);
    }

    #[test]
    fn fts_string_doubles_quotes_and_marks_prefix() {
        assert_eq!(fts_string("say \"hi\"", false), "\"say \"\"hi\"\"\"");
        assert_eq!(fts_string("webh", true), "\"webh\"*");
    }

    fn term(text: &str, quoted: bool) -> Term {
        Term { text: text.to_string(), quoted }
    }

    fn earlier(text: &str) -> bool {
        is_prefix(&term(text, false), Position::Earlier)
    }

    fn expressions(words: &[&str]) -> Vec<String> {
        let terms: Vec<Term> = words.iter().map(|w| term(w, false)).collect();
        term_expressions(&terms, &prefix_flags(&terms))
    }

    #[test]
    fn an_earlier_term_is_a_prefix_from_three_letters() {
        assert!(earlier("dep"));
        assert!(!earlier("de"));
        assert!(!is_prefix(&term("retry queue", true), Position::Earlier));
    }

    #[test]
    fn the_last_term_is_a_prefix_from_one_letter_unless_quoted() {
        assert!(is_prefix(&term("d", false), Position::Last));
        assert!(!is_prefix(&term("retry", true), Position::Last));
    }

    #[test]
    fn an_earlier_term_of_a_long_query_is_whole() {
        assert!(!is_prefix(&term("deplo", false), Position::EarlierInLongQuery));
        assert_eq!(expressions(&["dep", "run", "rig", "n"]), ["\"dep\"*", "\"run\"*", "\"rig\"*", "\"n\"*"]);
        assert_eq!(
            expressions(&["dep", "run", "the", "rig", "n"]),
            ["\"dep\"", "\"run\"", "\"the\"", "\"rig\"", "\"n\"*"]
        );
    }

    #[test]
    fn letters_count_in_the_last_part_of_a_word() {
        assert!(!earlier("abc-de"));
        assert!(earlier("de-abc"));
        assert!(earlier("zork.quixa"));
    }

    #[test]
    fn latin_letters_count_and_a_removed_diacritic_is_no_letter() {
        assert!(earlier("\u{E9}t\u{E9}"));
        assert!(!earlier("\u{E9}t"));
        assert!(earlier("e\u{301}te"));
        assert!(!earlier("e\u{301}t"));
        for mark in ['\u{300}', '\u{304}', '\u{306}', '\u{30C}', '\u{30F}', '\u{311}', '\u{31B}', '\u{323}', '\u{328}', '\u{32D}', '\u{330}', '\u{331}'] {
            assert!(earlier(&format!("ab{mark}c")), "{mark:?}");
        }
        assert!(!earlier("ab\u{305}c"));
        assert!(!earlier("ab\u{332}c"));
    }

    #[test]
    fn a_word_outside_latin_letters_is_whole() {
        assert!(!earlier("\u{915}\u{93F}\u{924}\u{93E}\u{92C}"));
        assert!(!earlier("домой"));
        assert!(!earlier("λόγος"));
        assert!(!earlier("ab\u{E000}"));
        assert!(!earlier("ab\u{20D0}c"));
        assert!(!earlier("abc\u{345}"));
        assert!(!earlier("ab\u{24B6}c"));
        assert!(!earlier("ab\u{D7}cd"));
    }

    #[test]
    fn rows_group_by_thread_and_keep_only_rows_in_both_lists() {
        let row = |thread: i64, seq: i64| (thread << MESSAGE_ROWID_SHIFT) | seq;
        let first = [row(1, 0), row(1, 4), row(2, 1), row(5, 0), row(5, 2), row(5, 3)];
        assert_eq!(thread_counts(&first), [(1, 2), (2, 1), (5, 3)]);
        let mut all = first.to_vec();
        keep_common(&mut all, &[row(1, 4), row(3, 0), row(5, 0), row(5, 3), row(6, 0)]);
        assert_eq!(all, [row(1, 4), row(5, 0), row(5, 3)]);
        keep_common(&mut all, &[]);
        assert!(all.is_empty());
    }

    #[test]
    fn phrase_keeps_the_prefix_mark_of_each_term() {
        let terms = [term("deplo", false), term("to", false), term("retry queue", true), term("n", false)];
        let exprs = term_expressions(&terms, &prefix_flags(&terms));
        assert_eq!(phrase_expression(&exprs), "\"deplo\"* + \"to\" + \"retry queue\" + \"n\"*");
    }
}
