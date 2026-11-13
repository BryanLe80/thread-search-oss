// Each test crate uses a different subset of the builder.
#![allow(dead_code)]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, types::FromSql};
use serde_json::{Map, Value, json};
use tempfile::TempDir;
use thread_search::folder::{RealDirs, slug};
use thread_search::search::{SearchResult, Term, parse_query, search};
use thread_search::store::Store;
use thread_search::update::{UpdateReport, rebuild, update};

/// 2026-11-10T12:00:00Z, the default record time.
pub const NOW: i64 = 1_794_312_000;
const DEFAULT_TS: &str = "2026-11-10T12:00:00.000Z";

pub fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

/// A typed prompt record with no timestamp, cwd, or session id, as one JSON line.
pub fn user_record(text: &str) -> String {
    json!({"type": "user", "message": {"content": text}, "entrypoint": "cli"}).to_string()
}

pub struct Fx {
    pub root: PathBuf,
    pub index: PathBuf,
    /// Canonical: folder resolution walks the real filesystem from `/`, and on macOS the
    /// temp folder sits behind the `/var` -> `/private/var` symlink.
    pub work: PathBuf,
    _dir: TempDir,
}

impl Fx {
    pub fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("projects");
        let work = base.join("work");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&work).unwrap();
        Fx { root, index: base.join("index").join("threads.db"), work, _dir: dir }
    }

    pub fn thread(&self, cwd_rel: &str, session: &str) -> ThreadFile {
        let cwd = self.work.join(cwd_rel);
        fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.to_str().unwrap().to_string();
        ThreadFile {
            path: self.root.join(slug(&cwd)).join(format!("{session}.jsonl")),
            session: session.to_string(),
            cwd,
            ts: DEFAULT_TS.to_string(),
            branch: "main".to_string(),
            entrypoint: "cli".to_string(),
            session_kind: None,
            queue: String::new(),
        }
    }

    pub fn update(&self) -> UpdateReport {
        self.update_root(&self.root)
    }

    pub fn update_root(&self, root: &Path) -> UpdateReport {
        update(root, &self.index, &self.work, &RealDirs, NOW).unwrap()
    }

    pub fn rebuild(&self) -> UpdateReport {
        rebuild(&self.root, &self.index, &self.work, &RealDirs, NOW).unwrap()
    }

    pub fn search(&self, words: &[&str]) -> SearchResult {
        self.search_limit(words, 20)
    }

    pub fn search_limit(&self, words: &[&str], limit: usize) -> SearchResult {
        self.search_terms(&parse_query(&strings(words)), limit)
    }

    pub fn search_terms(&self, terms: &[Term], limit: usize) -> SearchResult {
        let store = Store::open_existing(&self.index).unwrap().unwrap();
        search(&store, terms, limit, NOW, &self.work, &RealDirs).unwrap_or_else(|e| panic!("{terms:?}: {e}"))
    }

    fn reader(&self) -> Connection {
        Connection::open_with_flags(&self.index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
    }

    pub fn one<T: FromSql>(&self, sql: &str) -> T {
        self.reader().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    pub fn strings(&self, sql: &str) -> Vec<String> {
        let conn = self.reader();
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    }
}

pub struct ThreadFile {
    path: PathBuf,
    session: String,
    cwd: String,
    ts: String,
    branch: String,
    entrypoint: String,
    session_kind: Option<String>,
    queue: String,
}

impl ThreadFile {
    pub fn kind(mut self, entrypoint: &str, session_kind: Option<&str>) -> Self {
        self.entrypoint = entrypoint.to_string();
        self.session_kind = session_kind.map(String::from);
        self
    }

    pub fn desk_job(self) -> Self {
        self.kind("cli", Some("bg"))
    }

    pub fn scripted(self) -> Self {
        self.kind("sdk-cli", None)
    }

    pub fn at(mut self, iso: &str) -> Self {
        self.ts = iso.to_string();
        self
    }

    pub fn branch(mut self, name: &str) -> Self {
        self.branch = name.to_string();
        self
    }

    pub fn cwd(mut self, abs: &str) -> Self {
        self.cwd = abs.to_string();
        self
    }

    pub fn user(self, text: &str) -> Self {
        let record = self.record("user", json!({ "content": text }));
        self.push(record)
    }

    pub fn assistant(self, text: &str) -> Self {
        let record = self.record("assistant", json!({ "content": [{ "type": "text", "text": text }] }));
        self.push(record)
    }

    pub fn tool_result(self, text: &str) -> Self {
        let content = json!({ "content": [{ "type": "tool_result", "tool_use_id": "toolu_1", "content": text }] });
        let record = self.record("user", content);
        self.push(record)
    }

    pub fn summary(self, text: &str) -> Self {
        let mut record = self.record("user", json!({ "content": text }));
        record.insert("isCompactSummary".into(), json!(true));
        self.push(record)
    }

    pub fn custom_title(self, text: &str) -> Self {
        let record = json!({ "type": "custom-title", "customTitle": text, "sessionId": self.session });
        self.push_value(record)
    }

    pub fn ai_title(self, text: &str) -> Self {
        let record = json!({ "type": "ai-title", "aiTitle": text, "sessionId": self.session });
        self.push_value(record)
    }

    pub fn pr(self, number: u64, repo: &str) -> Self {
        let record = json!({
            "type": "pr-link",
            "prNumber": number,
            "prRepository": repo,
            "prUrl": format!("https://github.com/{repo}/pull/{number}"),
            "sessionId": self.session,
        });
        self.push_value(record)
    }

    pub fn raw(mut self, line: &str) -> Self {
        self.queue.push_str(line);
        self.queue.push('\n');
        self
    }

    pub fn raw_no_newline(mut self, bytes: &str) -> Self {
        self.queue.push_str(bytes);
        self
    }

    pub fn write(self) -> PathBuf {
        fs::create_dir_all(self.path.parent().unwrap()).unwrap();
        let mut file = OpenOptions::new().create(true).append(true).open(&self.path).unwrap();
        file.write_all(self.queue.as_bytes()).unwrap();
        self.path
    }

    fn record(&self, kind: &str, message: Value) -> Map<String, Value> {
        let mut record = Map::new();
        record.insert("type".into(), json!(kind));
        record.insert("message".into(), message);
        record.insert("timestamp".into(), json!(self.ts));
        record.insert("cwd".into(), json!(self.cwd));
        record.insert("gitBranch".into(), json!(self.branch));
        record.insert("entrypoint".into(), json!(self.entrypoint));
        record.insert("sessionId".into(), json!(self.session));
        if let Some(kind) = &self.session_kind {
            record.insert("sessionKind".into(), json!(kind));
        }
        record
    }

    fn push(self, record: Map<String, Value>) -> Self {
        self.push_value(Value::Object(record))
    }

    fn push_value(self, record: Value) -> Self {
        self.raw(&record.to_string())
    }
}
