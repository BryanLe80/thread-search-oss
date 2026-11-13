mod common;

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use common::Fx;
use serde_json::Value;

const RESULT_KEYS: [&str; 12] = [
    "agent",
    "session_id",
    "title",
    "last_active",
    "folder",
    "folder_state",
    "folder_name",
    "branch",
    "keys",
    "prompts",
    "desk_job",
    "match",
];

fn command(fx: &Fx) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_threads"));
    command.env("THREADS_ROOT", &fx.root).env("THREADS_INDEX", &fx.index).env("HOME", &fx.work);
    command
}

fn threads(fx: &Fx, args: &[&str]) -> Output {
    command(fx).args(args).output().unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn json(fx: &Fx, args: &[&str]) -> Value {
    let out = threads(fx, args);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = stdout(&out);
    assert!(text.ends_with('\n'));
    serde_json::from_str(&text).unwrap()
}

fn seeded() -> Fx {
    let fx = Fx::new();
    fx.thread("widgets", "s1")
        .custom_title("Retry the upload")
        .user("add a retry to DEMO-101 uploads")
        .assistant("done, the retry backs off")
        .write();
    fx.thread("widgets", "s2").user("retry the other job").assistant("ok").write();
    fx.thread("widgets", "s3").user("--json retry flag notes").write();
    let out = threads(&fx, &["update"]);
    assert_eq!(out.status.code(), Some(0));
    fx
}

#[test]
fn search_without_an_index_reports_no_index() {
    let fx = Fx::new();
    let v = json(&fx, &["search", "--json", "x"]);
    assert_eq!(v["state"], "no_index");
    assert_eq!(v["matched_all"], 0);
    assert_eq!(v["matched_some"], 0);
    assert_eq!(v["results"], Value::Array(vec![]));
    assert!(!fx.index.exists());
}

#[test]
fn search_json_result_has_the_contract_keys() {
    let fx = seeded();
    let text = stdout(&threads(&fx, &["search", "--json", "retry"]));
    let at = |key: &str| text.find(&format!("\"{key}\":")).unwrap();
    assert!(at("state") < at("matched_all") && at("matched_all") < at("matched_some"));
    assert!(at("matched_some") < at("results"));
    let v = json(&fx, &["search", "--json", "retry"]);
    assert_eq!(v["state"], "ok");
    let results = v["results"].as_array().unwrap();
    assert!(!results.is_empty());
    for r in results {
        let mut got: Vec<&str> = r.as_object().unwrap().keys().map(String::as_str).collect();
        got.sort_unstable();
        let mut want = RESULT_KEYS.to_vec();
        want.sort_unstable();
        assert_eq!(got, want);
        assert!(r["folder_state"] == "ok" || r["folder_state"] == "gone");
        assert!(r["keys"].is_array());
        assert!(r["prompts"].is_u64());
        assert!(r["desk_job"].is_boolean());
        assert!(r["last_active"].is_i64());
        assert!(r["branch"].is_string() || r["branch"].is_null());
        assert!(r["match"]["role"].is_string());
        let segments = r["match"]["segments"].as_array().unwrap();
        assert!(!segments.is_empty());
        assert!(segments.iter().all(|s| s["text"].is_string() && s["hit"].is_boolean()));
    }
}

#[test]
fn limit_caps_the_results_and_options_may_come_in_any_order() {
    let fx = seeded();
    for args in [
        ["search", "--json", "--limit", "1", "retry"],
        ["search", "--limit", "1", "--json", "retry"],
    ] {
        let v = json(&fx, &args);
        assert_eq!(v["results"].as_array().unwrap().len(), 1);
        assert!(v["matched_all"].as_u64().unwrap() >= 2);
    }
}

#[test]
fn double_dash_makes_the_rest_query_text() {
    let fx = seeded();
    let v = json(&fx, &["search", "--json", "--", "--json retry"]);
    assert_eq!(v["state"], "ok");
    let results = v["results"].as_array().unwrap();
    assert_eq!(v["matched_all"], 1);
    assert_eq!(results[0]["session_id"], "s3");
}

#[test]
fn usage_errors_exit_2_with_a_message_on_stderr() {
    let fx = seeded();
    for args in [
        &[][..],
        &["search"],
        &["search", "--json"],
        &["bogus"],
        &["search", "--bogus", "retry"],
        &["search", "--limit", "retry"],
        &["search", "--limit", "0", "retry"],
        &["search", "--limit"],
        &["update", "x"],
        &["rebuild", "x"],
        &["stats", "x"],
    ] {
        let out = threads(&fx, args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(!out.stderr.is_empty(), "{args:?}");
    }
}

#[test]
fn non_utf8_argument_is_a_usage_error_not_a_panic() {
    let fx = seeded();
    let out = command(&fx).arg("search").arg(OsStr::from_bytes(b"re\xFFtry")).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("usage:") && !stderr.contains("panicked"), "{stderr}");
}

#[test]
fn missing_home_is_an_error() {
    let fx = Fx::new();
    for args in [["stats"], ["update"], ["rebuild"]] {
        let out = command(&fx)
            .args(args)
            .env_remove("HOME")
            .env_remove("THREADS_ROOT")
            .env_remove("THREADS_INDEX")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("HOME is not set"), "{args:?}: {stderr}");
    }
}

#[test]
fn blank_query_text_is_ok_with_no_results() {
    let fx = seeded();
    for query in ["", "   "] {
        let v = json(&fx, &["search", "--json", "--", query]);
        assert_eq!(v["state"], "ok");
        assert_eq!(v["matched_all"], 0);
        assert_eq!(v["results"], Value::Array(vec![]));
    }
}

#[test]
fn index_without_a_last_update_is_building() {
    let fx = seeded();
    let conn = rusqlite::Connection::open(&fx.index).unwrap();
    conn.execute("delete from meta where key = 'last_update'", []).unwrap();
    drop(conn);

    let v = json(&fx, &["search", "--json", "retry"]);
    assert_eq!(v["state"], "building");
    assert_eq!(v["matched_all"], 0);
    assert_eq!(v["matched_some"], 0);
    assert_eq!(v["results"], Value::Array(vec![]));
    assert_eq!(stdout(&threads(&fx, &["search", "retry"])), "index is building\n");
}

#[test]
fn update_prints_one_report_line() {
    let fx = Fx::new();
    fx.thread("widgets", "s1").user("hello there").write();
    let out = threads(&fx, &["update"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "scanned 1, unchanged 0, appended 1, reindexed 0, removed 0, scripted 0\n");
    let out = threads(&fx, &["update"]);
    assert_eq!(stdout(&out), "scanned 1, unchanged 1, appended 0, reindexed 0, removed 0, scripted 0\n");
}

#[test]
fn update_with_a_missing_root_still_creates_the_index() {
    let fx = Fx::new();
    fs::remove_dir_all(&fx.root).unwrap();
    let out = threads(&fx, &["update"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).starts_with("scanned 0, "));
    assert!(fx.index.exists());
}

#[test]
fn held_write_transaction_does_not_block_update_or_search() {
    let fx = seeded();
    let conn = rusqlite::Connection::open(&fx.index).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();

    let started = Instant::now();
    let out = threads(&fx, &["update"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "busy: another update runs\n");
    assert!(started.elapsed() < Duration::from_secs(1), "update took {:?}", started.elapsed());

    let v = json(&fx, &["search", "--json", "retry"]);
    assert_eq!(v["state"], "ok");
    assert!(!v["results"].as_array().unwrap().is_empty());
    conn.execute_batch("ROLLBACK").unwrap();
}

fn bad_file(fx: &Fx) -> std::path::PathBuf {
    let mut name = fx.index.as_os_str().to_owned();
    name.push(".bad");
    name.into()
}

#[test]
fn rebuild_sets_a_garbage_index_aside_and_builds_a_new_one() {
    let fx = seeded();
    let garbage = vec![0x42u8; 8192];
    fs::write(&fx.index, &garbage).unwrap();
    let out = threads(&fx, &["rebuild"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout(&out).starts_with("scanned 3, "));
    assert!(fs::read(bad_file(&fx)).unwrap_or_default() == garbage, ".bad must hold the old bytes");
    let v = json(&fx, &["search", "--json", "retry"]);
    assert_eq!(v["state"], "ok");
    assert!(!v["results"].as_array().unwrap().is_empty());
}

#[test]
fn update_and_rebuild_refuse_a_database_of_another_app() {
    for schema in [
        "create table notes(body text); insert into notes values('keep me');",
        "create table meta(key text, value text); insert into meta values('version', '7');
         create table urls(id integer primary key, url text); insert into urls(url) values('keep me');",
    ] {
        let fx = Fx::new();
        fx.thread("widgets", "s1").user("hello there").write();
        fs::create_dir_all(fx.index.parent().unwrap()).unwrap();
        rusqlite::Connection::open(&fx.index).unwrap().execute_batch(schema).unwrap();
        let before = fs::read(&fx.index).unwrap();
        for command in ["update", "rebuild"] {
            let out = threads(&fx, &[command]);
            assert_eq!(out.status.code(), Some(1), "{command}: {schema}");
            assert!(out.stdout.is_empty(), "{command}");
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(stderr.contains("not a thread-search index"), "{command}: {stderr}");
            assert!(fs::read(&fx.index).unwrap() == before, "{command} changed the file: {schema}");
        }
        assert!(!bad_file(&fx).exists());
    }
}

#[test]
fn rebuild_while_another_update_runs_deletes_nothing() {
    let fx = seeded();
    let _held = thread_search::store::Store::open(&fx.index).unwrap();
    let out = threads(&fx, &["rebuild"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "busy: another update runs\n");
    assert!(fx.index.exists());
}

#[test]
fn human_search_prints_the_resume_command() {
    let fx = seeded();
    let out = threads(&fx, &["search", "retry"]);
    assert_eq!(out.status.code(), Some(0));
    let text = stdout(&out);
    assert!(text.contains("claude --resume s1"), "{text}");
    assert!(text.starts_with("3 threads match all words, 0 match some\n"), "{text}");
    assert!(text.contains("keys: DEMO-101"), "{text}");
    assert!(text.contains("[retry]"), "{text}");
}

#[test]
fn human_search_shows_the_branch_and_singular_counts() {
    let fx = Fx::new();
    fx.thread("widgets", "b1").branch("feat/upload-retry").user("one lonely prompt").write();
    threads(&fx, &["update"]);
    let text = stdout(&threads(&fx, &["search", "lonely"]));
    assert!(text.starts_with("1 thread matches all words, 0 match some\n"), "{text}");
    assert!(text.contains(" · widgets · feat/upload-retry · 1 prompt\n"), "{text}");
}

#[test]
fn deleted_folder_is_reported_gone() {
    let fx = Fx::new();
    fx.thread("widgets/feature", "g").user("cleanup task").write();
    fs::remove_dir_all(fx.work.join("widgets").join("feature")).unwrap();
    threads(&fx, &["update"]);
    let v = json(&fx, &["search", "--json", "cleanup"]);
    let hit = &v["results"][0];
    assert_eq!(hit["folder_state"], "gone");
    assert_eq!(hit["folder"], fx.work.join("widgets").to_str().unwrap());
    let text = stdout(&threads(&fx, &["search", "cleanup"]));
    assert!(text.contains(" · folder gone · "), "{text}");
}

#[test]
fn human_search_states() {
    let fx = Fx::new();
    assert_eq!(stdout(&threads(&fx, &["search", "retry"])), "no index yet: run threads update\n");
    let fx = seeded();
    assert_eq!(stdout(&threads(&fx, &["search", "zzzzqqq"])), "no thread matches\n");
}

#[test]
fn stats_reports_counts_and_handles_a_missing_index() {
    let fx = Fx::new();
    let out = threads(&fx, &["stats"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "no index yet: run threads update\n");

    let fx = seeded();
    let text = stdout(&threads(&fx, &["stats"]));
    assert!(text.contains("threads: 3"), "{text}");
    assert!(text.contains("messages: 5"), "{text}");
    assert!(text.contains("index size: "), "{text}");
    assert!(text.contains("last update: "), "{text}");
    assert!(!text.contains("never"), "{text}");
}
