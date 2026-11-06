# nfm-cli

`nfm` is a fuzzy picker window for scripts, built on `nfm-egui`. It reads
candidates, opens a borderless always-on-top window on the monitor under the
mouse, and prints the selection.

```powershell
git ls-files | nfm --preview bat --preview-arg --color=always --preview-arg '{item}'
nfm file history.txt --reverse --unique
nfm command --unique -- git log --format=%s
rg -n --no-heading TODO | nfm --delimiter : --text-field 3 --preview-file-field 1 --preview-center-line-field 2
nfm filesystem C:\src --max-depth 3 --files-only
nfm processes
nfm windows --activate
```

Exit status: `0` selected, `1` no match, `2` error, `130` cancelled.
Run `nfm --help` for every option.

## Inputs

| Command | Candidates |
| --- | --- |
| *(none)* | Lines of standard input |
| `file PATH` | Lines of a file |
| `command [OPTIONS] -- PROGRAM ARGS...` | Lines of a program's output |
| `filesystem [ROOT...]` | Files and directories; Enter opens directories, Ctrl+U goes up |
| `processes` | Processes; Ctrl+K kills, Ctrl+R refreshes |
| `windows` | Top-level windows with a live thumbnail |

Line inputs also take `--unique`, `--reverse`, `--line-continuation`,
`--delimiter` with `--text-field`/`--value-field`, and `--input-format csv`
(filter columns with `/:size==2`, sort with `/!Descending==size`).

## Previews

- `--preview PROGRAM --preview-arg ...`: `{item}` and `{line}` are replaced in
  arguments, and the program also gets `NFM_PREVIEW_ITEM`/`NFM_PREVIEW_LINE`.
  `--preview-type image` decodes stdout as an image.
- Profiles: `--preview-command NAME PROGRAM` plus a `--preview-resolver` that
  prints a profile name per item (or `--preview-default`).
- With `--preview-file-field`, the built-in highlighted file preview is used,
  centered on `--preview-center-line-field`.

## Keys and actions

- `--bind KEY ACTION`: built-ins `accept`, `cancel`, `parent` (filesystem), or
  an action declared with `--action NAME action-resolver` and
  `--action-program NAME PROGRAM`.
- `--accept-resolver PROGRAM` routes Enter through a program.
- Action programs receive `NFM_ACTION_STATE`
  (`{"selection":…,"picker":{"kind":…},"query":…}`) and print
  `{"action":"none"}`, `{"action":"complete"}`, or
  `{"action":"picker","picker":{"kind":"filewalker","roots":["{item}"]}}`.
- `--expect ctrl-o,alt-enter` accepts with those keys too and prints the key
  on the line before the selection.

## Scripting

- `--filter QUERY` prints the matches without opening a window.
- `--select-1` and `--exit-0` decide without a window when the query has one
  match or none.
- `--print-query`, and `--output json` for the value, display text, index and
  the item's fields.

## Configuration

Defaults are read from `%APPDATA%\nfm\config.toml` (or `NFM_CONFIG`, or
`--config FILE`; `--no-config` skips it), then `NFM_DEFAULT_OPTS`, then the
command line. Config keys are flag names:

```toml
layout = "top"
rows = 12
unique = true
expect = ["ctrl-o"]

[colors]
accent = "#ff8800"

[profiles.code]
program = "bat"
args = ["--color=always", "{item}"]

[actions.open]
program = "open.exe"

[bind]
ctrl-o = "open"
```
