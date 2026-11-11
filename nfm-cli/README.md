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
| `argument` | None: a one-row prompt takes text as typed and Enter prints it |
| `output [OPTIONS] -- PROGRAM ARGS...` | None: what the program prints is shown as it prints it, to read |

Line inputs also take `--unique`, `--reverse`, `--line-continuation`,
`--delimiter` with `--text-field`/`--value-field`, and `--input-format csv`
(filter columns with `/:size==2`, sort with `/!Descending==size`).
Lines (and joined records) over 16 KiB are skipped; the counter shows how
many, as `matched/total · N skipped`.

### Column types

`--csv-types NAME:TYPE,...` says how CSV columns are sorted and filtered, for
example `--csv-types size:bytes=KB,took:duration,started:datetime`:

| Type | Values | Sorted and compared as |
|---|---|---|
| `auto` (default) | a number, else a date and time, else text | numbers, then times, then text |
| `text` | anything | text, ignoring case |
| `number` | `42`, `-1.5`, `1e5` | numbers |
| `bytes`, `bytes=UNIT` | `1234`, `100MB`, `1.5 GiB`; plain numbers are in UNIT | bytes |
| `duration` | seconds, `[d.]hh:mm[:ss[.fff]]` or `2h 13m` | seconds |
| `datetime` | a timestamp (below), including Unix seconds or milliseconds | instants |

Each cell is parsed once, as its row arrives. Values a column's type cannot
parse, including empty ones, sort after the rest in either direction. Byte
units are binary (`KB` is 1024 bytes). An `auto` column never reads a plain
number as a Unix time or `12:30` as a duration; declare the type for that.

Filter values are read the same way, so `/:size>100MB`, `/:took>=1:30:00` and
`/:cpu==1` (matching `1.0`) work. A date alone means its whole local day:
`/:started==2026-11-06` matches any time that day, and `/:started>2026-11-06`
matches from the next day on. Comparing a number with a word matches nothing.

Number, size and duration columns are right-aligned. `--csv-formats
NAME:FORMAT,...` shows columns formatted, such as `size:bytes,took:duration`:

| Format | Value | Shown as |
|---|---|---|
| `date`, `date=STRFTIME` | timestamp | `2026-11-06 14:05`, local time |
| `duration` | a duration | `2h 13m` |
| `number`, `number=N` | a number | `1,234,568`; N decimals |
| `bytes` | a size | `117.7 MB` |
| `percent`, `percent=N` | a fraction | `15.3%`; N decimals (default 1) |

A column without a format on a `datetime`, `duration` or `bytes` column uses
`date`, `duration` or `bytes`. The table and cards show the formatted text and
fuzzy search reads it, while sorting, filters and the printed selection use
the raw values. Timestamps can be RFC 3339, Unix seconds or milliseconds, or a
date and time without an offset, which is read as local time. Values are read
as their column's type reads them, so `bytes` shows a `bytes=KB` column's
`123456` as `120.6 MB`. A value that does not parse is shown as it is. A
format must suit its column's type: `date` on a `bytes` column is an error,
while `auto` and `text` columns take any format.

The `processes` table declares its columns (sizes in KB, CPU as a duration),
so `/:WorkingSet>500MB` and `/!Descending==PrivateBytes` work there too. It
shows sizes and CPU time formatted (`117.7 MB`, `1h 2m`), and lists the busiest
processes first: by CPU time, then private bytes. Typing text orders by match
instead, and a `/!` sort in the query replaces the default order.

CSV rows can be shown as multi-line cards. `--row-template` lists lines
separated by `;` and slots separated by `|`. Each slot is a column with
optional modifiers: `title`, `muted`, `accent`, `right` and `nohl` (no match
highlighting). A slot shows its column as the table does, formatted, and
highlights matches in it.

For wording `nfm` does not have, such as a relative time ("9 minutes ago"),
put it in its own column: for example `completed` holding the timestamp (typed
`datetime`, for `/!Descending==completed` and `/:completed>2026-11-01`) and
`completed_ago` holding the words, shown on the card and found by fuzzy
search. Hosts embedding the picker can do this with
`StructuredSchema::with_formatter` instead.

### Icons

`--row-icon SLOT` paints an icon beside each card's title, and the `icon`
modifier paints any slot as an icon inline (`path:icon:devicon|path`). An
icon slot's value is an icon reference, or a file name or path with the
`devicon` format (the one slot format), which shows the file type's Nerd Font
glyph:

| Reference | Icon |
|---|---|
| `glyph:TEXT[@COLOR]`, or 1–2 characters | the text, in the symbols font |
| *(empty)* | nothing; the column is kept so cards stay aligned |

COLOR is `#rrggbb`, so `glyph:●@#40a02b` is a green dot; without a colour a
glyph uses the theme's text colour. To show a state as an icon, put the
reference in its own column and keep the state word in another, so
`/:state==running` still filters.

Icons only ever come from glyphs here: input data cannot make `nfm`
open files, load images or query windows. Hosts that embed the picker can add
resolvers for their own schemes, but only for references they build
themselves.

```text
nfm file agents.csv --input-format csv --title Agents \
  --csv-types updated:datetime \
  --row-template "project:muted|updated_ago:right:muted;title:title;summary:muted" \
  --row-icon icon
```

where the `icon` column holds references such as `glyph:●@#40a02b`, or a Nerd
Font symbol with a colour, and the script writes `updated_ago` ("9 minutes
ago") next to the `updated` timestamp it sorts by.

## Previews

- `--preview PROGRAM --preview-arg ...`: `{item}` and `{line}` are replaced in
  arguments, and the program also gets `NFM_PREVIEW_ITEM`/`NFM_PREVIEW_LINE`.
  `--preview-type image` decodes stdout as an image.
- Profiles: `--preview-command NAME PROGRAM` plus a `--preview-resolver` that
  prints a profile name per item (or `--preview-default`).
- With `--preview-file-field`, the built-in highlighted file preview is used,
  centered on `--preview-center-line-field`.
- Long text preview lines wrap at the pane width; `--preview-wrap false` (or
  Alt+Z) lets them scroll sideways instead.

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

### Pages

An action can show another picker on top of the one showing, as a page. Esc
goes back a page, to the one beneath exactly as it was left, and closes the
window from the first; the built-in `cancel` action closes it from any page.
Accepting on any page prints that page's selection.

A page is an nfm command line. It reads the same config file and
`NFM_DEFAULT_OPTS` as the first page, but none of the first page's own flags,
and it cannot read standard input. The window keeps the first page's font,
colors and position and takes each page's size.

- `--action NAME push` with one `--action-arg NAME ARG` per argument of the
  page's command line; `{query}` and `{item}` are replaced in them. Put text
  that may start with `-` after `--`.
- An action program answers `{"action":"push","args":[...]}`.
- `nfm argument` is a page that only asks for text, with `--prompt` and
  `--placeholder` to say what for. Its selection is the text, so an action
  bound to Enter gets it as `{query}` (or `query` in `NFM_ACTION_STATE`, where
  the picker kind is `argument`).
- `nfm output -- PROGRAM ARGS...` is a page that shows what a program prints,
  standard output and standard error together with their colors, while it
  runs. It stays at the end as more arrives until you move up, and follows
  again from the last row. `j`/`k` and the arrows move, `/` searches, `v` and
  `y` select and copy, Ctrl+Shift+C copies everything. Esc ends a search or a
  selection first, then goes back; the program is stopped when the page goes.
  `--rows` is its height. `--format markdown` lays what is printed out as
  markdown instead: `nfm output --format markdown -- cmd /c type README.md`.

```powershell
# Ask for a host, then watch it being pinged; Esc stops the ping and asks again.
nfm argument --prompt "Ping:" --placeholder "<host>" --bind enter go `
  --action go push --action-arg go output --action-arg go -- `
  --action-arg go ping --action-arg go "{query}"
```

```powershell
# Ask for text, then pick among the lines that match it; Esc asks again.
nfm argument --prompt "Grep:" --placeholder "<text>" --bind enter grep `
  --action grep push `
  --action-arg grep command --action-arg grep --delimiter --action-arg grep : `
  --action-arg grep --text-field --action-arg grep 3 `
  --action-arg grep --preview-file-field --action-arg grep 1 `
  --action-arg grep --preview-center-line-field --action-arg grep 2 `
  --action-arg grep -- --action-arg grep rg --action-arg grep -n `
  --action-arg grep --no-heading --action-arg grep -- --action-arg grep "{query}"
```

In a config file the same action is `type = "push"` with the page's command
line as `args`.

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

[actions.walk]
type = "push"
args = ["filesystem", "{item}"]

[bind]
ctrl-o = "open"
```
