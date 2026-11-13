# Commands and search syntax

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
