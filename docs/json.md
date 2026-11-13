# JSON output

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
