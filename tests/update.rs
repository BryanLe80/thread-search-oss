mod common;

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, SystemTime};

use common::{Fx, user_record};
use serde_json::json;
use thread_search::store::Store;
use thread_search::update::UpdateReport;

fn messages(fx: &Fx) -> i64 {
    fx.one("select count(*) from messages")
}

fn threads(fx: &Fx) -> i64 {
    fx.one("select count(*) from threads")
}

fn with_text(fx: &Fx, text: &str) -> i64 {
    fx.one(&format!("select count(*) from messages where text = '{text}'"))
}

fn three_kinds(fx: &Fx) {
    fx.thread("widgets", "own").user("fix the widget").assistant("done").write();
    fx.thread("widgets", "desk").desk_job().user("review the pull request").write();
    fx.thread("widgets", "script").scripted().user("one shot prompt").write();
}

#[test]
fn first_update_indexes_own_and_desk_threads_and_skips_scripted() {
    let fx = Fx::new();
    three_kinds(&fx);
    let report = fx.update();
    assert_eq!(report.scanned, 3);
    assert_eq!(report.appended + report.reindexed, 2);
    assert_eq!(report.scripted, 1);
    assert!(!report.busy);
    assert_eq!(threads(&fx), 2);
    assert_eq!(fx.one::<i64>("select desk_job from threads where session_id = 'desk'"), 1);
    assert_eq!(fx.one::<i64>("select desk_job from threads where session_id = 'own'"), 0);
    assert_eq!(with_text(&fx, "one shot prompt"), 0);
}

#[test]
fn subagent_files_are_not_scanned() {
    let fx = Fx::new();
    let own = fx.thread("widgets", "own").user("fix the widget").write();
    let subagents = own.parent().unwrap().join("own").join("subagents");
    fs::create_dir_all(&subagents).unwrap();
    fs::write(subagents.join("agent-1.jsonl"), format!("{}\n", user_record("subagent prompt"))).unwrap();

    let report = fx.update();
    assert_eq!(report.scanned, 1);
    assert_eq!(threads(&fx), 1);
    assert_eq!(with_text(&fx, "subagent prompt"), 0);
}

#[test]
fn file_with_an_empty_stem_is_not_scanned() {
    let fx = Fx::new();
    let nameless = fx.thread("widgets", "").user("no session id").write();
    assert_eq!(nameless.file_name().unwrap(), ".jsonl");
    fx.thread("widgets", "s1").user("has a session id").write();
    let report = fx.update();
    assert_eq!(report.scanned, 1);
    assert_eq!(threads(&fx), 1);
    assert_eq!(with_text(&fx, "no session id"), 0);
    assert_eq!(fx.one::<i64>("select count(*) from threads where session_id = ''"), 0);
}

#[test]
fn second_update_without_change_reads_nothing() {
    let fx = Fx::new();
    three_kinds(&fx);
    fx.update();
    let report = fx.update();
    assert_eq!(report.unchanged, 2);
    assert_eq!(report.appended, 0);
    assert_eq!(report.reindexed, 0);
    assert_eq!(report.scripted, 1);
    assert_eq!(report.scanned, 3);
}

#[test]
fn append_adds_only_new_messages() {
    let fx = Fx::new();
    fx.thread("widgets", "s1").user("first prompt").assistant("first reply").write();
    fx.update();
    assert_eq!(messages(&fx), 2);

    fx.thread("widgets", "s1").user("second prompt").assistant("second reply").write();
    let report = fx.update();
    assert_eq!(report.appended, 1);
    assert_eq!(report.reindexed, 0);
    assert_eq!(messages(&fx), 4);
    assert_eq!(with_text(&fx, "first prompt"), 1);
    assert_eq!(fx.one::<i64>("select prompts from threads"), 2);
}

#[test]
fn cut_line_is_indexed_once_it_is_complete() {
    let fx = Fx::new();
    let line = user_record("cut line words");
    let (head, tail) = line.split_at(20);
    let (middle, tail) = tail.split_at(10);

    fx.thread("widgets", "s1").user("whole line").raw_no_newline(head).write();
    fx.update();
    assert_eq!(messages(&fx), 1);

    fx.thread("widgets", "s1").raw_no_newline(middle).write();
    fx.update();
    assert_eq!(messages(&fx), 1);

    fx.thread("widgets", "s1").raw(tail).write();
    let report = fx.update();
    assert_eq!(report.appended, 1);
    assert_eq!(messages(&fx), 2);
    assert_eq!(with_text(&fx, "cut line words"), 1);
    assert_eq!(fx.update().unchanged, 1);
    assert_eq!(with_text(&fx, "cut line words"), 1);
}

#[test]
fn broken_line_between_good_lines_is_skipped() {
    let fx = Fx::new();
    fx.thread("widgets", "s1").user("good one").raw("{broken").user("good two").write();
    fx.update();
    assert_eq!(messages(&fx), 2);
    assert_eq!(with_text(&fx, "good one"), 1);
    assert_eq!(with_text(&fx, "good two"), 1);
}

#[test]
fn huge_unwanted_line_is_skipped() {
    let fx = Fx::new();
    let blob = format!(r#"{{"type":"api-request-blob","data":"{}"}}"#, "x".repeat(5 << 20));
    fx.thread("widgets", "s1").user("before blob").raw(&blob).user("after blob").write();
    let report = fx.update();
    assert_eq!(report.appended, 1);
    assert_eq!(messages(&fx), 2);
    assert_eq!(with_text(&fx, "after blob"), 1);
}

#[test]
fn shorter_rewrite_reindexes() {
    let fx = Fx::new();
    let path = fx.thread("widgets", "s1").user("old one").user("old two").user("old three").write();
    fx.update();
    fs::remove_file(&path).unwrap();
    fx.thread("widgets", "s1").user("new one").write();

    let report = fx.update();
    assert_eq!(report.reindexed, 1);
    assert_eq!(messages(&fx), 1);
    assert_eq!(with_text(&fx, "old one"), 0);
    assert_eq!(with_text(&fx, "new one"), 1);
    assert_eq!(threads(&fx), 1);
}

#[test]
fn longer_rewrite_with_a_new_start_reindexes() {
    let fx = Fx::new();
    let path = fx.thread("widgets", "s1").user("old first line").write();
    let old_size = fs::metadata(&path).unwrap().len();
    fx.update();
    fs::remove_file(&path).unwrap();
    fx.thread("widgets", "s1").user("new first line").user("new second line").user("new third line").write();
    assert!(fs::metadata(&path).unwrap().len() > old_size);

    let report = fx.update();
    assert_eq!(report.reindexed, 1);
    assert_eq!(report.appended, 0);
    assert_eq!(with_text(&fx, "old first line"), 0);
    assert_eq!(messages(&fx), 3);
    for text in ["new first line", "new second line", "new third line"] {
        assert_eq!(with_text(&fx, text), 1, "{text}");
    }
}

#[test]
fn same_size_rewrite_with_new_mtime_reindexes() {
    let fx = Fx::new();
    let path = fx.thread("widgets", "s1").user("first alpha").write();
    let size = fs::metadata(&path).unwrap().len();
    fx.update();
    fs::remove_file(&path).unwrap();
    fx.thread("widgets", "s1").user("first bravo").write();
    assert_eq!(fs::metadata(&path).unwrap().len(), size);
    let later = SystemTime::now() + Duration::from_secs(60);
    File::options().write(true).open(&path).unwrap().set_modified(later).unwrap();

    let report = fx.update();
    assert_eq!(report.reindexed, 1);
    assert_eq!(with_text(&fx, "first alpha"), 0);
    assert_eq!(with_text(&fx, "first bravo"), 1);
}

#[test]
fn deleted_file_removes_thread() {
    let fx = Fx::new();
    let path = fx.thread("widgets", "s1").user("short lived").write();
    fx.thread("widgets", "s2").user("stays").write();
    fx.update();
    fs::remove_file(&path).unwrap();

    let report = fx.update();
    assert_eq!(report.removed, 1);
    assert_eq!(report.unchanged, 1);
    assert_eq!(threads(&fx), 1);
    assert_eq!(with_text(&fx, "short lived"), 0);
    assert_eq!(fx.one::<i64>("select count(*) from files"), 1);
}

#[test]
fn tool_results_and_thinking_are_not_indexed() {
    let fx = Fx::new();
    let thinking = json!({
        "type": "assistant",
        "message": {"content": [{"type": "thinking", "thinking": "hidden thought"}, {"type": "text", "text": "visible reply"}]},
        "entrypoint": "cli",
    });
    fx.thread("widgets", "s1")
        .user("run the tests")
        .tool_result("tool output text")
        .raw(&thinking.to_string())
        .write();
    fx.update();
    assert_eq!(messages(&fx), 2);
    assert_eq!(with_text(&fx, "visible reply"), 1);
    assert_eq!(fx.one::<i64>("select count(*) from messages where text like '%tool output%'"), 0);
    assert_eq!(fx.one::<i64>("select count(*) from messages where text like '%hidden thought%'"), 0);
}

#[test]
fn compact_summary_is_a_summary_not_a_prompt() {
    let fx = Fx::new();
    fx.thread("widgets", "s1")
        .summary("recap of DEMO-303 and #88")
        .user("typed prompt")
        .write();
    fx.update();
    assert_eq!(
        fx.one::<String>("select role from messages where text = 'recap of DEMO-303 and #88'"),
        "summary"
    );
    assert_eq!(fx.one::<i64>("select prompts from threads"), 1);
    assert_eq!(fx.one::<String>("select first_prompt from threads"), "typed prompt");
    assert_eq!(fx.one::<i64>("select count(*) from keys"), 0);
}

#[test]
fn keys_come_from_title_branch_pr_link_and_prompts() {
    let fx = Fx::new();
    fx.thread("widgets", "s1")
        .branch("dev/demo-202-x")
        .custom_title("Grid filter · DEMO-101")
        .pr(42, "acme/widgets")
        .user("see #77")
        .assistant("DEMO-999 #555")
        .write();
    fx.update();
    assert_eq!(
        fx.strings("select value from keys where kind = 'ticket' order by value"),
        ["DEMO-101", "DEMO-202"]
    );
    assert_eq!(
        fx.strings("select value || ' ' || repo from keys where kind = 'pr' order by value"),
        ["42 acme/widgets", "77 "]
    );
}

#[test]
fn branch_words_with_short_numbers_are_not_tickets() {
    let fx = Fx::new();
    fx.thread("widgets", "s1").branch("feat/add-2-columns").user("plain prompt").write();
    fx.thread("widgets", "s2").branch("dev/abc-4812-fix").user("plain prompt").write();
    fx.update();
    assert_eq!(fx.strings("select value from keys where kind = 'ticket'"), ["ABC-4812"]);
}

#[test]
fn later_cwd_wins() {
    let fx = Fx::new();
    let later = fx.work.join("widgets-api");
    fs::create_dir_all(&later).unwrap();
    let later = later.to_str().unwrap().to_string();
    fx.thread("widgets", "s1").user("start here").cwd(&later).user("move there").write();
    fx.update();
    assert_eq!(fx.one::<String>("select last_cwd from threads"), later);
    assert_eq!(fx.one::<String>("select folder from threads"), later);
    assert_eq!(fx.one::<i64>("select folder_gone from threads"), 0);
}

#[test]
fn removed_cwd_marks_folder_gone_with_nearest_parent() {
    let fx = Fx::new();
    fx.thread("repos/widgets", "s1").user("old work").write();
    fs::remove_dir_all(fx.work.join("repos").join("widgets")).unwrap();
    fx.update();
    assert_eq!(fx.one::<i64>("select folder_gone from threads"), 1);
    let parent = fx.work.join("repos");
    assert_eq!(fx.one::<String>("select folder from threads"), parent.to_str().unwrap());
}

#[test]
fn append_without_cwd_keeps_the_stored_folder() {
    let fx = Fx::new();
    fx.thread("widgets", "s1").user("first").write();
    fx.update();
    fx.thread("widgets", "s1").ai_title("Named later").write();
    let report = fx.update();
    assert_eq!(report.appended, 1);
    assert_eq!(fx.one::<String>("select title from threads"), "Named later");
    let expected = fx.work.join("widgets");
    assert_eq!(fx.one::<String>("select folder from threads"), expected.to_str().unwrap());
    assert_eq!(fx.one::<i64>("select folder_gone from threads"), 0);
}

#[test]
fn busy_when_another_update_holds_the_index() {
    let fx = Fx::new();
    fx.thread("widgets", "s1").user("hello").write();
    let mut holder = Store::open(&fx.index).unwrap();
    let tx = holder.begin_update().unwrap().unwrap();
    assert_eq!(fx.update(), UpdateReport { busy: true, ..Default::default() });
    drop(tx);
    drop(holder);
    assert_eq!(threads(&fx), 0);
}

#[test]
fn busy_when_an_outside_writer_holds_the_database() {
    let fx = Fx::new();
    fx.update();
    fx.thread("widgets", "s1").user("hello").write();
    let outside = rusqlite::Connection::open(&fx.index).unwrap();
    outside.execute_batch("begin immediate").unwrap();
    assert!(fx.update().busy);
    outside.execute_batch("rollback").unwrap();
    assert_eq!(threads(&fx), 0);
    assert_eq!(fx.update().appended, 1);
}

#[test]
fn unreadable_new_file_is_skipped_until_it_can_be_read() {
    let fx = Fx::new();
    let locked = fx.thread("widgets", "s1").user("locked away").write();
    fx.thread("widgets", "s2").user("readable").write();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let report = fx.update();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(report.scanned, 2);
    assert_eq!(report.appended, 1);
    assert_eq!(threads(&fx), 1);

    assert_eq!(fx.update().appended, 1);
    assert_eq!(threads(&fx), 2);
}

#[test]
fn unreadable_file_is_skipped_and_kept() {
    let fx = Fx::new();
    let locked = fx.thread("widgets", "s1").user("locked away").write();
    fx.update();
    fx.thread("widgets", "s1").user("added while locked").write();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let report = fx.update();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!((report.scanned, report.appended, report.reindexed, report.removed), (1, 0, 0, 0));
    assert_eq!(threads(&fx), 1);
    assert_eq!(messages(&fx), 1);
    assert_eq!(with_text(&fx, "locked away"), 1);
    assert_eq!(with_text(&fx, "added while locked"), 0);

    assert_eq!(fx.update().appended, 1);
    assert_eq!(with_text(&fx, "added while locked"), 1);
}

#[test]
fn unreadable_project_folder_keeps_its_threads() {
    let fx = Fx::new();
    let path = fx.thread("widgets", "s1").user("behind a locked folder").write();
    fx.thread("gadgets", "s2").user("in an open folder").write();
    fx.update();
    let project = path.parent().unwrap();
    fs::set_permissions(project, fs::Permissions::from_mode(0o000)).unwrap();
    let report = fx.update();
    fs::set_permissions(project, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(report.removed, 0);
    assert_eq!(report.scanned, 1);
    assert_eq!(threads(&fx), 2);
    assert_eq!(with_text(&fx, "behind a locked folder"), 1);
}

fn file_scripted(fx: &Fx) -> i64 {
    fx.one("select scripted from files")
}

#[test]
fn sdk_entrypoint_appended_to_a_cli_thread_is_indexed() {
    let fx = Fx::new();
    fx.thread("widgets", "s1").user("typed prompt").write();
    fx.update();
    fx.thread("widgets", "s1").scripted().user("resumed by a script").write();
    let report = fx.update();
    assert_eq!(report.scripted, 0);
    assert_eq!(report.appended, 1);
    assert_eq!(threads(&fx), 1);
    assert_eq!(with_text(&fx, "resumed by a script"), 1);
    assert_eq!(file_scripted(&fx), 0);
}

fn index_content(fx: &Fx) -> (i64, i64, i64) {
    (threads(fx), messages(fx), file_scripted(fx))
}

#[test]
fn append_and_rebuild_agree_on_a_later_sdk_entrypoint() {
    let fx = Fx::new();
    fx.thread("widgets", "s1").user("typed prompt").write();
    fx.update();
    fx.thread("widgets", "s1").scripted().user("resumed by a script").write();
    fx.update();
    let by_update = index_content(&fx);
    fx.rebuild();
    assert_eq!(index_content(&fx), by_update);
    assert_eq!(by_update, (1, 2, 0));
}

#[test]
fn sdk_first_entrypoint_stays_scripted_after_a_cli_line() {
    let fx = Fx::new();
    fx.thread("widgets", "s1").scripted().user("one shot prompt").write();
    fx.update();
    fx.thread("widgets", "s1").kind("cli", None).user("typed later").write();
    let report = fx.update();
    assert_eq!(report.scripted, 1);
    assert_eq!(index_content(&fx), (0, 0, 1));
    fx.rebuild();
    assert_eq!(index_content(&fx), (0, 0, 1));
}

#[test]
fn first_entrypoint_in_an_append_decides_when_the_file_had_none() {
    for (entrypoint, expected) in [("sdk-cli", (0, 0, 1)), ("cli", (1, 1, 0))] {
        let fx = Fx::new();
        fx.thread("widgets", "s1").custom_title("Plans for later").write();
        fx.update();
        assert_eq!(index_content(&fx), (1, 0, 0), "{entrypoint}");
        fx.thread("widgets", "s1").kind(entrypoint, None).user("first prompt").write();
        fx.update();
        assert_eq!(index_content(&fx), expected, "{entrypoint}");
        fx.thread("widgets", "s1").scripted().user("second prompt").write();
        fx.update();
        let after = if expected.2 == 1 { expected } else { (1, 2, 0) };
        assert_eq!(index_content(&fx), after, "{entrypoint}");
    }
}

#[test]
fn missing_projects_root_is_an_empty_scan() {
    let fx = Fx::new();
    let missing = fx.work.join("no-such-projects");
    let report = fx.update_root(&missing);
    assert_eq!(report.scanned, 0);
    assert!(!report.busy);
    assert!(fx.index.exists());
}

#[test]
fn rebuild_while_a_store_is_open_is_busy_and_keeps_the_index() {
    let fx = Fx::new();
    fx.thread("widgets", "s1").user("fix the widget").write();
    fx.update();
    let held = Store::open(&fx.index).unwrap();
    let report = fx.rebuild();
    assert!(report.busy);
    assert_eq!(threads(&fx), 1);
    drop(held);

    fx.thread("widgets", "s2").user("a second thread").write();
    let report = fx.rebuild();
    assert!(!report.busy);
    assert_eq!(report.scanned, 2);
    assert_eq!(threads(&fx), 2);
}

#[test]
fn rebuild_drops_rows_of_deleted_transcripts() {
    let fx = Fx::new();
    let gone = fx.thread("widgets", "s1").user("fix the widget").write();
    fx.thread("widgets", "s2").user("keep this one").write();
    fx.update();
    fs::remove_file(gone).unwrap();
    let report = fx.rebuild();
    assert_eq!(report.removed, 0);
    assert_eq!(threads(&fx), 1);
}
