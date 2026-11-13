mod common;

use std::fs::{self, File};
use std::time::{Duration, UNIX_EPOCH};

use common::{Fx, strings};
use thread_search::folder::slug;
use thread_search::search::{Hit, SearchResult, Term, parse_query};

const TWO_DAYS_AGO: &str = "2026-11-08T12:00:00.000Z";
const TEN_DAYS_AGO: &str = "2026-10-31T12:00:00.000Z";
const SIXTY_DAYS_AGO: &str = "2026-09-11T12:00:00.000Z";

fn sessions(result: &SearchResult) -> Vec<&str> {
    result.results.iter().map(|h| h.session_id.as_str()).collect()
}

fn first(result: &SearchResult) -> &Hit {
    result.results.first().expect("at least one hit")
}

fn term(text: &str, quoted: bool) -> Term {
    Term { text: text.to_string(), quoted }
}

fn retry_threads(fx: &Fx) {
    fx.thread("widgets", "a").user("retry queue drops webhooks").write();
    fx.thread("widgets", "b").user("retry later").write();
    fx.update();
}

#[test]
fn parse_query_keeps_quoted_groups_and_drops_symbols() {
    assert_eq!(
        parse_query(&strings(&["fix", "\"retry", "queue\"", "--"])),
        vec![term("fix", false), term("retry queue", true)]
    );
    assert_eq!(
        parse_query(&strings(&["fix \"retry queue\" now"])),
        vec![term("fix", false), term("retry queue", true), term("now", false)]
    );
}

#[test]
fn parse_query_takes_an_unclosed_quote_to_the_end() {
    assert_eq!(
        parse_query(&strings(&["fix \"retry  queue"])),
        vec![term("fix", false), term("retry queue", true)]
    );
    assert!(parse_query(&strings(&["\"", "-", "*", "("])).is_empty());
}

#[test]
fn thread_matching_every_term_comes_first() {
    let fx = Fx::new();
    retry_threads(&fx);
    let result = fx.search(&["retry", "webhooks"]);
    assert_eq!(sessions(&result), ["a", "b"]);
    assert_eq!(result.matched_all, 1);
    assert_eq!(result.matched_some, 1);
}

fn title_and_body_threads(fx: &Fx) {
    fx.thread("widgets", "c").custom_title("Webhook retries").user("set up the hooks").write();
    fx.thread("widgets", "d")
        .user("what happens next")
        .assistant("retries happen when a webhook fails")
        .write();
    fx.update();
}

#[test]
fn title_match_beats_body_match() {
    let fx = Fx::new();
    title_and_body_threads(&fx);
    let result = fx.search(&["webhook"]);
    assert_eq!(sessions(&result), ["c", "d"]);
}

#[test]
fn prefix_matches_but_a_quoted_term_is_not_a_prefix() {
    let fx = Fx::new();
    retry_threads(&fx);
    assert!(fx.search(&["retries"]).results.is_empty());
    assert_eq!(sessions(&fx.search(&["webh"])), ["a"]);
    let quoted = fx.search(&["\"webh\""]);
    assert!(quoted.results.is_empty());
    assert_eq!(quoted.matched_all + quoted.matched_some, 0);
}

#[test]
fn last_word_finds_a_longer_word_by_its_prefix() {
    let fx = Fx::new();
    fx.thread("widgets", "d").user("the deployment failed").write();
    fx.thread("widgets", "o").user("other words").write();
    fx.update();
    for word in ["deploy", "deploymen", "deployment"] {
        assert_eq!(sessions(&fx.search(&[word])), ["d"], "{word}");
    }
}

#[test]
fn a_word_before_the_last_matches_by_its_prefix_but_is_not_stemmed() {
    let fx = Fx::new();
    fx.thread("widgets", "r").user("retries now").write();
    fx.thread("widgets", "s").user("retry now").write();
    fx.update();
    let result = fx.search(&["retr", "now"]);
    assert_eq!((result.matched_all, result.matched_some), (2, 0));
    let result = fx.search(&["retry", "now"]);
    assert_eq!(sessions(&result), ["s", "r"]);
    assert_eq!((result.matched_all, result.matched_some), (1, 1));
}

fn deployment_threads(fx: &Fx) {
    fx.thread("widgets", "d").user("the deployment runs now").write();
    fx.thread("widgets", "x").user("nothing here yet").write();
    fx.update();
}

#[test]
fn a_partial_first_word_matches_with_a_full_last_word() {
    let fx = Fx::new();
    deployment_threads(&fx);
    let result = fx.search(&["deplo", "now"]);
    assert_eq!(sessions(&result), ["d"]);
    assert_eq!((result.matched_all, result.matched_some), (1, 0));
}

#[test]
fn every_partial_word_matches_by_its_prefix() {
    let fx = Fx::new();
    deployment_threads(&fx);
    let result = fx.search(&["deplo", "run", "no"]);
    assert_eq!(sessions(&result), ["d", "x"]);
    assert_eq!((result.matched_all, result.matched_some), (1, 1));
}

#[test]
fn a_three_letter_earlier_word_matches_by_its_prefix() {
    let fx = Fx::new();
    deployment_threads(&fx);
    let result = fx.search(&["dep", "now"]);
    assert_eq!(sessions(&result), ["d"]);
    assert_eq!((result.matched_all, result.matched_some), (1, 0));
}

#[test]
fn a_one_or_two_letter_earlier_word_matches_a_whole_word_only() {
    let fx = Fx::new();
    deployment_threads(&fx);
    for word in ["d", "de"] {
        let result = fx.search(&[word, "now"]);
        assert_eq!((result.matched_all, result.matched_some), (0, 1), "{word}");
    }
    fx.thread("widgets", "today").user("today we ship").write();
    fx.thread("widgets", "to").user("time to ship").write();
    fx.update();
    let result = fx.search(&["to", "ship"]);
    assert_eq!(sessions(&result), ["to", "today"]);
    assert_eq!((result.matched_all, result.matched_some), (1, 1));
}

#[test]
fn a_one_letter_last_word_is_still_a_prefix() {
    let fx = Fx::new();
    deployment_threads(&fx);
    let result = fx.search(&["now", "d"]);
    assert_eq!(sessions(&result), ["d"]);
    assert_eq!((result.matched_all, result.matched_some), (1, 0));
}

#[test]
fn a_query_of_four_words_keeps_the_earlier_prefixes() {
    let fx = Fx::new();
    fx.thread("widgets", "d").user("the deployment runs right now").write();
    fx.update();
    let result = fx.search(&["deplo", "run", "rig", "now"]);
    assert_eq!((result.matched_all, result.matched_some), (1, 0));
}

#[test]
fn a_query_of_five_words_matches_the_earlier_words_whole() {
    let fx = Fx::new();
    fx.thread("widgets", "d").user("the deployment runs right now").write();
    fx.update();
    let partial = fx.search(&["deplo", "run", "the", "rig", "now"]);
    assert_eq!((partial.matched_all, partial.matched_some), (0, 1));
    let whole = fx.search(&["deployment", "runs", "the", "right", "no"]);
    assert_eq!((whole.matched_all, whole.matched_some), (1, 0));
}

#[test]
fn an_adjacent_prefix_phrase_ranks_first() {
    let fx = Fx::new();
    fx.thread("widgets", "far").user("now the deployment works").assistant("deployment again").write();
    fx.thread("widgets", "adjacent").user("the deployment now works").write();
    fx.update();
    let result = fx.search(&["deplo", "now"]);
    assert_eq!(sessions(&result), ["adjacent", "far"]);
    assert_eq!(result.matched_all, 2);
}

#[test]
fn every_word_in_one_message_ranks_first() {
    let fx = Fx::new();
    fx.thread("widgets", "split").user("deployment runs").assistant("now later").assistant("now again").write();
    fx.thread("widgets", "one").user("deployment runs now").write();
    fx.update();
    let result = fx.search(&["deplo", "now"]);
    assert_eq!(sessions(&result), ["one", "split"]);
    assert_eq!(result.matched_all, 2);
}

#[test]
fn exact_words_rank_above_a_newer_thread_with_longer_words() {
    let fx = Fx::new();
    fx.thread("widgets", "exact").at(SIXTY_DAYS_AGO).user("deploy the fix").write();
    fx.thread("widgets", "longer").at(TWO_DAYS_AGO).user("deployment got fixed").write();
    fx.update();
    let result = fx.search(&["deploy", "fix"]);
    assert_eq!(sessions(&result), ["exact", "longer"]);
    assert_eq!(result.matched_all, 2);
}

#[test]
fn with_the_same_exact_words_the_newer_thread_ranks_first() {
    let fx = Fx::new();
    fx.thread("widgets", "old").at(SIXTY_DAYS_AGO).user("deploy the fix").write();
    fx.thread("widgets", "new").at(TWO_DAYS_AGO).user("deploy the fix").write();
    fx.update();
    assert_eq!(sessions(&fx.search(&["deploy", "fix"])), ["new", "old"]);
}

#[test]
fn one_exact_word_of_two_beats_none() {
    let fx = Fx::new();
    fx.thread("widgets", "one").at(SIXTY_DAYS_AGO).user("deploy got fixed").write();
    fx.thread("widgets", "none").at(TWO_DAYS_AGO).user("deployment got fixed").write();
    fx.update();
    assert_eq!(sessions(&fx.search(&["deploy", "fix"])), ["one", "none"]);
}

#[test]
fn all_words_through_longer_words_rank_above_one_exact_word() {
    let fx = Fx::new();
    fx.thread("widgets", "all").at(SIXTY_DAYS_AGO).user("deployment got fixed").write();
    fx.thread("widgets", "some").at(TWO_DAYS_AGO).user("deploy only").write();
    fx.update();
    let result = fx.search(&["deploy", "fix"]);
    assert_eq!(sessions(&result), ["all", "some"]);
    assert_eq!((result.matched_all, result.matched_some), (1, 1));
}

#[test]
fn a_long_query_ranks_exact_words_first_too() {
    let fx = Fx::new();
    fx.thread("widgets", "exact").at(SIXTY_DAYS_AGO).user("alpha beta gamma delta deploy").write();
    fx.thread("widgets", "longer").at(TWO_DAYS_AGO).user("alpha beta gamma delta deployment").write();
    fx.thread("widgets", "branch").at(TWO_DAYS_AGO).branch("alpha").user("beta gamma delta deploy").write();
    fx.update();
    let result = fx.search(&["alpha", "beta", "gamma", "delta", "deploy"]);
    assert_eq!(sessions(&result), ["exact", "longer", "branch"]);
    assert_eq!(result.matched_all, 3);
}

#[test]
fn an_exact_word_in_the_title_ranks_first() {
    let fx = Fx::new();
    fx.thread("widgets", "exact").at(SIXTY_DAYS_AGO).custom_title("Deploy").user("set up the hooks").write();
    fx.thread("widgets", "longer").at(TWO_DAYS_AGO).custom_title("Deployment").user("set up the hooks").write();
    fx.update();
    assert_eq!(sessions(&fx.search(&["deploy"])), ["exact", "longer"]);
}

/// Unrelated threads, so that a word held by one or two threads is rare in the index.
fn fillers(fx: &Fx, count: usize) {
    for i in 0..count {
        fx.thread("widgets", &format!("filler-{i}")).user("plain words here").write();
    }
}

/// `message` (old) holds `gizmo` in its chat; `branch` (new) holds it only in a long branch name.
/// Without an exact-word factor the newer `branch` ranks first; one factor for `message` turns it.
fn message_and_branch(fx: &Fx) {
    fillers(fx, 20);
    fx.thread("widgets", "message")
        .at(SIXTY_DAYS_AGO)
        .user("gizmo one")
        .user("gizmo two")
        .user("gizmo three")
        .user("alpha beta gamma delta")
        .write();
    fx.thread("widgets", "branch")
        .at(TWO_DAYS_AGO)
        .branch("gizmo-a-b-c-d-e-f-g-h")
        .user("one two three")
        .user("alpha beta gamma delta")
        .write();
    fx.update();
}

#[test]
fn a_quoted_word_gets_no_exact_factor() {
    let fx = Fx::new();
    message_and_branch(&fx);
    assert_eq!(sessions(&fx.search(&["gizmo"]))[..2], ["message", "branch"]);
    assert_eq!(sessions(&fx.search(&["\"gizmo\""]))[..2], ["branch", "message"]);
    let long = fx.search(&["\"gizmo\"", "alpha", "beta", "gamma", "delta"]);
    assert_eq!(sessions(&long)[..2], ["branch", "message"]);
}

#[test]
fn a_partial_last_word_adds_no_exact_bonus() {
    let fx = Fx::new();
    message_and_branch(&fx);
    let result = fx.search(&["alpha", "gizm"]);
    assert_eq!(sessions(&result)[..2], ["branch", "message"]);
    assert_eq!(result.matched_all, 2);
}

#[test]
fn a_whole_word_only_in_the_title_is_exact_in_a_long_query() {
    let fx = Fx::new();
    fillers(&fx, 20);
    fx.thread("widgets", "title")
        .at(SIXTY_DAYS_AGO)
        .custom_title("gizmo notes for the long planning session today")
        .user("alpha beta gamma delta")
        .write();
    fx.thread("widgets", "branch")
        .at(TWO_DAYS_AGO)
        .branch("gizmo")
        .user("alpha beta gamma delta")
        .user("alpha beta gamma delta")
        .user("alpha beta gamma delta")
        .write();
    fx.update();
    let result = fx.search(&["gizmo", "alpha", "beta", "gamma", "delta"]);
    assert_eq!(sessions(&result)[..2], ["title", "branch"]);
}

#[test]
fn a_desk_job_title_gives_no_exact_factor() {
    let fx = Fx::new();
    fx.thread("widgets", "desk-titled").desk_job().at(SIXTY_DAYS_AGO).ai_title("deploy").user("alpha").write();
    fx.thread("widgets", "desk-other").desk_job().at(TWO_DAYS_AGO).ai_title("other").user("alpha").write();
    fx.update();
    let result = fx.search(&["alpha", "deploy"]);
    assert_eq!(sessions(&result), ["desk-other", "desk-titled"]);
}

#[test]
fn a_key_from_the_branch_counts_as_an_exact_word() {
    let fx = Fx::new();
    fillers(&fx, 20);
    fx.thread("widgets", "key").branch("feat/DEMO-101-x").user("set up the hooks").write();
    fx.thread("widgets", "named")
        .custom_title("notes on demo 101 and the long planning session for today with many more words in it")
        .user("set up the hooks")
        .assistant("demo 101 is open")
        .assistant("demo 101 again")
        .assistant("demo 101 once more")
        .write();
    fx.update();
    assert_eq!(sessions(&fx.search(&["DEMO-101"]))[..2], ["key", "named"]);
}

#[test]
fn an_earlier_word_counts_characters_not_bytes() {
    let fx = Fx::new();
    fx.thread("widgets", "summers").user("étés maintenant").write();
    fx.thread("widgets", "and").user("et maintenant").write();
    fx.update();
    let three = fx.search(&["été", "maintenant"]);
    assert_eq!(sessions(&three), ["summers", "and"]);
    assert_eq!((three.matched_all, three.matched_some), (1, 1));
    let two = fx.search(&["ét", "maintenant"]);
    assert_eq!(sessions(&two), ["and", "summers"]);
    assert_eq!((two.matched_all, two.matched_some), (1, 1));
}

#[test]
fn an_earlier_word_outside_latin_letters_matches_whole() {
    let fx = Fx::new();
    fx.thread("widgets", "home").user("домохозяйка сейчас").write();
    fx.update();
    let result = fx.search(&["домох", "сейчас"]);
    assert_eq!((result.matched_all, result.matched_some), (0, 1));
}

#[test]
fn a_quoted_group_stays_an_exact_phrase() {
    let fx = Fx::new();
    deployment_threads(&fx);
    let result = fx.search(&["\"deplo runs\""]);
    assert_eq!(result.matched_all + result.matched_some, 0);
    let result = fx.search(&["\"deployment runs\"", "no"]);
    assert_eq!(sessions(&result), ["d", "x"]);
    assert_eq!((result.matched_all, result.matched_some), (1, 1));
}

#[test]
fn a_partial_word_matches_only_in_the_title() {
    let fx = Fx::new();
    fx.thread("widgets", "c").custom_title("Deployment checklist").user("set up the hooks").write();
    fx.thread("widgets", "h").user("the hooks again").write();
    fx.update();
    let result = fx.search(&["deplo", "hooks"]);
    assert_eq!(sessions(&result), ["c", "h"]);
    assert_eq!((result.matched_all, result.matched_some), (1, 1));
}

#[test]
fn a_partial_word_matches_in_the_branch_and_the_folder_name() {
    let fx = Fx::new();
    fx.thread("gadgets", "g").branch("feat/turbo").user("tune the cache").write();
    fx.thread("widgets", "w").user("tune the cache").write();
    fx.update();
    for word in ["gadg", "tur"] {
        let result = fx.search(&[word, "cache"]);
        assert_eq!(sessions(&result), ["g", "w"], "{word}");
        assert_eq!((result.matched_all, result.matched_some), (1, 1), "{word}");
    }
}

#[test]
fn a_ticket_key_typed_in_part_gets_no_key_boost() {
    let fx = Fx::new();
    fx.thread("widgets", "k").branch("feat/DEMO-1012-x").user("fix the form").write();
    fx.thread("widgets", "m").user("fix it").assistant("demo 1012 is the code").write();
    fx.update();
    let full = fx.search(&["demo-1012", "fix"]);
    assert_eq!(sessions(&full), ["k", "m"]);
    let part = fx.search(&["demo-101", "fix"]);
    assert_eq!(sessions(&part), ["m", "k"]);
    assert_eq!((part.matched_all, part.matched_some), (2, 0));
}

#[test]
fn segments_mark_the_whole_word_that_an_earlier_partial_word_matched() {
    let fx = Fx::new();
    deployment_threads(&fx);
    let result = fx.search(&["deplo", "no"]);
    let hit = first(&result);
    assert_eq!(hit.session_id, "d");
    let marked: Vec<&str> = hit.segments.iter().filter(|s| s.hit).map(|s| s.text.as_str()).collect();
    assert_eq!(marked, ["deployment", "now"]);
}

#[test]
fn exact_phrase_ranks_above_scattered_words() {
    let fx = Fx::new();
    fx.thread("widgets", "f").user("queue the retry").write();
    fx.thread("widgets", "e").user("the retry queue is full").write();
    fx.update();
    let result = fx.search(&["retry", "queue"]);
    assert_eq!(sessions(&result), ["e", "f"]);
    assert_eq!(result.matched_all, 2);
}

#[test]
fn ticket_key_from_the_branch_finds_only_that_thread() {
    let fx = Fx::new();
    fx.thread("widgets", "k").branch("feat/DEMO-101-login").user("fix the login form").write();
    fx.thread("widgets", "l").user("the answer is 101").write();
    fx.update();
    let result = fx.search(&["demo-101"]);
    assert_eq!(sessions(&result), ["k"]);
    assert_eq!(result.matched_all, 1);
    assert_eq!(first(&result).keys, ["DEMO-101"]);
}

/// Thread `p` holds PR 42 of `widgets`; thread `q` only mentions the number.
fn assert_pr_forms_find_p_first(fx: &Fx) {
    fx.thread("widgets", "q").user("see issue 42 in the tracker").write();
    fx.update();
    for word in ["#42", "42", "widgets#42", "PR#42", "pr#42"] {
        let result = fx.search(&[word]);
        assert_eq!(first(&result).session_id, "p", "{word}");
        assert_eq!(first(&result).keys, ["#42"], "{word}");
    }
}

#[test]
fn pr_number_forms_find_the_pr_thread_first() {
    let fx = Fx::new();
    fx.thread("widgets", "p").pr(42, "acme/widgets").user("ship the widget fix").write();
    assert_pr_forms_find_p_first(&fx);
    let other = fx.search(&["other#42"]);
    assert!(!sessions(&other).contains(&"p"));
}

#[test]
fn repo_hash_number_in_a_title_finds_that_thread_first() {
    let fx = Fx::new();
    fx.thread("widgets", "p").custom_title("work widgets#42").user("ship the widget fix").write();
    assert_pr_forms_find_p_first(&fx);
}

#[test]
fn owner_and_repo_query_finds_a_bare_repo_key() {
    let fx = Fx::new();
    fx.thread("widgets", "o").user("ship orca#7 today").write();
    fx.thread("widgets", "n").user("issue 7 is elsewhere").write();
    fx.thread("widgets", "other").user("see other#7").write();
    fx.thread("widgets", "suffix").user("see rca#7").write();
    fx.update();
    let result = fx.search(&["acme/orca#7"]);
    assert_eq!(sessions(&result), ["o"]);
    assert_eq!(first(&result).keys, ["#7"]);
}

#[test]
fn stored_gone_folder_that_came_back_is_ok_again() {
    let fx = Fx::new();
    fx.thread("widgets/feature", "g").user("cleanup task").write();
    let feature = fx.work.join("widgets").join("feature");
    fs::remove_dir_all(&feature).unwrap();
    fx.update();
    assert_eq!(fx.one::<i64>("select folder_gone from threads"), 1);
    fs::create_dir_all(&feature).unwrap();
    let result = fx.search(&["cleanup"]);
    let hit = first(&result);
    assert!(!hit.folder_gone);
    assert_eq!(hit.folder, feature.to_str().unwrap());
    assert_eq!(hit.folder_name, "feature");
}

#[test]
fn thread_without_a_timestamp_shows_the_file_time() {
    let fx = Fx::new();
    let path = fx.thread("widgets", "t").at("not a time").user("timeless words").write();
    let modified = UNIX_EPOCH + Duration::from_secs((common::NOW - 3600) as u64);
    File::options().write(true).open(&path).unwrap().set_modified(modified).unwrap();
    fx.update();
    let result = fx.search(&["timeless"]);
    assert_eq!(first(&result).last_active, common::NOW - 3600);
}

#[test]
fn thread_without_a_timestamp_ranks_by_the_file_time() {
    let fx = Fx::new();
    fx.thread("widgets", "old").at(SIXTY_DAYS_AGO).user("timeless words").write();
    let path = fx.thread("widgets", "undated").at("not a time").user("timeless words").write();
    let modified = UNIX_EPOCH + Duration::from_secs((common::NOW - 3600) as u64);
    File::options().write(true).open(&path).unwrap().set_modified(modified).unwrap();
    fx.update();
    let result = fx.search(&["timeless"]);
    assert_eq!(sessions(&result), ["undated", "old"]);
}

#[test]
fn branch_only_match_ranks_below_a_message_match() {
    let fx = Fx::new();
    fx.thread("widgets", "branch").branch("gizmo").user("tune the cache").write();
    fx.thread("widgets", "chat").branch("main").user("the gizmo is loose").write();
    fx.update();
    let result = fx.search(&["gizmo"]);
    assert_eq!(sessions(&result), ["chat", "branch"]);
    assert_eq!(result.matched_all, 2);
}

#[test]
fn folder_name_matches_through_place() {
    let fx = Fx::new();
    fx.thread("my_app", "r").user("fix the login form").write();
    fx.thread("widgets", "w").user("fix the login form").write();
    fx.update();
    let result = fx.search(&["my_app"]);
    assert_eq!(sessions(&result), ["r"]);
    assert_eq!(first(&result).folder_name, "my_app");
}

#[test]
fn operator_characters_are_plain_text() {
    let fx = Fx::new();
    fx.thread("widgets", "s").user("foo bar baz").write();
    fx.update();
    for word in ["\"", "-", "foo-bar:", "AND", "(", "*"] {
        fx.search(&[word]);
    }
    assert_eq!(sessions(&fx.search(&["foo-bar:"])), ["s"]);
}

#[test]
fn hostile_terms_that_reach_fts5_never_fail() {
    let fx = Fx::new();
    fx.thread("widgets", "s").user("retry foo bar").write();
    fx.update();
    let long = "a".repeat(10_000);
    let hostile = [
        "NEAR(retry",
        "{text}:x",
        "^foo",
        "say\"hi",
        "a*b",
        "(retry)",
        "retry OR",
        "NOT retry",
        long.as_str(),
        "\u{05D0}\u{05B0}",
        "\u{05B0}",
    ];
    assert_eq!(parse_query(&strings(&["\u{05B0}"])), [term("\u{05B0}", false)]);
    for text in hostile {
        for quoted in [false, true] {
            let terms = [term(text, quoted), term("retry", false)];
            for terms in [&terms[..1], &terms[..]] {
                fx.search_terms(terms, 20);
            }
        }
        fx.search(&[text]);
        fx.search(&["retry", text]);
    }
}

#[test]
fn search_stays_ok_while_an_update_rewrites_threads() {
    let fx = Fx::new();
    let write_all = |round: usize| {
        for name in ["a", "b", "c"] {
            fx.thread("widgets", name).user(&format!("retry queue round {round} {name}")).write();
        }
    };
    write_all(0);
    fx.update();
    let project = fx.root.join(slug(fx.work.join("widgets").to_str().unwrap()));
    let terms = [term("retry", false), term("queue", false)];
    std::thread::scope(|s| {
        s.spawn(|| {
            for round in 1..=50 {
                for name in ["a", "b", "c"] {
                    fs::remove_file(project.join(format!("{name}.jsonl"))).unwrap();
                }
                if round % 2 == 0 {
                    fx.update();
                }
                write_all(round);
                fx.update();
            }
        });
        for _ in 0..200 {
            fx.search_terms(&terms, 20);
        }
    });
}

#[test]
fn empty_query_returns_nothing() {
    let fx = Fx::new();
    retry_threads(&fx);
    let result = fx.search(&[]);
    assert!(result.results.is_empty());
    assert_eq!(result.matched_all, 0);
    assert_eq!(result.matched_some, 0);
}

#[test]
fn thread_without_messages_is_never_counted() {
    let fx = Fx::new();
    fx.thread("widgets", "t").custom_title("webhook plans").write();
    fx.thread("widgets", "u").user("webhook setup").write();
    fx.update();
    let result = fx.search(&["webhook"]);
    assert_eq!(sessions(&result), ["u"]);
    assert_eq!(result.matched_all, 1);
}

#[test]
fn desk_jobs_with_one_title_show_once_and_rank_below_own_thread() {
    let fx = Fx::new();
    let title = "pr-review acme/widgets#42";
    fx.thread("widgets", "desk-old")
        .desk_job()
        .at(TEN_DAYS_AGO)
        .ai_title(title)
        .user("review the pull request")
        .write();
    fx.thread("widgets", "desk-new").desk_job().ai_title(title).user("review the pull request").write();
    fx.thread("widgets", "own").ai_title(title).user("review the pull request").write();
    fx.update();
    let result = fx.search(&["review"]);
    assert_eq!(sessions(&result), ["own", "desk-new"]);
    assert_eq!(result.matched_all, 2);
    assert!(result.results[1].desk_job);
}

#[test]
fn desk_job_group_shows_the_newest_at_the_best_members_place() {
    let fx = Fx::new();
    let desk = |session: &str| fx.thread("widgets", session).desk_job().ai_title("nightly sweep");
    desk("desk-old").at(TEN_DAYS_AGO).user("deploy the gadget").write();
    desk("desk-new").user("deploy later").write();
    fx.thread("widgets", "own").user("gadget only").write();
    fx.update();
    let result = fx.search(&["deploy", "gadget"]);
    assert_eq!(sessions(&result), ["desk-new", "own"]);
    assert_eq!((result.matched_all, result.matched_some), (1, 1));
}

#[test]
fn desk_jobs_without_a_stored_title_never_collapse() {
    let fx = Fx::new();
    for session in ["desk-1", "desk-2"] {
        fx.thread("widgets", session).desk_job().assistant("sweep done").write();
    }
    fx.update();
    let result = fx.search(&["sweep"]);
    assert_eq!(result.results.len(), 2);
    assert_eq!(result.matched_all, 2);
}

#[test]
fn desk_job_title_does_not_match_but_its_place_does() {
    let fx = Fx::new();
    fx.thread("gadgets", "desk").desk_job().ai_title("nightly sweep").user("run the job").write();
    fx.update();
    assert!(fx.search(&["sweep"]).results.is_empty());
    assert_eq!(sessions(&fx.search(&["gadgets"])), ["desk"]);
}

#[test]
fn one_session_in_two_folders_shows_the_newer_file() {
    let fx = Fx::new();
    fx.thread("widgets", "same").at(TWO_DAYS_AGO).user("deploy the gadget").write();
    fx.thread("gadgets", "same").user("deploy the gadget").write();
    fx.update();
    let result = fx.search(&["deploy"]);
    assert_eq!(sessions(&result), ["same"]);
    assert_eq!(result.matched_all, 1);
    assert_eq!(first(&result).folder_name, "gadgets");
}

#[test]
fn newer_thread_wins_between_equal_matches() {
    let fx = Fx::new();
    fx.thread("widgets", "old").at(SIXTY_DAYS_AGO).user("tune the cache").write();
    fx.thread("widgets", "new").at(TWO_DAYS_AGO).user("tune the cache").write();
    fx.update();
    assert_eq!(sessions(&fx.search(&["cache"])), ["new", "old"]);
}

#[test]
fn segments_mark_exactly_the_matched_word() {
    let fx = Fx::new();
    retry_threads(&fx);
    let result = fx.search(&["webhooks"]);
    let hit = first(&result);
    assert_eq!(hit.session_id, "a");
    assert_eq!(hit.match_role, "user");
    let joined: String = hit.segments.iter().map(|s| s.text.as_str()).collect();
    assert!(joined.contains("webhooks"), "{joined}");
    assert!(hit.segments.iter().any(|s| s.hit));
    for segment in &hit.segments {
        assert_eq!(segment.hit, segment.text.contains("webhooks"), "{segment:?}");
    }
}

#[test]
fn each_hit_gets_a_snippet_from_its_own_thread() {
    let fx = Fx::new();
    fx.thread("widgets", "x").user("alpha webhook one").assistant("unrelated reply").write();
    fx.thread("widgets", "y").user("beta webhook two").write();
    fx.thread("widgets", "z").user("gamma webhook three").write();
    fx.update();
    let result = fx.search(&["webhook"]);
    assert_eq!(result.results.len(), 3);
    for hit in &result.results {
        let joined: String = hit.segments.iter().map(|s| s.text.as_str()).collect();
        let own = match hit.session_id.as_str() {
            "x" => "alpha webhook one",
            "y" => "beta webhook two",
            _ => "gamma webhook three",
        };
        assert_eq!(joined, own);
    }
}

#[test]
fn match_in_a_reply_has_assistant_role() {
    let fx = Fx::new();
    title_and_body_threads(&fx);
    let result = fx.search(&["fails"]);
    assert_eq!(first(&result).session_id, "d");
    assert_eq!(first(&result).match_role, "assistant");
}

#[test]
fn title_only_match_shows_the_first_prompt() {
    let fx = Fx::new();
    title_and_body_threads(&fx);
    let result = fx.search(&["webhook"]);
    let hit = first(&result);
    assert_eq!(hit.session_id, "c");
    assert_eq!(hit.title, "Webhook retries");
    assert_eq!(hit.match_role, "user");
    assert_eq!(hit.segments.len(), 1);
    assert_eq!(hit.segments[0].text, "set up the hooks");
    assert!(!hit.segments[0].hit);
}

#[test]
fn removed_folder_falls_back_to_the_nearest_parent() {
    let fx = Fx::new();
    fx.thread("widgets/feature", "g").user("cleanup task").write();
    fx.update();
    fs::remove_dir_all(fx.work.join("widgets").join("feature")).unwrap();
    let result = fx.search(&["cleanup"]);
    let hit = first(&result);
    assert!(hit.folder_gone);
    assert_eq!(hit.folder, fx.work.join("widgets").to_str().unwrap());
    assert_eq!(hit.folder_name, "widgets");
}

#[test]
fn limit_cuts_results_but_not_counts() {
    let fx = Fx::new();
    retry_threads(&fx);
    fx.thread("widgets", "c").user("retry again").write();
    fx.update();
    let result = fx.search_limit(&["retry"], 1);
    assert_eq!(result.results.len(), 1);
    assert_eq!(result.matched_all, 3);
}

#[test]
fn hit_carries_thread_details() {
    let fx = Fx::new();
    fx.thread("widgets", "h")
        .branch("feat/DEMO-7-x")
        .pr(9, "acme/widgets")
        .user("fix DEMO-8 now")
        .assistant("done")
        .write();
    fx.update();
    let result = fx.search(&["fix"]);
    let hit = first(&result);
    assert_eq!(hit.agent, "claude");
    assert_eq!(hit.title, "fix DEMO-8 now");
    assert_eq!(hit.last_active, common::NOW);
    assert_eq!(hit.folder, fx.work.join("widgets").to_str().unwrap());
    assert!(!hit.folder_gone);
    assert_eq!(hit.branch.as_deref(), Some("feat/DEMO-7-x"));
    assert_eq!(hit.keys, ["DEMO-7", "DEMO-8", "#9"]);
    assert_eq!(hit.prompts, 1);
    assert!(!hit.desk_job);
}
