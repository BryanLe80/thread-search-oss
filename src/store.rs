use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::keys::PrKey;
use crate::transcript::Role;

const SCHEMA_VERSION: u32 = 3;

/// Stored in the SQLite header (`PRAGMA application_id`): "THRS" in ASCII. A `meta` table alone
/// cannot mark a file as ours, because Chromium and Electron databases have one too.
const APPLICATION_ID: i32 = 0x5448_5253;

/// The `keys.kind` values.
pub const KEY_TICKET: &str = "ticket";
pub const KEY_PR: &str = "pr";

const FIRST_PROMPT_CHARS: usize = 240;

/// A message rowid is `(thread_id << MESSAGE_ROWID_SHIFT) | seq`, `seq` counting from 1, so a
/// search maps a match to its thread from the rowid alone: reading the unindexed `thread_id`
/// column costs a content-table read per matched row.
pub const MESSAGE_ROWID_SHIFT: u32 = 24;
const MAX_MESSAGE_SEQ: i64 = (1 << MESSAGE_ROWID_SHIFT) - 1;

// FTS5 cannot index the unindexed `thread_id` column, so a thread's rows are found by rowid:
// messages through `MESSAGE_ROWID_SHIFT`, and `titles` rows use the thread id as their rowid.
// `threads.messages` counts a thread's message rows and gives the next `seq`.
// `files.entry_seen` records that the file's first entrypoint was not `sdk*`; without it an
// append could not tell a file whose first entrypoint is still to come from a decided one.
// The prefix index keeps a one- to three-letter prefix (typed on every key press) from
// merging the doclists of every token it starts. No stemmer: porter also stems a prefix query,
// so `"deploy"*` became `deploi*` and missed the stored stem of "deployment".
const SCHEMA: &str = "
create table meta(key text primary key, value text);
create table files(path text primary key, size integer not null, mtime_ns integer not null,
  offset integer not null, check_value integer not null, scripted integer not null default 0,
  entry_seen integer not null default 0);
create table threads(id integer primary key, path text unique not null, agent text not null,
  session_id text not null, project_folder text not null, desk_job integer not null,
  title text not null default '', title_custom integer not null default 0,
  first_prompt text not null default '', last_cwd text, branch text,
  first_ts integer, last_ts integer, prompts integer not null default 0,
  messages integer not null default 0,
  folder text not null, folder_gone integer not null, summary text not null default '');
create table keys(thread_id integer not null, kind text not null,
  value text not null,
  repo text not null default '',
  unique(thread_id, kind, value, repo));
create virtual table messages using fts5(text, thread_id unindexed, role unindexed, ts unindexed,
  tokenize='unicode61', prefix='1 2 3');
create virtual table titles using fts5(title, place, thread_id unindexed,
  tokenize='unicode61');
";

#[derive(Debug, Clone)]
pub struct FileState {
    pub size: u64,
    pub mtime_ns: i64,
    pub offset: u64,
    pub check_value: u64,
    pub entry_seen: bool,
}

#[derive(Debug, Clone)]
pub struct NewMessage {
    pub role: Role,
    pub text: String,
    pub ts: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct ThreadChange {
    pub reset: bool,
    pub session_id: String,
    pub project_folder: String,
    pub desk_job: Option<bool>,
    pub messages: Vec<NewMessage>,
    pub title_custom: Option<String>,
    pub title_ai: Option<String>,
    pub last_cwd: Option<String>,
    pub branch: Option<String>,
    pub tickets: Vec<String>,
    pub prs: Vec<PrKey>,
    pub folder: String,
    pub folder_gone: bool,
}

pub struct Store {
    conn: Connection,
    // Held for the life of the writer: `open` and `open_fresh` are the only paths that delete or
    // rebuild the file.
    _lock: Option<fs::File>,
}

pub struct UpdateTx<'a> {
    tx: Transaction<'a>,
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Summary => "summary",
    }
}

fn has_table(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "select count(*) > 0 from sqlite_master where type = 'table' and name = ?1",
        [name],
        |r| r.get(0),
    )
}

fn schema_version(conn: &Connection) -> rusqlite::Result<Option<String>> {
    if !has_table(conn, "meta")? {
        return Ok(None);
    }
    conn.query_row("select value from meta where key = 'schema_version'", [], |r| r.get(0))
        .optional()
}

#[derive(Debug, PartialEq, Eq)]
enum Owner {
    Ours { current: bool },
    /// An index of ours built before the `APPLICATION_ID` stamp.
    Legacy,
    Empty,
    Foreign,
}

fn owner(conn: &Connection) -> rusqlite::Result<Owner> {
    let stamp: i32 = conn.query_row("pragma application_id", [], |r| r.get(0))?;
    if stamp == APPLICATION_ID {
        let current = schema_version(conn)? == Some(SCHEMA_VERSION.to_string());
        return Ok(Owner::Ours { current });
    }
    if stamp != 0 {
        return Ok(Owner::Foreign);
    }
    // Only versions 1 and 2 existed before the stamp, so any other unstamped value is another app's.
    let legacy_version = matches!(schema_version(conn)?.as_deref(), Some("1" | "2"));
    if legacy_version && has_table(conn, "threads")? {
        return Ok(Owner::Legacy);
    }
    let empty: bool = conn.query_row("select count(*) = 0 from sqlite_master", [], |r| r.get(0))?;
    Ok(if empty { Owner::Empty } else { Owner::Foreign })
}

/// Another writer holds the index lock, or SQLite reports the file busy or locked.
pub fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == ErrorCode::DatabaseBusy || f.code == ErrorCode::DatabaseLocked
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stats {
    pub threads: i64,
    pub messages: i64,
    pub last_update: Option<i64>,
}

fn is_unusable_file(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == ErrorCode::NotADatabase || f.code == ErrorCode::DatabaseCorrupt
    )
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn remove_index_files(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(sibling(path, suffix));
    }
}

/// A file SQLite cannot read may still be something the user wants back, so it is kept as
/// `<index>.bad` instead of unlinked; only one is kept, so it replaces an older `.bad`. Its `-wal`
/// and `-shm` would be read as the new index's.
fn set_aside(path: &Path) -> rusqlite::Result<()> {
    fs::rename(path, sibling(path, ".bad")).map_err(io_error)?;
    for suffix in ["-wal", "-shm"] {
        let _ = fs::remove_file(sibling(path, suffix));
    }
    Ok(())
}

fn not_ours(path: &Path) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
        Some(format!("{} is not a thread-search index; threads will not replace it", path.display())),
    )
}

/// Serializes writers across processes, so a delete-and-rebuild never races another `open`.
/// The lock file itself is never deleted: unlinking it would let two processes lock different inodes.
fn take_writer_lock(index_path: &Path) -> rusqlite::Result<fs::File> {
    let file = open_private(&sibling(index_path, ".lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("another process holds the index writer lock".to_string()),
        )),
        Err(fs::TryLockError::Error(e)) => Err(io_error(e)),
    }
}

/// Created with mode 0600, so the file never exists with a wider mode.
fn open_private(path: &Path) -> rusqlite::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(io_error)
}

enum Found {
    Ready(Connection),
    /// Ours, but of another schema version or built before the stamp.
    Stale,
    /// A non-empty database that is not ours: another app's data.
    Foreign,
}

fn create_and_check(path: &Path) -> rusqlite::Result<Found> {
    open_private(path)?;
    try_open(path)
}

fn try_open(path: &Path) -> rusqlite::Result<Found> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::ZERO)?;
    // The checks, the stamp, and the schema creation share one write lock: with separate reads, a
    // second first-time open could see an empty file before and an unstamped or half-built one
    // after the first one commits, and then delete a live index. The WAL switch comes after,
    // because it cannot run inside a transaction and two first opens racing on it could both fail
    // before either creates the schema.
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
    match owner(&tx)? {
        Owner::Ours { current: true } => {}
        Owner::Ours { current: false } | Owner::Legacy => return Ok(Found::Stale),
        Owner::Foreign => return Ok(Found::Foreign),
        Owner::Empty => {
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
            tx.execute_batch(SCHEMA)?;
            tx.execute(
                "insert into meta(key, value) values('schema_version', ?1)",
                [SCHEMA_VERSION.to_string()],
            )?;
        }
    }
    tx.commit()?;
    conn.pragma_update(None, "journal_mode", "wal")?;
    Ok(Found::Ready(conn))
}

/// Every check and every delete or rename runs under the writer lock, so a concurrent `open`
/// cannot start writing to a file that is about to be replaced. `fresh` also drops a current
/// index of ours, because `rebuild` wants every file read again.
fn open_writer(path: &Path, fresh: bool) -> rusqlite::Result<Store> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(io_error)?;
    }
    let lock = take_writer_lock(path)?;
    match create_and_check(path) {
        Ok(Found::Ready(conn)) if !fresh => return Ok(Store { conn, _lock: Some(lock) }),
        Ok(Found::Ready(conn)) => {
            drop(conn);
            remove_index_files(path);
        }
        Ok(Found::Stale) => remove_index_files(path),
        Ok(Found::Foreign) => return Err(not_ours(path)),
        Err(e) if is_unusable_file(&e) => set_aside(path)?,
        // Busy, permission, and I/O errors are transient: another process may be using
        // the file, so deleting it would destroy a live index.
        Err(e) => return Err(e),
    }
    match create_and_check(path)? {
        Found::Ready(conn) => Ok(Store { conn, _lock: Some(lock) }),
        // A freshly created file was still judged unusable: report it rather than loop.
        Found::Stale | Found::Foreign => Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
            Some("index file unusable after rebuild".to_string()),
        )),
    }
}

impl Store {
    /// The index is a cache. An index of ours with another schema version, or one built before the
    /// `APPLICATION_ID` stamp, is deleted with its `-wal` / `-shm` and rebuilt; a file SQLite
    /// reports as NOTADB or CORRUPT is renamed to `<index>.bad`; a database of another app is an
    /// error and is left as it is. Only one `Store`
    /// from `open` exists at a time: another holder of the sidecar `.lock` file makes this return
    /// a busy `Err`, and a busy or locked file is left untouched.
    pub fn open(path: &Path) -> rusqlite::Result<Store> {
        open_writer(path, false)
    }

    /// Like `open`, but an index of ours is deleted whatever its version.
    pub fn open_fresh(path: &Path) -> rusqlite::Result<Store> {
        open_writer(path, true)
    }

    /// `None` until the first update commits.
    pub fn last_update(&self) -> rusqlite::Result<Option<i64>> {
        let value: Option<String> = self
            .conn
            .query_row("select value from meta where key = 'last_update'", [], |r| r.get(0))
            .optional()?;
        Ok(value.map(|v| v.parse().unwrap_or(0)))
    }

    pub fn stats(&self) -> rusqlite::Result<Stats> {
        let (threads, messages) = self.conn.query_row(
            "select count(*) filter (where messages > 0), coalesce(sum(messages), 0) from threads",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(Stats { threads, messages, last_update: self.last_update()? })
    }

    pub fn open_existing(path: &Path) -> rusqlite::Result<Option<Store>> {
        if !path.exists() {
            return Ok(None);
        }
        open_read_only(path)
    }

    /// `None` when another writer holds the lock; waiting would stall a search that wants a refresh.
    pub fn begin_update(&mut self) -> rusqlite::Result<Option<UpdateTx<'_>>> {
        match self.conn.transaction_with_behavior(TransactionBehavior::Immediate) {
            Ok(tx) => Ok(Some(UpdateTx { tx })),
            Err(e) if is_busy(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

/// `rebuild` deletes the file before it creates the new one, so a file that passed the
/// existence check can be gone at the open: that is no index yet, not an error. A permission
/// error is CANTOPEN as well, so only a file that is gone counts as no index.
fn open_read_only(path: &Path) -> rusqlite::Result<Option<Store>> {
    let opened = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .and_then(|conn| owner(&conn).map(|o| (conn, o)));
    match opened {
        Ok((conn, Owner::Ours { current: true })) => Ok(Some(Store { conn, _lock: None })),
        Ok(_) => Ok(None),
        Err(e) if is_unusable_file(&e) => Ok(None),
        Err(e) if is_cannot_open(&e) && !path.exists() => Ok(None),
        Err(e) => Err(e),
    }
}

fn is_cannot_open(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::CannotOpen)
}

fn io_error(e: std::io::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
}

/// Trimmed and cut to `max_chars`, unlike `snippet::squeeze_spaces`: a prompt shown alone has no
/// neighbour to keep its edge spaces for.
fn one_line_prefix(text: &str, max_chars: usize) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

fn last_segment(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or("")
}

impl UpdateTx<'_> {
    pub fn file_state(&self, path: &str) -> rusqlite::Result<Option<(FileState, bool)>> {
        self.tx
            .query_row(
                "select size, mtime_ns, offset, check_value, scripted, entry_seen from files where path = ?1",
                [path],
                |r| {
                    let state = FileState {
                        size: r.get::<_, i64>(0)? as u64,
                        mtime_ns: r.get(1)?,
                        offset: r.get::<_, i64>(2)? as u64,
                        check_value: r.get::<_, i64>(3)? as u64,
                        entry_seen: r.get(5)?,
                    };
                    Ok((state, r.get::<_, i64>(4)? != 0))
                },
            )
            .optional()
    }

    pub fn last_cwd(&self, path: &str) -> rusqlite::Result<Option<String>> {
        let cwd: Option<Option<String>> = self
            .tx
            .query_row("select last_cwd from threads where path = ?1", [path], |r| r.get(0))
            .optional()?;
        Ok(cwd.flatten())
    }

    pub fn known_paths(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.tx.prepare("select path from files")?;
        stmt.query_map([], |r| r.get(0))?.collect()
    }

    pub fn apply(&mut self, path: &str, state: &FileState, change: &ThreadChange) -> rusqlite::Result<()> {
        if change.reset {
            self.delete_thread(path)?;
        }
        let tx = &self.tx;
        let id = match thread_id(tx, path)? {
            Some(id) => id,
            None => {
                tx.execute(
                    "insert into threads(path, agent, session_id, project_folder, desk_job, folder, folder_gone)
                     values(?1, 'claude', ?2, ?3, 0, ?4, ?5)",
                    params![path, change.session_id, change.project_folder, change.folder, change.folder_gone],
                )?;
                tx.last_insert_rowid()
            }
        };

        if !change.session_id.is_empty() {
            tx.execute("update threads set session_id = ?2 where id = ?1", params![id, change.session_id])?;
        }
        if !change.project_folder.is_empty() {
            tx.execute(
                "update threads set project_folder = ?2 where id = ?1",
                params![id, change.project_folder],
            )?;
        }
        tx.execute(
            "update threads set folder = ?2, folder_gone = ?3 where id = ?1",
            params![id, change.folder, change.folder_gone],
        )?;
        if let Some(desk_job) = change.desk_job {
            tx.execute("update threads set desk_job = ?2 where id = ?1", params![id, desk_job])?;
        }
        if let Some(cwd) = &change.last_cwd {
            tx.execute("update threads set last_cwd = ?2 where id = ?1", params![id, cwd])?;
        }
        if let Some(branch) = &change.branch {
            tx.execute("update threads set branch = ?2 where id = ?1", params![id, branch])?;
        }

        insert_messages(tx, id, change)?;
        apply_titles(tx, id, change)?;
        insert_keys(tx, id, change)?;
        write_title_row(tx, id)?;
        write_file_row(tx, path, state, false)
    }

    pub fn mark_scripted(&mut self, path: &str, state: &FileState) -> rusqlite::Result<()> {
        self.delete_thread(path)?;
        write_file_row(&self.tx, path, state, true)
    }

    pub fn remove(&mut self, path: &str) -> rusqlite::Result<()> {
        self.delete_thread(path)?;
        self.tx.execute("delete from files where path = ?1", [path])?;
        Ok(())
    }

    pub fn commit(self, now: i64) -> rusqlite::Result<()> {
        self.tx.execute(
            "insert into meta(key, value) values('last_update', ?1)
             on conflict(key) do update set value = excluded.value",
            [now.to_string()],
        )?;
        self.tx.commit()
    }

    fn delete_thread(&self, path: &str) -> rusqlite::Result<()> {
        let tx = &self.tx;
        let Some(id) = thread_id(tx, path)? else {
            return Ok(());
        };
        let first = id << MESSAGE_ROWID_SHIFT;
        tx.execute(
            "delete from messages where rowid between ?1 and ?2",
            [first, first | MAX_MESSAGE_SEQ],
        )?;
        tx.execute("delete from titles where rowid = ?1", [id])?;
        tx.execute("delete from keys where thread_id = ?1", [id])?;
        tx.execute("delete from threads where id = ?1", [id])?;
        Ok(())
    }
}

fn thread_id(tx: &Transaction<'_>, path: &str) -> rusqlite::Result<Option<i64>> {
    tx.query_row("select id from threads where path = ?1", [path], |r| r.get(0))
        .optional()
}

fn insert_messages(tx: &Transaction<'_>, id: i64, change: &ThreadChange) -> rusqlite::Result<()> {
    let (mut first_prompt, mut first_ts, mut last_ts, stored): (String, Option<i64>, Option<i64>, i64) =
        tx.query_row(
            "select first_prompt, first_ts, last_ts, messages from threads where id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
    let mut prompts = 0i64;
    let added = change.messages.len() as i64;
    if stored + added > MAX_MESSAGE_SEQ {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
            Some("thread has more messages than its rowid range holds".to_string()),
        ));
    }

    let mut insert =
        tx.prepare("insert into messages(rowid, text, thread_id, role, ts) values(?1, ?2, ?3, ?4, ?5)")?;
    for (seq, message) in (stored + 1..).zip(&change.messages) {
        let rowid = (id << MESSAGE_ROWID_SHIFT) | seq;
        insert.execute(params![rowid, message.text, id, role_name(message.role), message.ts])?;
        if let Some(ts) = message.ts {
            first_ts = Some(first_ts.map_or(ts, |t| t.min(ts)));
            last_ts = Some(last_ts.map_or(ts, |t| t.max(ts)));
        }
        match message.role {
            Role::User => {
                prompts += 1;
                if first_prompt.is_empty() {
                    first_prompt = one_line_prefix(&message.text, FIRST_PROMPT_CHARS);
                }
            }
            Role::Summary | Role::Assistant => {}
        }
    }
    tx.execute(
        "update threads set first_prompt = ?2, first_ts = ?3, last_ts = ?4, prompts = prompts + ?5,
           messages = messages + ?6 where id = ?1",
        params![id, first_prompt, first_ts, last_ts, prompts, added],
    )?;
    Ok(())
}

fn apply_titles(tx: &Transaction<'_>, id: i64, change: &ThreadChange) -> rusqlite::Result<()> {
    if let Some(title) = &change.title_custom {
        tx.execute(
            "update threads set title = ?2, title_custom = 1 where id = ?1",
            params![id, title],
        )?;
    } else if let Some(title) = &change.title_ai {
        tx.execute(
            "update threads set title = ?2 where id = ?1 and title_custom = 0",
            params![id, title],
        )?;
    }
    Ok(())
}

fn insert_keys(tx: &Transaction<'_>, id: i64, change: &ThreadChange) -> rusqlite::Result<()> {
    let mut insert =
        tx.prepare("insert or ignore into keys(thread_id, kind, value, repo) values(?1, ?2, ?3, ?4)")?;
    for ticket in &change.tickets {
        insert.execute(params![id, KEY_TICKET, ticket, ""])?;
    }
    for pr in &change.prs {
        let repo = pr.repo.as_deref().map(str::to_lowercase).unwrap_or_default();
        insert.execute(params![id, KEY_PR, pr.number.to_string(), repo])?;
    }
    Ok(())
}

fn write_title_row(tx: &Transaction<'_>, id: i64) -> rusqlite::Result<()> {
    let (title, cwd, folder, branch): (String, Option<String>, String, Option<String>) = tx.query_row(
        "select title, last_cwd, folder, branch from threads where id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let name = last_segment(cwd.as_deref().unwrap_or(&folder));
    let place = format!("{} {}", name, branch.unwrap_or_default());
    tx.execute("delete from titles where rowid = ?1", [id])?;
    tx.execute(
        "insert into titles(rowid, title, place, thread_id) values(?1, ?2, ?3, ?1)",
        params![id, title, place],
    )?;
    Ok(())
}

fn write_file_row(tx: &Transaction<'_>, path: &str, state: &FileState, scripted: bool) -> rusqlite::Result<()> {
    tx.execute(
        "insert into files(path, size, mtime_ns, offset, check_value, scripted, entry_seen)
           values(?1, ?2, ?3, ?4, ?5, ?6, ?7)
         on conflict(path) do update set size = excluded.size, mtime_ns = excluded.mtime_ns,
           offset = excluded.offset, check_value = excluded.check_value, scripted = excluded.scripted,
           entry_seen = excluded.entry_seen",
        params![
            path,
            state.size as i64,
            state.mtime_ns,
            state.offset as i64,
            // Bit-preserving: the value is a hash, so the sign bit carries information.
            state.check_value as i64,
            scripted,
            state.entry_seen
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    fn db_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("nested").join("index.db")
    }

    fn state(offset: u64) -> FileState {
        FileState { size: offset + 10, mtime_ns: 5, offset, check_value: u64::MAX - 3, entry_seen: true }
    }

    fn msg(role: Role, text: &str, ts: i64) -> NewMessage {
        NewMessage { role, text: text.to_string(), ts: Some(ts) }
    }

    fn base_change() -> ThreadChange {
        ThreadChange {
            session_id: "sess-1".into(),
            project_folder: "-proj".into(),
            folder: "/work/widgets".into(),
            messages: vec![
                msg(Role::User, "  fix   the\n widget  bug ", 100),
                msg(Role::Assistant, "looking at DEMO-101", 90),
            ],
            tickets: vec!["DEMO-101".into()],
            prs: vec![PrKey { number: 7, repo: Some("Acme/Widgets".into()) }],
            ..Default::default()
        }
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.conn().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn commit_apply(store: &mut Store, change: &ThreadChange) {
        let mut tx = store.begin_update().unwrap().unwrap();
        tx.apply("/t/a.jsonl", &state(20), change).unwrap();
        tx.commit(1000).unwrap();
    }

    #[test]
    fn open_creates_private_wal_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        let store = Store::open(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let journal: String = store.conn().query_row("pragma journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(journal, "wal");
        let stamp: i32 = store.conn().query_row("pragma application_id", [], |r| r.get(0)).unwrap();
        assert_eq!(stamp, APPLICATION_ID);
    }

    #[test]
    fn other_schema_version_is_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        {
            let mut store = Store::open(&path).unwrap();
            commit_apply(&mut store, &base_change());
            store
                .conn()
                .execute("update meta set value = '0' where key = 'schema_version'", [])
                .unwrap();
        }
        assert!(Store::open_existing(&path).unwrap().is_none());
        let store = Store::open(&path).unwrap();
        assert_eq!(count(&store, "select count(*) from threads"), 0);
        assert!(Store::open_existing(&path).unwrap().is_some());
    }

    fn assert_usable(mut store: Store) {
        commit_apply(&mut store, &base_change());
        assert_eq!(count(&store, "select count(*) from threads"), 1);
    }

    #[test]
    fn garbage_file_is_set_aside_as_bad() {
        let garbage = vec![0x41u8; 5000];
        let wal_garbage = vec![0x57u8; 700];
        for fresh in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = db_path(&dir);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, &garbage).unwrap();
            fs::write(sibling(&path, "-wal"), &wal_garbage).unwrap();
            fs::write(sibling(&path, "-shm"), &wal_garbage).unwrap();
            fs::write(sibling(&path, ".bad"), b"older bad file").unwrap();
            assert!(Store::open_existing(&path).unwrap().is_none());

            let store = if fresh { Store::open_fresh(&path) } else { Store::open(&path) }.unwrap();
            let bad = fs::read(sibling(&path, ".bad")).unwrap_or_default();
            assert!(bad == garbage, ".bad must hold the old bytes, fresh={fresh}");
            for companion in ["-wal", "-shm"] {
                let now = fs::read(sibling(&path, companion)).unwrap_or_default();
                assert!(now != wal_garbage, "{companion} kept, fresh={fresh}");
            }
            assert!(sibling(&path, ".lock").exists());
            assert_usable(store);
        }
    }

    /// Chromium and Electron databases carry a `meta(key, value)` table of their own.
    const FOREIGN_SCHEMAS: [&str; 2] = [
        "create table notes(body text); insert into notes values('keep me');",
        "create table meta(key text, value text); insert into meta values('version', '7');
         create table urls(id integer primary key, url text); insert into urls(url) values('keep me');",
    ];

    fn foreign_database(path: &Path, schema: &str) -> Vec<u8> {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        Connection::open(path).unwrap().execute_batch(schema).unwrap();
        fs::read(path).unwrap()
    }

    #[test]
    fn foreign_database_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        for (i, schema) in FOREIGN_SCHEMAS.into_iter().enumerate() {
            let path = dir.path().join(format!("app{i}")).join("index.db");
            let before = foreign_database(&path, schema);
            for opened in [Store::open(&path), Store::open_fresh(&path)] {
                let message = opened.err().expect("a foreign database is an error").to_string();
                assert!(message.contains("not a thread-search index"), "{message}");
                assert!(message.contains("will not replace it"), "{message}");
            }
            assert!(fs::read(&path).unwrap() == before, "the foreign file changed: {schema}");
            assert!(!sibling(&path, ".bad").exists());
            assert!(Store::open_existing(&path).unwrap().is_none());
        }
    }

    #[test]
    fn zero_length_file_becomes_the_index() {
        let dir = tempfile::tempdir().unwrap();
        for fresh in [false, true] {
            let path = dir.path().join(format!("empty-{fresh}.db"));
            fs::write(&path, b"").unwrap();
            let store = if fresh { Store::open_fresh(&path) } else { Store::open(&path) }.unwrap();
            assert!(!sibling(&path, ".bad").exists(), "fresh={fresh}");
            assert_usable(store);
        }
    }

    fn index_with(path: &Path, change: &str) {
        let mut store = Store::open(path).unwrap();
        commit_apply(&mut store, &base_change());
        store.conn().execute_batch(change).unwrap();
    }

    #[test]
    fn older_index_of_ours_is_rebuilt_without_a_bad_file() {
        let dir = tempfile::tempdir().unwrap();
        for (i, stale) in [
            "update meta set value = '1' where key = 'schema_version'",
            // The stamp alone marks the file as ours.
            "delete from meta where key = 'schema_version'",
            "pragma application_id = 0; update meta set value = '2' where key = 'schema_version'",
        ]
        .into_iter()
        .enumerate()
        {
            let path = dir.path().join(format!("v{i}")).join("index.db");
            index_with(&path, stale);
            let store = Store::open(&path).unwrap();
            assert_eq!(count(&store, "select count(*) from threads"), 0, "{stale}");
            assert!(!sibling(&path, ".bad").exists(), "{stale}");
            let stamp: i32 = store.conn().query_row("pragma application_id", [], |r| r.get(0)).unwrap();
            assert_eq!(stamp, APPLICATION_ID, "{stale}");
        }
    }

    #[test]
    fn unstamped_file_without_a_schema_version_is_not_ours() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        index_with(&path, "pragma application_id = 0; delete from meta where key = 'schema_version'");
        let before = fs::read(&path).unwrap();
        for opened in [Store::open(&path), Store::open_fresh(&path)] {
            let message = opened.err().expect("an unstamped file is not ours").to_string();
            assert!(message.contains("not a thread-search index"), "{message}");
        }
        assert!(fs::read(&path).unwrap() == before);
    }

    #[test]
    fn unstamped_file_with_an_unknown_schema_version_is_not_ours() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        index_with(&path, "pragma application_id = 0; update meta set value = '7' where key = 'schema_version'");
        let before = fs::read(&path).unwrap();
        for opened in [Store::open(&path), Store::open_fresh(&path)] {
            let message = opened.err().expect("an unstamped file of an unknown version is not ours").to_string();
            assert!(message.contains("not a thread-search index"), "{message}");
        }
        assert!(fs::read(&path).unwrap() == before);
    }

    #[test]
    fn database_with_another_application_id_is_not_ours() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        index_with(&path, "pragma application_id = 7");
        assert!(Store::open_existing(&path).unwrap().is_none());
        assert!(Store::open(&path).is_err());
        assert!(path.exists());
    }

    #[test]
    fn open_fresh_holds_the_lock_and_drops_the_old_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        {
            let mut old = Store::open(&path).unwrap();
            commit_apply(&mut old, &base_change());
        }
        let fresh = Store::open_fresh(&path).unwrap();
        assert_eq!(count(&fresh, "select count(*) from threads"), 0);
        assert!(Store::open(&path).is_err_and(|e| is_busy(&e)));
        drop(fresh);
        assert!(Store::open(&path).is_ok());
    }

    #[test]
    fn open_fresh_while_locked_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        let mut held = Store::open(&path).unwrap();
        commit_apply(&mut held, &base_change());
        assert!(Store::open_fresh(&path).is_err_and(|e| is_busy(&e)));
        assert_eq!(count(&held, "select count(*) from threads"), 1);
        assert!(path.exists());
    }

    #[test]
    fn last_update_and_stats_read_the_committed_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.last_update().unwrap(), None);
        assert_eq!(store.stats().unwrap(), Stats { threads: 0, messages: 0, last_update: None });
        commit_apply(&mut store, &base_change());
        assert_eq!(store.last_update().unwrap(), Some(1000));
        assert_eq!(store.stats().unwrap(), Stats { threads: 1, messages: 2, last_update: Some(1000) });
    }

    #[test]
    fn stats_count_only_threads_with_messages() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&db_path(&dir)).unwrap();
        store
            .conn()
            .execute(
                "insert into threads(path, agent, session_id, project_folder, desk_job, folder, folder_gone, messages)
                 values('/t/empty', 'claude', 'e', '-p', 0, '/w', 0, 0)",
                [],
            )
            .unwrap();
        assert_eq!(store.stats().unwrap().threads, 0);
    }

    #[test]
    fn concurrent_first_opens_never_panic_or_delete() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..50 {
            let path = dir.path().join(format!("idx{i}")).join("index.db");
            let results: Vec<_> = std::thread::scope(|s| {
                let a = s.spawn(|| Store::open(&path).map(drop));
                let b = s.spawn(|| Store::open(&path).map(drop));
                vec![a.join().unwrap(), b.join().unwrap()]
            });
            for result in results {
                if let Err(e) = result {
                    assert!(is_busy(&e), "unexpected error: {e}");
                }
            }
            let reopened = Store::open_existing(&path).unwrap();
            assert!(reopened.is_some(), "index missing after concurrent open {i}");
        }
    }

    fn insert_row_through_open(path: &Path, thread_path: &str) -> rusqlite::Result<()> {
        let mut store = Store::open(path)?;
        let mut tx = store.begin_update()?.expect("the sidecar lock excludes other writers");
        tx.apply(thread_path, &state(20), &base_change())?;
        tx.commit(1000)
    }

    fn race_two_opens(dir: &tempfile::TempDir, prepare: impl Fn(&Path)) {
        for i in 0..50 {
            let path = dir.path().join(format!("race{i}")).join("index.db");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            prepare(&path);
            let results: Vec<_> = std::thread::scope(|s| {
                let a = s.spawn(|| insert_row_through_open(&path, "/t/a.jsonl"));
                let b = s.spawn(|| insert_row_through_open(&path, "/t/b.jsonl"));
                vec![a.join().unwrap(), b.join().unwrap()]
            });
            let mut committed = 0;
            for result in results {
                match result {
                    Ok(()) => committed += 1,
                    Err(e) => assert!(is_busy(&e), "unexpected error: {e}"),
                }
            }
            assert!(committed >= 1, "no open won on {i}");
            let reader = Store::open_existing(&path).unwrap().expect("index present");
            assert_eq!(count(&reader, "select count(*) from threads"), committed, "path {i}");
        }
    }

    #[test]
    fn racing_opens_on_old_version_keep_committed_rows() {
        let dir = tempfile::tempdir().unwrap();
        race_two_opens(&dir, |path| {
            let store = Store::open(path).unwrap();
            store
                .conn()
                .execute("update meta set value = '0' where key = 'schema_version'", [])
                .unwrap();
        });
    }

    #[test]
    fn racing_opens_on_garbage_keep_committed_rows() {
        let dir = tempfile::tempdir().unwrap();
        race_two_opens(&dir, |path| fs::write(path, vec![0x42u8; 5000]).unwrap());
    }

    #[test]
    fn second_open_is_busy_while_first_store_lives() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        let first = Store::open(&path).unwrap();
        let started = Instant::now();
        let second = Store::open(&path);
        assert!(second.is_err_and(|e| is_busy(&e)));
        assert!(started.elapsed() < Duration::from_millis(200));
        drop(first);
        assert!(Store::open(&path).is_ok());
    }

    #[test]
    fn open_with_held_write_lock_keeps_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        let mut holder = Store::open(&path).unwrap();
        let mut tx = holder.begin_update().unwrap().unwrap();
        tx.apply("/t/a.jsonl", &state(20), &base_change()).unwrap();
        let blocked = Store::open(&path);
        assert!(blocked.is_err_and(|e| is_busy(&e)));
        assert!(path.exists());
        tx.commit(1000).unwrap();
        assert_eq!(count(&holder, "select count(*) from threads"), 1);
        let reread = Store::open_existing(&path).unwrap().unwrap();
        assert_eq!(count(&reread, "select count(*) from threads"), 1);
    }

    #[test]
    fn open_existing_ignores_empty_and_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.db");
        fs::write(&empty, b"").unwrap();
        assert!(Store::open_existing(&empty).unwrap().is_none());
        assert_eq!(fs::metadata(&empty).unwrap().len(), 0);

        let foreign = dir.path().join("foreign.db");
        Connection::open(&foreign)
            .unwrap()
            .execute_batch("create table other(x integer)")
            .unwrap();
        let before = fs::read(&foreign).unwrap();
        assert!(Store::open_existing(&foreign).unwrap().is_none());
        assert_eq!(fs::read(&foreign).unwrap(), before);
    }

    #[test]
    fn open_returns_error_and_keeps_unreadable_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        drop(Store::open(&path).unwrap());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let opened = Store::open(&path);
        let existing = Store::open_existing(&path);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(opened.is_err());
        assert!(existing.is_err());
        assert!(path.exists());
        assert!(Store::open_existing(&path).unwrap().is_some());
    }

    #[test]
    fn summary_message_leaves_summary_column_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&db_path(&dir)).unwrap();
        let change = ThreadChange {
            folder: "/w".into(),
            messages: vec![msg(Role::Summary, "compact recap", 1)],
            ..Default::default()
        };
        commit_apply(&mut store, &change);
        let summary: String = store.conn().query_row("select summary from threads", [], |r| r.get(0)).unwrap();
        assert_eq!(summary, "");
        assert_eq!(count(&store, "select count(*) from messages where role = 'summary'"), 1);
    }

    #[test]
    fn open_existing_missing_path_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Store::open_existing(&db_path(&dir)).unwrap().is_none());
        assert!(!db_path(&dir).exists());
    }

    #[test]
    fn index_gone_between_the_check_and_the_open_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        assert!(open_read_only(&path).unwrap().is_none());
    }

    #[test]
    fn open_existing_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        drop(Store::open(&path).unwrap());
        let reader = Store::open_existing(&path).unwrap().unwrap();
        assert!(reader.conn().execute("insert into meta values('x', 'y')", []).is_err());
    }

    #[test]
    fn second_writer_gets_none_quickly() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        let mut first = Store::open(&path).unwrap();
        let mut second = Store { conn: Connection::open(&path).unwrap(), _lock: None };
        second.conn.busy_timeout(Duration::ZERO).unwrap();
        let _held = first.begin_update().unwrap().unwrap();
        let started = Instant::now();
        assert!(second.begin_update().unwrap().is_none());
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn apply_then_commit_stores_thread_messages_and_keys() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&db_path(&dir)).unwrap();
        commit_apply(&mut store, &base_change());

        assert_eq!(count(&store, "select count(*) from threads"), 1);
        assert_eq!(count(&store, "select count(*) from messages"), 2);
        assert_eq!(count(&store, "select count(*) from keys where kind='ticket' and value='DEMO-101' and repo=''"), 1);
        assert_eq!(count(&store, "select count(*) from keys where kind='pr' and value='7' and repo='acme/widgets'"), 1);
        let (agent, prompt, prompts, first, last): (String, String, i64, i64, i64) = store
            .conn()
            .query_row(
                "select agent, first_prompt, prompts, first_ts, last_ts from threads",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!((agent.as_str(), prompt.as_str(), prompts, first, last), ("claude", "fix the widget bug", 1, 90, 100));
        let last_update: String = store
            .conn()
            .query_row("select value from meta where key='last_update'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(last_update, "1000");
        let place: String = store.conn().query_row("select place from titles", [], |r| r.get(0)).unwrap();
        assert_eq!(place, "widgets ");
    }

    #[test]
    fn second_apply_adds_messages_and_keeps_first_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&db_path(&dir)).unwrap();
        commit_apply(&mut store, &base_change());
        let more = ThreadChange {
            folder: "/work/widgets".into(),
            messages: vec![msg(Role::User, "another prompt", 200)],
            last_cwd: Some("/work/widgets/api".into()),
            branch: Some("feat/x".into()),
            ..Default::default()
        };
        commit_apply(&mut store, &more);

        assert_eq!(count(&store, "select count(*) from threads"), 1);
        assert_eq!(count(&store, "select count(*) from messages"), 3);
        assert_eq!(count(&store, "select prompts from threads"), 2);
        assert_eq!(count(&store, "select last_ts from threads"), 200);
        let prompt: String = store.conn().query_row("select first_prompt from threads", [], |r| r.get(0)).unwrap();
        assert_eq!(prompt, "fix the widget bug");
        let place: String = store.conn().query_row("select place from titles", [], |r| r.get(0)).unwrap();
        assert_eq!(place, "api feat/x");
    }

    #[test]
    fn reset_replaces_messages() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&db_path(&dir)).unwrap();
        commit_apply(&mut store, &base_change());
        let fresh = ThreadChange {
            reset: true,
            folder: "/work/widgets".into(),
            messages: vec![msg(Role::User, "only one", 5)],
            ..Default::default()
        };
        commit_apply(&mut store, &fresh);
        assert_eq!(count(&store, "select count(*) from messages"), 1);
        assert_eq!(count(&store, "select count(*) from threads"), 1);
        assert_eq!(count(&store, "select count(*) from keys"), 0);
    }

    #[test]
    fn custom_title_wins_in_any_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&db_path(&dir)).unwrap();
        let ai = ThreadChange { folder: "/w".into(), title_ai: Some("ai title".into()), ..Default::default() };
        let custom = ThreadChange { folder: "/w".into(), title_custom: Some("custom title".into()), ..Default::default() };
        let title = |s: &Store| -> String { s.conn().query_row("select title from threads", [], |r| r.get(0)).unwrap() };

        commit_apply(&mut store, &ai);
        assert_eq!(title(&store), "ai title");
        commit_apply(&mut store, &custom);
        assert_eq!(title(&store), "custom title");
        commit_apply(&mut store, &ai);
        assert_eq!(title(&store), "custom title");
        let in_titles: String = store.conn().query_row("select title from titles", [], |r| r.get(0)).unwrap();
        assert_eq!(in_titles, "custom title");
    }

    #[test]
    fn remove_deletes_everything_for_the_thread() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&db_path(&dir)).unwrap();
        commit_apply(&mut store, &base_change());
        let mut tx = store.begin_update().unwrap().unwrap();
        tx.remove("/t/a.jsonl").unwrap();
        tx.commit(2000).unwrap();
        for table in ["threads", "keys", "messages", "titles", "files"] {
            assert_eq!(count(&store, &format!("select count(*) from {table}")), 0, "{table}");
        }
    }

    fn rowids(store: &Store, thread_path: &str) -> Vec<i64> {
        let mut stmt = store
            .conn()
            .prepare(
                "select m.rowid from messages m join threads t on t.id = m.thread_id
                 where t.path = ?1 order by m.rowid",
            )
            .unwrap();
        stmt.query_map([thread_path], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    }

    fn thread_of(store: &Store, thread_path: &str) -> (i64, i64) {
        store
            .conn()
            .query_row("select id, messages from threads where path = ?1", [thread_path], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap()
    }

    #[test]
    fn message_rowids_encode_the_thread_and_removal_keeps_other_threads() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&db_path(&dir)).unwrap();
        let later = ThreadChange {
            folder: "/work/widgets".into(),
            messages: vec![msg(Role::User, "later prompt", 300)],
            ..Default::default()
        };
        let mut tx = store.begin_update().unwrap().unwrap();
        tx.apply("/t/a.jsonl", &state(20), &base_change()).unwrap();
        tx.apply("/t/b.jsonl", &state(20), &base_change()).unwrap();
        tx.apply("/t/a.jsonl", &state(30), &later).unwrap();
        tx.commit(1000).unwrap();

        let (a, a_count) = thread_of(&store, "/t/a.jsonl");
        let (b, b_count) = thread_of(&store, "/t/b.jsonl");
        assert_eq!((a_count, b_count), (3, 2));
        let base = |id: i64| id << MESSAGE_ROWID_SHIFT;
        assert_eq!(rowids(&store, "/t/a.jsonl"), [base(a) | 1, base(a) | 2, base(a) | 3]);
        assert_eq!(rowids(&store, "/t/b.jsonl"), [base(b) | 1, base(b) | 2]);

        let mut tx = store.begin_update().unwrap().unwrap();
        tx.remove("/t/a.jsonl").unwrap();
        tx.commit(2000).unwrap();
        assert_eq!(count(&store, "select count(*) from messages"), 2);
        assert_eq!(rowids(&store, "/t/b.jsonl"), [base(b) | 1, base(b) | 2]);
    }

    #[test]
    fn mark_scripted_drops_thread_and_keeps_file_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&db_path(&dir)).unwrap();
        commit_apply(&mut store, &base_change());
        let mut tx = store.begin_update().unwrap().unwrap();
        tx.mark_scripted("/t/a.jsonl", &state(30)).unwrap();
        let (st, scripted) = tx.file_state("/t/a.jsonl").unwrap().unwrap();
        assert!(scripted);
        assert_eq!(st.check_value, u64::MAX - 3);
        assert_eq!(tx.known_paths().unwrap(), vec!["/t/a.jsonl".to_string()]);
        tx.commit(2000).unwrap();
        assert_eq!(count(&store, "select count(*) from threads"), 0);
        assert_eq!(count(&store, "select count(*) from messages"), 0);
    }

    #[test]
    fn dropped_tx_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&db_path(&dir)).unwrap();
        {
            let mut tx = store.begin_update().unwrap().unwrap();
            tx.apply("/t/a.jsonl", &state(20), &base_change()).unwrap();
        }
        assert_eq!(count(&store, "select count(*) from threads"), 0);
        assert_eq!(count(&store, "select count(*) from files"), 0);
    }

    #[test]
    fn reader_sees_old_state_until_commit() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(&dir);
        let mut writer = Store::open(&path).unwrap();
        let reader = Store::open_existing(&path).unwrap().unwrap();
        let mut tx = writer.begin_update().unwrap().unwrap();
        tx.apply("/t/a.jsonl", &state(20), &base_change()).unwrap();
        assert_eq!(count(&reader, "select count(*) from threads"), 0);
        tx.commit(1000).unwrap();
        let after = Store::open_existing(&path).unwrap().unwrap();
        assert_eq!(count(&after, "select count(*) from threads"), 1);
    }
}
