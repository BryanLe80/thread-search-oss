use std::collections::HashSet;
use std::error::Error;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::ops::ControlFlow;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

use memchr::memchr;
use crate::folder::{Dirs, FolderCache, FolderState};
use crate::keys::{PrKey, branch_ticket_keys, pr_keys, push_new, ticket_keys};
use crate::store::{FileState, NewMessage, Store, ThreadChange, UpdateTx, is_busy};
use crate::time::NANOS_PER_SECOND;
use crate::transcript::{Item, Parsed, Role, parse_line};

const CHUNK_BYTES: usize = 256 * 1024;
const CHECK_SPAN: usize = 4096;
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateReport {
    pub scanned: u32,
    pub unchanged: u32,
    pub appended: u32,
    pub reindexed: u32,
    pub removed: u32,
    pub scripted: u32,
    pub busy: bool,
}

impl UpdateReport {
    fn busy() -> UpdateReport {
        UpdateReport { busy: true, ..Default::default() }
    }
}

/// Brings the index up to date in one transaction. A file whose size and mtime are unchanged is
/// not opened, so the no-change path costs one directory scan, one `stat` and one `files` row
/// query per file, one query for the known paths, and the commit.
pub fn update(
    root: &Path,
    index: &Path,
    home: &Path,
    dirs: &dyn Dirs,
    now: i64,
) -> Result<UpdateReport, Box<dyn Error>> {
    open_and_run(Store::open(index), root, home, dirs, now)
}

/// Deletes the index and indexes everything again. The writer lock is taken before the delete and
/// kept until the commit, so no other update can write to the file being replaced.
pub fn rebuild(
    root: &Path,
    index: &Path,
    home: &Path,
    dirs: &dyn Dirs,
    now: i64,
) -> Result<UpdateReport, Box<dyn Error>> {
    open_and_run(Store::open_fresh(index), root, home, dirs, now)
}

fn open_and_run(
    opened: rusqlite::Result<Store>,
    root: &Path,
    home: &Path,
    dirs: &dyn Dirs,
    now: i64,
) -> Result<UpdateReport, Box<dyn Error>> {
    match opened {
        Ok(store) => run(store, root, home, dirs, now),
        Err(e) if is_busy(&e) => Ok(UpdateReport::busy()),
        Err(e) => Err(e.into()),
    }
}

fn run(
    mut store: Store,
    root: &Path,
    home: &Path,
    dirs: &dyn Dirs,
    now: i64,
) -> Result<UpdateReport, Box<dyn Error>> {
    let Some(tx) = store.begin_update()? else {
        return Ok(UpdateReport::busy());
    };
    let scan = scan(root)?;
    let mut run = Run {
        tx,
        folders: FolderCache::new(home, dirs),
        reader: LineReader::new(CHUNK_BYTES),
        report: UpdateReport { scanned: scan.found.len() as u32, ..Default::default() },
    };
    for file in &scan.files {
        run.file(file)?;
    }
    for path in run.tx.known_paths()? {
        if !scan.found.contains(&path) && !scan.in_unlisted_folder(&path) {
            run.tx.remove(&path)?;
            run.report.removed += 1;
        }
    }
    run.tx.commit(now)?;
    Ok(run.report)
}

struct Transcript {
    path: String,
    project_folder: String,
    session_id: String,
    size: u64,
    mtime_ns: i64,
}

#[derive(Default)]
struct Scan {
    files: Vec<Transcript>,
    found: HashSet<String>,
    // A folder that cannot be listed hides its files, which must not count as deleted.
    unlisted: HashSet<PathBuf>,
    // The root listing broke off, so any project folder may be missing from it.
    root_unlisted: bool,
}

impl Scan {
    fn in_unlisted_folder(&self, path: &str) -> bool {
        self.root_unlisted || Path::new(path).parent().is_some_and(|dir| self.unlisted.contains(dir))
    }
}

/// Lists `<root>/*/*.jsonl` only; subagent transcripts sit one level deeper and are never read.
fn scan(root: &Path) -> io::Result<Scan> {
    let mut scan = Scan::default();
    let projects = match fs::read_dir(root) {
        Ok(projects) => projects,
        // A machine that never ran Claude Code has no projects folder: that is zero files.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(scan),
        Err(e) => return Err(e),
    };
    for project in projects {
        let Ok(project) = project else {
            scan.root_unlisted = true;
            break;
        };
        let Ok(project_folder) = project.file_name().into_string() else {
            continue;
        };
        let dir = project.path();
        match project.file_type() {
            Ok(t) if t.is_dir() => {}
            Ok(_) => continue,
            Err(_) => {
                scan.unlisted.insert(dir);
                continue;
            }
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            scan.unlisted.insert(dir);
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                scan.unlisted.insert(dir.clone());
                break;
            };
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            // A file named only `.jsonl` has no session id, so it has nothing to resume.
            let Some(session_id) = name.strip_suffix(".jsonl").filter(|id| !id.is_empty()) else {
                continue;
            };
            let Some(path) = dir.join(&name).to_str().map(String::from) else {
                continue;
            };
            match entry.file_type() {
                Ok(t) if t.is_file() => {}
                Ok(_) => continue,
                // It may be a transcript: it counts as present, so its indexed thread is kept.
                Err(_) => {
                    scan.found.insert(path);
                    continue;
                }
            }
            scan.found.insert(path.clone());
            // A file that cannot be stat'ed stays in `found`, so its indexed thread is kept.
            if let Ok(meta) = entry.metadata() {
                scan.files.push(Transcript {
                    path,
                    project_folder: project_folder.clone(),
                    session_id: session_id.to_string(),
                    size: meta.size(),
                    mtime_ns: meta.mtime() * NANOS_PER_SECOND + meta.mtime_nsec(),
                });
            }
        }
    }
    Ok(scan)
}

enum Outcome {
    Scripted,
    Thread { change: Box<ThreadChange>, state: FileState, append: bool },
}

struct Run<'a, 't> {
    tx: UpdateTx<'t>,
    folders: FolderCache<'a>,
    reader: LineReader,
    report: UpdateReport,
}

impl Run<'_, '_> {
    fn file(&mut self, t: &Transcript) -> rusqlite::Result<()> {
        let stored = self.tx.file_state(&t.path)?;
        let same = |st: &FileState| st.size == t.size && st.mtime_ns == t.mtime_ns;
        match &stored {
            Some((st, true)) => {
                if !same(st) {
                    self.tx.mark_scripted(&t.path, &scripted_state(t))?;
                }
                self.report.scripted += 1;
                return Ok(());
            }
            Some((st, false)) if same(st) => {
                self.report.unchanged += 1;
                return Ok(());
            }
            _ => {}
        }
        let known = stored.map(|(st, _)| st);
        // An unreadable file is left as it was; the next run tries it again.
        let Ok(outcome) = self.read(t, known.as_ref()) else {
            return Ok(());
        };
        match outcome {
            Outcome::Scripted => {
                self.tx.mark_scripted(&t.path, &scripted_state(t))?;
                self.report.scripted += 1;
            }
            Outcome::Thread { mut change, state, append } => {
                let cwd = match &change.last_cwd {
                    Some(cwd) => Some(cwd.clone()),
                    None if append => self.tx.last_cwd(&t.path)?,
                    None => None,
                };
                let folder = self.folders.resolve(&t.project_folder, cwd.as_deref());
                change.folder = folder.path.to_string_lossy().into_owned();
                change.folder_gone = folder.state == FolderState::Gone;
                self.tx.apply(&t.path, &state, &change)?;
                if append || known.is_none() {
                    self.report.appended += 1;
                } else {
                    self.report.reindexed += 1;
                }
            }
        }
        Ok(())
    }

    fn read(&mut self, t: &Transcript, known: Option<&FileState>) -> io::Result<Outcome> {
        let mut file = File::open(&t.path)?;
        let resume = match known {
            Some(st) if t.size > st.size && check_value(&file, st.offset)? == st.check_value => Some(st),
            _ => None,
        };
        let start = resume.map_or(0, |st| st.offset);
        file.seek(SeekFrom::Start(start))?;
        let mut fold = Fold::new(t, resume.is_some_and(|st| st.entry_seen));
        fold.change.reset = resume.is_none() && known.is_some();
        // Bounded by the size from `stat`, so the stored offset never passes the stored size.
        let consumed = self.reader.read_lines((&file).take(t.size - start), |line| fold.line(line))?;
        if fold.scripted {
            return Ok(Outcome::Scripted);
        }
        let offset = start + consumed;
        let check_value = match resume {
            Some(st) if consumed == 0 => st.check_value,
            _ => check_value(&file, offset)?,
        };
        let state = FileState { size: t.size, mtime_ns: t.mtime_ns, offset, check_value, entry_seen: fold.entry_seen };
        Ok(Outcome::Thread { change: Box::new(fold.change), state, append: resume.is_some() })
    }
}

/// A scripted file is never read again, so its offset and check value are never compared.
fn scripted_state(t: &Transcript) -> FileState {
    FileState { size: t.size, mtime_ns: t.mtime_ns, offset: 0, check_value: 0, entry_seen: false }
}

struct Fold {
    change: ThreadChange,
    entry_seen: bool,
    scripted: bool,
}

impl Fold {
    fn new(t: &Transcript, entry_seen: bool) -> Fold {
        let change = ThreadChange {
            session_id: t.session_id.clone(),
            project_folder: t.project_folder.clone(),
            ..Default::default()
        };
        Fold { change, entry_seen, scripted: false }
    }

    fn line(&mut self, line: &[u8]) -> ControlFlow<()> {
        let Some(parsed) = parse_line(line) else {
            return ControlFlow::Continue(());
        };
        if self.is_scripted(&parsed) {
            self.scripted = true;
            return ControlFlow::Break(());
        }
        let Parsed { item, ts, cwd, branch, session_kind, .. } = parsed;
        if session_kind.as_deref() == Some("bg") {
            self.change.desk_job = Some(true);
        }
        if cwd.is_some() {
            self.change.last_cwd = cwd;
        }
        if let Some(branch) = branch.filter(|b| !b.is_empty()) {
            if self.change.branch.as_ref() != Some(&branch) {
                self.add_tickets(branch_ticket_keys(&branch));
            }
            self.change.branch = Some(branch);
        }
        match item {
            Some(Item::Message { role, text }) => {
                if role == Role::User {
                    self.add_keys(&text);
                }
                self.change.messages.push(NewMessage { role, text, ts });
            }
            Some(Item::Title { text, custom }) => {
                self.add_keys(&text);
                if custom {
                    self.change.title_custom = Some(text);
                } else {
                    self.change.title_ai = Some(text);
                }
            }
            Some(Item::PrLink { number, repo }) => push_new(&mut self.change.prs, PrKey { number, repo }),
            None => {}
        }
        ControlFlow::Continue(())
    }

    fn add_keys(&mut self, text: &str) {
        self.add_tickets(ticket_keys(text));
        for key in pr_keys(text) {
            push_new(&mut self.change.prs, key);
        }
    }

    /// The file's first entrypoint decides, on a full read and on an append alike, so `update`
    /// and `rebuild` agree. A later `sdk*` line is a print-mode resume of a typed thread: judging
    /// it would drop that thread until the next rebuild.
    fn is_scripted(&mut self, parsed: &Parsed) -> bool {
        let Some(entrypoint) = &parsed.entrypoint else {
            return false;
        };
        if self.entry_seen {
            return false;
        }
        self.entry_seen = true;
        entrypoint.starts_with("sdk")
    }

    fn add_tickets(&mut self, keys: Vec<String>) {
        for key in keys {
            push_new(&mut self.change.tickets, key);
        }
    }
}

fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Hashes the first and the last bytes before `offset` instead of all of them: a full hash costs
/// a read of the whole file on every append, and a rewrite almost always changes the start or the
/// bytes just before the old end.
fn check_value(file: &File, offset: u64) -> io::Result<u64> {
    let span = offset.min(CHECK_SPAN as u64) as usize;
    let mut block = [0u8; CHECK_SPAN];
    file.read_exact_at(&mut block[..span], 0)?;
    let hash = fnv1a(FNV_OFFSET_BASIS, &block[..span]);
    file.read_exact_at(&mut block[..span], offset - span as u64)?;
    Ok(fnv1a(hash, &block[..span]))
}

/// Splits a stream into complete lines with buffers that are reused across files. Bytes after the
/// last newline are never handed out: a writer may still be appending to that line.
struct LineReader {
    chunk: Vec<u8>,
    // Holds a line that crosses a chunk boundary; it grows to the longest such line.
    long: Vec<u8>,
}

impl LineReader {
    fn new(chunk_bytes: usize) -> LineReader {
        LineReader { chunk: vec![0; chunk_bytes], long: Vec::new() }
    }

    /// Returns the bytes consumed: the end of the last line handed to `each`, newline included.
    fn read_lines<R: Read>(
        &mut self,
        mut src: R,
        mut each: impl FnMut(&[u8]) -> ControlFlow<()>,
    ) -> io::Result<u64> {
        self.long.clear();
        let mut consumed = 0u64;
        loop {
            let filled = match src.read(&mut self.chunk) {
                Ok(0) => return Ok(consumed),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            let mut start = 0;
            while let Some(i) = memchr(b'\n', &self.chunk[start..filled]) {
                let end = start + i;
                let line: &[u8] = if self.long.is_empty() {
                    &self.chunk[start..end]
                } else {
                    self.long.extend_from_slice(&self.chunk[start..end]);
                    &self.long
                };
                consumed += line.len() as u64 + 1;
                let flow = each(line);
                self.long.clear();
                start = end + 1;
                if flow.is_break() {
                    return Ok(consumed);
                }
            }
            self.long.extend_from_slice(&self.chunk[start..filled]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(input: &[u8], chunk: usize) -> (Vec<String>, u64) {
        let mut reader = LineReader::new(chunk);
        let mut out = Vec::new();
        let consumed = reader
            .read_lines(input, |line| {
                out.push(String::from_utf8(line.to_vec()).unwrap());
                ControlFlow::Continue(())
            })
            .unwrap();
        (out, consumed)
    }

    #[test]
    fn yields_complete_lines_and_holds_back_the_tail() {
        assert_eq!(lines(b"ab\n\ncd\nef", 64), (vec!["ab".into(), "".into(), "cd".into()], 7));
    }

    #[test]
    fn lines_longer_than_the_chunk_are_joined() {
        let input = b"0123456789abcdef\nxy\nlong tail without newline";
        assert_eq!(lines(input, 4), (vec!["0123456789abcdef".into(), "xy".into()], 20));
    }

    #[test]
    fn empty_input_consumes_nothing() {
        assert_eq!(lines(b"", 8), (vec![], 0));
        assert_eq!(lines(b"no newline", 4), (vec![], 0));
    }

    #[test]
    fn break_stops_after_the_current_line() {
        let mut reader = LineReader::new(4);
        let mut seen = 0;
        let consumed = reader
            .read_lines(&b"one\ntwo\nthree\n"[..], |_| {
                seen += 1;
                if seen == 2 { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
            })
            .unwrap();
        assert_eq!((seen, consumed), (2, 8));
    }

    #[test]
    fn reader_is_reusable_after_a_cut_line() {
        let mut reader = LineReader::new(4);
        reader.read_lines(&b"partial line"[..], |_| ControlFlow::Continue(())).unwrap();
        let mut out = Vec::new();
        reader
            .read_lines(&b"fresh\n"[..], |line| {
                out.push(line.to_vec());
                ControlFlow::Continue(())
            })
            .unwrap();
        assert_eq!(out, vec![b"fresh".to_vec()]);
    }

    #[test]
    fn fnv1a_matches_reference_vectors() {
        assert_eq!(fnv1a(FNV_OFFSET_BASIS, b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(FNV_OFFSET_BASIS, b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(FNV_OFFSET_BASIS, b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn check_value_covers_head_and_tail_before_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let mut bytes = vec![b'a'; 10_000];
        fs::write(&path, &bytes).unwrap();
        let base = check_value(&File::open(&path).unwrap(), 9_000).unwrap();

        bytes[8_999] = b'b';
        fs::write(&path, &bytes).unwrap();
        assert_ne!(check_value(&File::open(&path).unwrap(), 9_000).unwrap(), base);

        bytes[8_999] = b'a';
        bytes[9_500] = b'b';
        fs::write(&path, &bytes).unwrap();
        assert_eq!(check_value(&File::open(&path).unwrap(), 9_000).unwrap(), base);

        let short = b"ab\n";
        let expected = fnv1a(fnv1a(FNV_OFFSET_BASIS, short), short);
        fs::write(&path, short).unwrap();
        assert_eq!(check_value(&File::open(&path).unwrap(), 3).unwrap(), expected);
    }
}
