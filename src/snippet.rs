use std::collections::HashMap;

use rusqlite::{Connection, ToSql};

use crate::store::MESSAGE_ROWID_SHIFT;

const SNIPPET_TOKENS: i64 = 18;
/// Stays far below SQLite's bound-parameter limit for any `limit`.
const SNIPPET_BATCH: usize = 500;
const ELLIPSIS: &str = "…";

// Indexed text arrives as Rust strings, so it is always valid UTF-8; these bytes never occur in
// valid UTF-8, which makes them unambiguous markers where any printable character could collide.
const HIT_OPEN: u8 = 0xFE;
const HIT_CLOSE: u8 = 0xFF;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub hit: bool,
}

/// The matched message's role and its marked text.
pub(crate) type Snippet = (String, Vec<Segment>);

/// One query per batch of threads: each `match` on a prefix of four or more letters (past the
/// prefix index) rebuilds that prefix's whole doclist, whatever the rowid range. The range bounds
/// the scan; `+` keeps the thread filter out of FTS5's plan, so `snippet()` runs only on rows of
/// `ids`. A thread's best row is the one whose snippet marks the most matches, newest on a tie.
/// `order by rank` is not used: `bm25()` counts each term's matching rows over the whole table on
/// every query, about 1.5 ms per common term.
pub(crate) fn best_snippets(
    conn: &Connection,
    expr: &str,
    ids: &[i64],
) -> rusqlite::Result<HashMap<i64, Snippet>> {
    let mut best: HashMap<i64, (usize, Snippet)> = HashMap::new();
    for chunk in ids.chunks(SNIPPET_BATCH) {
        collect_best(conn, expr, chunk, &mut best)?;
    }
    Ok(best.into_iter().map(|(id, (_, snippet))| (id, snippet)).collect())
}

/// The cast to blob keeps the marker bytes, which would fail a text read.
fn collect_best(
    conn: &Connection,
    expr: &str,
    ids: &[i64],
    best: &mut HashMap<i64, (usize, Snippet)>,
) -> rusqlite::Result<()> {
    let (Some(&low), Some(&high)) = (ids.iter().min(), ids.iter().max()) else {
        return Ok(());
    };
    let (first, last) = (low << MESSAGE_ROWID_SHIFT, ((high + 1) << MESSAGE_ROWID_SHIFT) - 1);
    let list: Vec<String> = (0..ids.len()).map(|i| format!("?{}", i + 9)).collect();
    let sql = format!(
        "select rowid, role, cast(snippet(messages, 0, ?1, ?2, ?3, ?4) as blob) from messages
         where messages match ?5 and rowid between ?6 and ?7 and (+rowid >> ?8) in ({})
         order by rowid",
        list.join(", ")
    );
    let (open, close): (&[u8], &[u8]) = (&[HIT_OPEN], &[HIT_CLOSE]);
    let mut values: Vec<&dyn ToSql> =
        vec![&open, &close, &ELLIPSIS, &SNIPPET_TOKENS, &expr, &first, &last, &MESSAGE_ROWID_SHIFT];
    values.extend(ids.iter().map(|id| id as &dyn ToSql));
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(values.as_slice())?;
    while let Some(row) = rows.next()? {
        let segments = segments(&row.get::<_, Vec<u8>>(2)?);
        let marked = segments.iter().filter(|s| s.hit).count();
        let id = row.get::<_, i64>(0)? >> MESSAGE_ROWID_SHIFT;
        if best.get(&id).is_none_or(|(most, _)| marked >= *most) {
            best.insert(id, (marked, (row.get(1)?, segments)));
        }
    }
    Ok(())
}

fn segments(marked: &[u8]) -> Vec<Segment> {
    let mut raw: Vec<(Vec<u8>, bool)> = Vec::new();
    let mut push = |bytes: &[u8], hit: bool| {
        if bytes.is_empty() {
            return;
        }
        match raw.last_mut() {
            Some((text, last_hit)) if *last_hit == hit => text.extend_from_slice(bytes),
            _ => raw.push((bytes.to_vec(), hit)),
        }
    };
    let mut start = 0;
    let mut hit = false;
    for (i, &b) in marked.iter().enumerate() {
        if b == HIT_OPEN || b == HIT_CLOSE {
            push(&marked[start..i], hit);
            hit = b == HIT_OPEN;
            start = i + 1;
        }
    }
    push(&marked[start..], hit);
    raw.into_iter()
        .map(|(bytes, hit)| Segment { text: squeeze_spaces(&String::from_utf8_lossy(&bytes)), hit })
        .collect()
}

/// Keeps one space at each edge, unlike `store::one_line_prefix`: the segments are joined, and an
/// edge space is what separates a hit from the words next to it.
fn squeeze_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_split_on_markers_merge_and_collapse_space() {
        let marked = [
            "…one  \n".as_bytes(),
            &[HIT_OPEN],
            b"retry",
            &[HIT_CLOSE, HIT_OPEN],
            b"queue",
            &[HIT_CLOSE],
            b"\tend",
        ]
        .concat();
        assert_eq!(
            segments(&marked),
            [
                Segment { text: "…one ".into(), hit: false },
                Segment { text: "retryqueue".into(), hit: true },
                Segment { text: " end".into(), hit: false },
            ]
        );
    }
}
