# thread-search

`threads` keeps a local SQLite full-text index of your Claude Code conversation threads
and searches it fast enough to run on every key press. It reads the transcripts that Claude
Code writes under `~/.claude/projects` and never changes them.

## Install

```sh
cargo install --path .
```

This puts the `threads` binary in `~/.cargo/bin`.

## Commands

```sh
threads update                                  # bring the index up to date (about 30 ms when nothing changed)
threads rebuild                                 # delete the index and build it again
threads stats                                   # thread count, message count, index size, last update
threads search [--json] [--limit N] [--] <words...>
```

Options go before the words. Use `--` before words that start with a dash. The default limit
is 20. A word not in quotes matches as a prefix, so `deplo now` finds `deployment now`. The last
word is a prefix from one letter, so a search can run while you type. A word before the last word
is a prefix only when it has three or more letters. A shorter one matches a whole word only: in
`to ship`, `to` does not match `today`, so a thread with `today we ship` does not match every word.
The letters are counted in the last part of a word that holds `-`, `.`, or `/`, because the index
splits the word there and only the last part can be a prefix. So `zork-quixa` is a prefix
(`quixa` has five letters), and `quixa-zo` matches whole words only (`zo` has two).
The prefix for a word before the last is for words in Latin letters and digits; a word with other
letters (for example Cyrillic or Greek) matches whole there.
A query of more than four words or quoted groups matches every word before the last as a whole
word only. To find part of a word in a long query, put that word last.
A thread that holds a typed word as a whole word, in a message, the title, or its keys, ranks above
a newer thread that matches only through longer words when the matches are otherwise equal. A
thread with more matches or a match in the title can still rank first.
A word in quotes matches exactly. The index has no stemmer: in `retry now`, `retry` does not
match `retries`, so a thread with `retries now` does not match every word.
`search` does not update the index; run `threads update` first (the first search on a
machine tells you to).

`update` prints one line: `scanned N, unchanged N, appended N, reindexed N, removed N, scripted N`.
Threads started by scripts (`claude -p`, the SDK) are counted as `scripted` and left out.
When another `update` or `rebuild` runs, `update` and `rebuild` print `busy: another update runs` and exit 0. The command that prints `busy` changes nothing.

## JSON contract

`threads search --json` prints one JSON object:

```json
{
  "state": "ok",
  "matched_all": 3,
  "matched_some": 12,
  "results": [
    {
      "agent": "claude",
      "session_id": "...",
      "title": "...",
      "last_active": 1762950000,
      "folder": "/path/to/project",
      "folder_state": "ok",
      "folder_name": "project",
      "branch": "main",
      "keys": ["ABC-123"],
      "prompts": 14,
      "desk_job": false,
      "match": {
        "role": "user",
        "segments": [{ "text": "...", "hit": false }, { "text": "...", "hit": true }]
      }
    }
  ]
}
```

- `state` is `ok`, `no_index` (run `threads update`), or `building`. With the last two,
  `results` is empty.
- `matched_all` counts threads that match every word; `matched_some` counts threads that
  match some of them. A word can match in a message, the title, the keys, the folder name,
  or the branch. `results` lists the `matched_all` threads first, best rank first.
- `last_active` is Unix seconds: the time of the last message, or the modification time of the
  transcript file when no message has a readable time. `folder_state` is `ok` or `gone` (the folder no longer exists).
- `branch` is `null` when unknown.
- `match.segments` is the snippet split into runs; `hit: true` marks the matched words.
- `match.role` is `user`, `assistant`, or `summary`.
- A thread that matches only in the title, the keys, the folder name, or the branch has
  `role: "user"` and the first prompt as its one segment, with no `hit: true` segment. When
  the thread has no first prompt, `segments` is empty.
- A failure prints no JSON. It prints one line on stderr and exits with code 1. A usage error
  exits with code 2.

## Where things live

| Item | Default | Override |
| --- | --- | --- |
| Transcripts (read only) | `~/.claude/projects` | `THREADS_ROOT` |
| Index | `~/Library/Application Support/thread-search/index.db` | `THREADS_INDEX` |

The tool writes the index and its `-wal`, `-shm`, and `.lock` companions, nothing else. It
also writes `index.db.bad` when the index file is unreadable. The newest unreadable file
replaces an older `.bad`.

## Privacy

- No network access. The tool has no network code and adds no dependency for it.
- The transcript folder is opened read-only.
- Index files are created with mode 0600, so only your user can read them. The index holds
  the text of your conversations; do not share or commit it.

## Delete the index

```sh
threads rebuild
```

`threads rebuild` deletes the index and builds it again from the transcripts. To delete it
by hand, first make sure that no `threads` process runs. Then delete only these files:

```sh
rm -f ~/Library/Application\ Support/thread-search/index.db \
      ~/Library/Application\ Support/thread-search/index.db-wal \
      ~/Library/Application\ Support/thread-search/index.db-shm
```

Do not delete `index.db.lock`. `threads update` builds the index again from the transcripts.

A new index carries an SQLite `application_id` stamp. The tool refuses a database that is not
empty and is not a thread-search index, and leaves it untouched. A thread-search index has the
stamp. An index built before the stamp has a `meta` table with a `schema_version` of 1 or 2 and a
`threads` table. The tool renames a file that SQLite cannot read as a database to
`index.db.bad` and builds a new index. The schema version is 3.

## Measured speed

See [docs/measurements.md](docs/measurements.md).
