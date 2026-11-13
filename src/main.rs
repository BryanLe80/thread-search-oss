use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use thread_search::folder::{Dirs, RealDirs};
use thread_search::output::{State, display_safe, human_search, json_text, stats_text, update_line};
use thread_search::search::{SearchResult, parse_query, search};
use thread_search::store::Store;
use thread_search::update::{UpdateReport, rebuild, update};

const USAGE: &str = "usage: threads update | rebuild | stats | search [--json] [--limit N] [--] <words...>";
const DEFAULT_LIMIT: usize = 20;

struct Paths {
    root: PathBuf,
    index: PathBuf,
    home: PathBuf,
}

struct SearchArgs {
    json: bool,
    limit: usize,
    words: Vec<String>,
}

enum Command {
    Update,
    Rebuild,
    Stats,
    Search(SearchArgs),
}

fn main() -> ExitCode {
    let args: Result<Vec<String>, String> = env::args_os()
        .skip(1)
        .map(|a| a.into_string().map_err(|_| "arguments must be valid UTF-8".to_string()))
        .collect();
    let command = match args.and_then(|args| parse(&args)) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("threads: {}\n{USAGE}", display_safe(&message));
            return ExitCode::from(2);
        }
    };
    match run(command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("threads: {}", display_safe(&e.to_string()));
            ExitCode::from(1)
        }
    }
}

fn parse(args: &[String]) -> Result<Command, String> {
    let Some((name, rest)) = args.split_first() else {
        return Err("missing command".to_string());
    };
    match name.as_str() {
        "update" | "rebuild" | "stats" if !rest.is_empty() => Err(format!("{name} takes no arguments")),
        "update" => Ok(Command::Update),
        "rebuild" => Ok(Command::Rebuild),
        "stats" => Ok(Command::Stats),
        "search" => parse_search(rest).map(Command::Search),
        other => Err(format!("unknown command: {other}")),
    }
}

fn parse_search(args: &[String]) -> Result<SearchArgs, String> {
    let mut parsed = SearchArgs { json: false, limit: DEFAULT_LIMIT, words: Vec::new() };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                i += 1;
                break;
            }
            "--json" => parsed.json = true,
            "--limit" => {
                i += 1;
                parsed.limit = args
                    .get(i)
                    .and_then(|v| v.parse::<usize>().ok())
                    .filter(|&n| n > 0)
                    .ok_or("--limit needs a positive integer")?;
            }
            option if option.starts_with('-') => return Err(format!("unknown option: {option}")),
            _ => break,
        }
        i += 1;
    }
    parsed.words = args[i..].to_vec();
    if parsed.words.is_empty() {
        return Err("search needs at least one word".to_string());
    }
    Ok(parsed)
}

fn paths() -> Result<Paths, Box<dyn Error>> {
    let home = PathBuf::from(env::var_os("HOME").ok_or("HOME is not set")?);
    let root = env::var_os("THREADS_ROOT").map_or_else(|| home.join(".claude/projects"), PathBuf::from);
    let index = env::var_os("THREADS_INDEX")
        .map_or_else(|| home.join("Library/Application Support/thread-search/index.db"), PathBuf::from);
    Ok(Paths { root, index, home })
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

type Writer = fn(&Path, &Path, &Path, &dyn Dirs, i64) -> Result<UpdateReport, Box<dyn Error>>;

fn run(command: Command) -> Result<(), Box<dyn Error>> {
    let paths = paths()?;
    match command {
        Command::Update => run_writer(&paths, update),
        Command::Rebuild => run_writer(&paths, rebuild),
        Command::Stats => stats(&paths),
        Command::Search(args) => run_search(&paths, &args),
    }
}

fn run_writer(paths: &Paths, write: Writer) -> Result<(), Box<dyn Error>> {
    let report = write(&paths.root, &paths.index, &paths.home, &RealDirs, now())?;
    println!("{}", update_line(&report));
    Ok(())
}

fn run_search(paths: &Paths, args: &SearchArgs) -> Result<(), Box<dyn Error>> {
    let now = now();
    let mut result = SearchResult::default();
    let state = match Store::open_existing(&paths.index)? {
        None => State::NoIndex,
        Some(store) => {
            if store.last_update()?.is_none() {
                State::Building
            } else {
                result = search(&store, &parse_query(&args.words), args.limit, now, &paths.home, &RealDirs)?;
                State::Ok
            }
        }
    };
    if args.json {
        println!("{}", json_text(state, &result)?);
    } else {
        print!("{}", human_search(state, &result, now));
    }
    Ok(())
}

fn stats(paths: &Paths) -> Result<(), Box<dyn Error>> {
    let Some(store) = Store::open_existing(&paths.index)? else {
        println!("no index yet: run threads update");
        return Ok(());
    };
    let stats = store.stats()?;
    let bytes = fs::metadata(&paths.index)?.len();
    print!("{}", stats_text(stats.threads, stats.messages, bytes, stats.last_update));
    Ok(())
}
