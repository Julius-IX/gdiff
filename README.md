# gdiff

A tiny TUI around `git diff`.

## Install

```sh
git clone https://github.com/Julius-IX/gdiff.git
cd gdiff
cargo install --path .
```

## Usage

```sh
gdiff                 # working tree vs HEAD~X
gdiff <commit>        # <commit> vs <commit>~X
gdiff --follow        # slide both sides back: HEAD~0 vs HEAD~1, then HEAD~1 vs HEAD~2, ...
gdiff <commit> -f     # same, starting from <commit>
gdiff --no-pager      # ignore your git pager/color config
```

`X` starts at 0 and goes up to the number of commits reachable from the starting commit.

## Keys

| Key | Action |
| --- | --- |
| `←` / `→` | Compare against a newer / older commit |
| `↑` / `↓` | Scroll the diff |
| `PgUp` / `PgDn` | Scroll a page |
| `q` / `Esc` | Quit |


## Pager support

gdiff renders diffs through the pager git itself would use for `git diff`: `pager.diff` if set, otherwise the result of `git var GIT_PAGER` (`$GIT_PAGER` -> `core.pager` -> `$PAGER`).
Tools such as `delta` and `bat` work without additional setup.

A TUI cannot hand the terminal to an interactive pager. Instead, gdiff pipes the diff through the configured command non-interactively, parses the resulting ANSI output,
and displays it in its own viewport. Scrolling is handled by gdiff.

- **Passthrough pagers:** `less`, `more` and `cat` are skipped. Git's native colors are used instead.
- **bat:** requires a language to highlight diffs (e.g. `bat -l diff`). gdiff sets `BAT_OPTS` to force color output and disable paging.
- **delta:** cannot detect the terminal width when its output is piped. gdiff sets `COLUMNS`; if side-by-side layouts render incorrectly, set `width` in your delta configuration.
- **Failures:** if the pager exits with an error, the message is shown in the diff pane. Use `--no-pager` to bypass it.

## Notes

- With no arguments, `X=0` shows `git diff HEAD` (staged + unstaged), not plain `git diff`.
- With an explicit commit, `X=0` compares the commit to itself, so you'll see "(no changes)" until you press `→`.
- Pagers are launched with `sh -c`, so on Windows you need Git for Windows.
