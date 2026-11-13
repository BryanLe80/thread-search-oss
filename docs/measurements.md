# Measurements

Measured on the real Claude Code projects folder of the author's machine.
Machine: Apple M5 Max, macOS 27.0.1. Date: 2026-11-12. Release build with the
`[profile.release]` settings in `Cargo.toml` (opt-level 3, LTO, one codegen unit, strip).

No chat text, snippet text, or folder path is recorded here, only numbers and pass or miss.

All rows are from the build with schema version 3 (no stemmer, the `application_id` stamp), except
the row marked "(first run)", which is from the first measurement on the same day. The real query
checks below were run again on this build.

## Targets

| Operation | Target | Measured | Result |
| --- | --- | --- | --- |
| `threads rebuild` (full build) | 5 s or less | 3.34 and 3.38 s wall (2.80 s user, 0.52 to 0.54 s system), two runs | pass |
| `threads rebuild` that replaces an index of schema version 2 | 5 s or less | 4.76 s wall (2.88 s user, 0.74 s system), single run, directly after the install | pass (close) |
| `threads rebuild` peak memory | no target | 24.4 to 24.7 MB (maximum resident set size), three runs | recorded |
| `threads update`, no change | 150 ms or less | 26.3 ms median; 25.5 to 29.1 ms over seven runs | pass |
| `threads update`, after live threads grew (first run) | 300 ms or less | 41 ms (2 files re-read, so not exactly the one-thread case) | pass |
| `threads search`, three ordinary words | 30 ms or less | 8.5 ms median (8.3 to 8.8 ms), the same median as the first run | pass |
| `threads search`, three very common words | 30 ms or less | 25.2 ms median (25.0 to 25.7 ms) | pass (close) |
| `threads search`, last word one letter | 30 ms or less | 15.5 ms median | pass |
| `threads search`, last word three letters | 30 ms or less | 8.9 ms median | pass |
| `threads search` peak memory, very common words | 30 MB or less | 10.0 to 10.1 MB | pass |
| `threads search` peak memory, other shapes | 30 MB or less | 8.7 to 9.5 MB | pass |
| Thread count | 1,500 to 2,200 plausible | 1,809 | pass |
| No `sdk` thread in the index | none | none: 1,943 indexed plus 1,967 scripted equals the 3,910 files scanned, and the thread count (1,809) is below the indexed file count | pass |

Method:

- Rebuild: `/usr/bin/time -l threads rebuild`.
- Update: `threads update` run back to back; the no-change figures are timed with a monotonic
  clock in a wrapper that starts the process and discards its output.
- Search: five runs per query shape, `threads search --json <words>` with output discarded,
  median wall time from the same wrapper. Memory is the range of "maximum resident set size"
  from `/usr/bin/time -l` over five runs per shape, with the output sent to `/dev/null`.
- The very common words query was `the and to`. The other shapes used ordinary words.

## Sizes and counts

- Index size: 89.3 MB (93,646,848 bytes) for 1,809 threads and 65,288 messages.
- Binary size: 2,413,936 bytes (2.3 MB).
- Source files scanned: 3,910 top-level transcript files: 1,943 indexed as threads
  (some hold no messages, so 1,809 threads appear in `threads stats`) and 1,967 skipped as
  scripted (SDK) runs. No scripted thread is in the index: indexed plus scripted equals the
  file count, and the thread count is below the indexed file count.
- The version-2 index had no `application_id` stamp. The rebuild took it as an index of ours by
  its `schema_version` row and its `threads` table, deleted it, and left no `index.db.bad`.
- Index files are mode 0600 (`index.db`, `-wal`, `-shm`, `.lock`).

## Real query checks

| Check | Result |
| --- | --- |
| `thread sleep wake trigger`: first hit is "Thread sleep wake testing" | pass |
| `ABC-4812`: only threads that hold the key in their text (3 hits match all words, 0 match some) | pass |
| `orca session search`: "Search past agent threads feature" in the first three (it is second, and its title has changed since the first run) | pass |
| No thread folder shows a `Desktop/Work/Dev` prefix (four queries, up to 200 hits each) | pass |
| `deploy` and `deploymen` as the last word find threads (626 and 137 match all words) | pass |
| No hit of a very common word shows a key such as `ADD-2` made from a lower-case branch word | pass |

The `orca session search` check had the feature thread first before the stemmer was removed. These
checks ran on a build where a word before the last word matched whole words only, so `session` did
not count `sessions`. The prefix rule of the section below applies to builds after it.

## Slug rule on a long path

A scratch folder with a 270-character path got a project folder name of 207 characters:
the first 200 characters of `slug(path)`, then `-`, then six random-looking letters and
digits. So for a path whose slug is longer than 200 characters, Claude Code truncates the slug to
200 characters and appends a hash suffix. `src/folder.rs` `slug()` does not apply this rule.
For such a thread the slug match does not find a moved tree, but the recorded last working
folder is still used while it exists. No code was changed; this is reported for a decision.

## Known notes, checked

- Very common words cost about 25 ms against 8.5 ms for ordinary words, as on the
  synthetic index (26 ms). It stays under 30 ms but has little room.
- The no-change update is 25 to 29 ms, so the one-query-per-file fix was not needed.
- Folder resolution is not a hot spot at these numbers, so `child_dirs` was not changed.
- The real index is 89.3 MB, with the `prefix='1 2 3'` option on the message table in place.
- With the current rank constants, all real query checks passed, so the constants
  were not tuned.

## What was changed after measuring

- Added the `[profile.release]` settings to `Cargo.toml` before installing. No source change.

## Prefix rule for words before the last (2026-11-12)

Rule: an unquoted word before the last word matches as a prefix when the last part of the word has
three or more letters. A shorter one matches a whole word only. The last word is a prefix from one
letter. In a query of more than four words or quoted groups, every word before the last matches a
whole word only.
A thread that holds a typed word whole gets its score multiplied by 1.6 for each such word.

Same machine and index as above. Release build of this rule against the real index, read only.
The baseline is the build before this rule, where only the last word is a prefix.

Method:

- `threads search --json --limit 20 -- "<words>"`, output sent to `/dev/null`.
- One warm-up run per query, not counted. Then five runs. Wall time is the median from a wrapper
  with a monotonic clock. The range follows in parentheses.
- Peak memory is the range of "maximum resident set size" from `/usr/bin/time -l` over five runs.
- The queries use common English words. Only the `matched_all` and `matched_some` counts were read.

| Query shape | Baseline | This rule | Peak memory | Result |
| --- | --- | --- | --- | --- |
| Three ordinary full words | 12.7 ms | 13.0 ms (12.8 to 15.1) | 9.5 to 9.6 MB | pass |
| Three very common words (`the and to`) | 25.3 ms | 28.7 ms (28.5 to 29.0) | 11.0 to 11.1 MB | pass (close) |
| Three partial words of four letters | 11.6 ms | 16.6 ms (16.5 to 16.7) | 9.4 to 9.5 MB | pass |
| Three partial words of one to three letters | 13.8 ms | 13.0 ms (12.9 to 13.2) | 9.5 to 9.6 MB | pass |
| Three one-letter words (`t a s`) | 14.4 ms | 13.9 ms (13.7 to 14.0) | 10.2 to 10.3 MB | pass |
| Three two-letter words | 13.1 ms | 13.8 ms (13.5 to 14.0) | 9.5 to 9.6 MB | pass |
| One letter, then two full words | 13.0 ms | 13.8 ms (13.5 to 14.1) | 9.5 to 9.7 MB | pass |
| Four partial words of four letters | 10.0 ms | 20.4 ms (20.2 to 20.5) | 9.4 to 9.5 MB | pass |
| Four words of a sentence | 20.4 ms | 24.5 ms (24.0 to 25.3) | 10.6 MB | pass |
| Five partial words of four letters | 9.1 ms | 8.3 ms (8.1 to 8.5) | 9.3 MB | pass |
| Five one-letter words | 21.9 ms | 20.0 ms (19.6 to 20.1) | 9.9 MB | pass |
| Five three-letter words | 27.5 ms | 25.3 ms (25.0 to 25.9) | 10.3 to 10.4 MB | pass |
| Five very common words | 27.1 ms | 24.7 ms (24.5 to 25.2) | 10.6 to 10.7 MB | pass |
| Five words of a sentence | 27.4 ms | 26.9 ms (25.4 to 27.2) | 10.6 to 10.8 MB | pass |
| Ten words of a sentence | 31.8 ms | 30.5 ms (30.4 to 30.7) | 10.8 to 10.9 MB | miss, faster than baseline |
| Ten words of another sentence | 41.4 ms | 38.4 ms (38.0 to 39.2) | 11.0 to 11.1 MB | miss, faster than baseline |
| Twenty words of a sentence | 54.8 ms | 51.9 ms (51.9 to 52.0) | 11.3 to 11.5 MB | miss, faster than baseline |
| Twenty words of another sentence | 62.5 ms | 58.7 ms (57.9 to 59.0) | 11.5 to 11.6 MB | miss, faster than baseline |

Known limit: a query of 10 or more words can take more than 30 ms on this index, on the baseline
too. Each common word reads a long list, whatever the prefix rule. The target of the design spec
(section 6.8) is set for a three-word search. This build is faster than the baseline on each
10-word and 20-word query.

The row closest to the target is three very common words: 28.7 ms with the exact-word check
(25.3 ms on the baseline). For each prefix word, the check reads the messages and titles that hold
the word whole, and a very common word has a long list.

A query of five or more words gives the same `matched_all` and `matched_some` counts as the
baseline, because every word before the last matches whole in both.

Word count limit: measured with a prefix for every earlier word of three or more letters, and no
word limit. The worst row of each count:

| Words | Worst median | Query |
| --- | --- | --- |
| 4 | 22.4 ms | four partial words of four letters |
| 5 | 30.6 ms | five words of a sentence |
| 6 | 36.6 ms | six words of a sentence |
| 8 | 34.3 ms | eight words of a sentence |
| 10 | 48.7 ms | ten words of a sentence |
| 20 | 75.0 ms | twenty words of a sentence |

Four is the largest count whose worst row stays under 25 ms, so the limit is four words.

Word scan rows instead of an `AND` query: the set of threads with every word in one message comes
from the rows of the word scans. A second `AND` query would read each word's list again. Measured
on the three-letter rule with no word limit, the same method, and the same counts in both builds:

| Query shape | With the `AND` query | With the word scan rows |
| --- | --- | --- |
| Three ordinary full words | 13.0 ms | 12.1 ms |
| Three very common words | 24.9 ms | 22.3 ms |
| Three partial words of four letters | 18.5 ms | 16.5 ms |
| Three partial words of one to three letters | 13.3 ms | 14.1 ms |
| Three one-letter words | 13.7 ms | 12.8 ms |
| Three two-letter words | 13.0 ms | 12.5 ms |
| One letter, then two full words | 14.4 ms | 13.1 ms |
| Five partial words of four letters | 26.0 ms | 22.8 ms |
| Five one-letter words | 21.1 ms | 20.1 ms |
| Five three-letter words | 23.6 ms | 22.0 ms |
| Five very common words | 25.8 ms | 23.4 ms |

Notes:

- A partial word of four or more letters is past the prefix index, so each search merges the
  lists of every word that it starts.
- A prefix of one or two letters before the last word was measured at 32.1 ms for `t a s` and
  65.0 ms for five one-letter words. Each such word reads most of the index again in the word
  scan, the phrase check, and the snippet query. This is why the rule starts at three letters.
