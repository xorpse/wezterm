# `OpenReviewPane`

{{since('nightly')}}

Splits the current pane and opens a **git review pane**: a scrollable, unified
diff of the git repository containing the active pane's working directory. You
can navigate hunks and files, attach text annotations to individual lines, and
send selected diff lines (with their annotations) to the pane you opened it
from.

The diff is computed by running `git` in a background thread, so opening the
pane is non-blocking even for large repositories.

```lua
local wezterm = require 'wezterm'
local act = wezterm.action

config.keys = {
  {
    key = 'r',
    mods = 'LEADER',
    action = act.OpenReviewPane {},
  },
}
```

## Arguments

`OpenReviewPane` accepts an optional table:

* `direction` - where to place the review split relative to the current pane:
  `'Right'` (default), `'Left'`, `'Up'` or `'Down'`.
* `size` - the size of the new split, either `{ Percent = 50 }` (default) or
  `{ Cells = 80 }`.
* `mode` - what to diff against:
    * `'WorkingTree'` (default) - uncommitted changes vs `HEAD`, including
      untracked files.
    * `{ Branch = 'main' }` - the working tree vs the tip of the named branch.
    * `{ MergeBase = 'main' }` - the working tree vs the merge base with the
      named branch, so both committed and uncommitted work on the branch is
      shown, including untracked files.
* `layout` - how the diff is arranged:
    * `'Unified'` (default) - one column, long lines wrap.
    * `'SideBySide'` - old and new in two columns, long lines truncated with
      `…` and read by scrolling horizontally. Panes narrower than 100 columns
      fall back to the unified layout.

Diff text is syntax highlighted when the file's language is recognised, with
added and removed lines marked by a background tint. The theme follows the
lightness of your configured background.

```lua
config.keys = {
  {
    key = 'R',
    mods = 'LEADER',
    action = act.OpenReviewPane {
      direction = 'Down',
      size = { Percent = 40 },
      mode = { MergeBase = 'main' },
    },
  },
}
```

## Keys inside the review pane

| Key | Action |
|-----|--------|
| `j` / `k`, arrows | Move the cursor down / up |
| `n` / `p` | Next / previous hunk |
| `N` / `P` | Next / previous file |
| `g` / `G` | Jump to the top / bottom |
| `Ctrl-d` / `Ctrl-u`, `PageDown` / `PageUp` | Scroll by a page |
| `o` / `Tab` / `Enter` (on a file header) | Collapse / expand the file under the cursor. Files start collapsed |
| `/` | Find a file by name. Type a query and press `Enter` to jump to the first match, then `n` / `p` to cycle to the next / previous match. `Enter` or `Esc` finishes; typing again refines the query |
| `i` / `a` / `Enter` | Open (or edit) an inline comment on the line under the cursor. `i` puts the cursor at the start, `a` at the end. `Enter` edits an existing comment, and otherwise sends the selection |
| `dd` | Delete the comment on the line under the cursor |
| `e` | Open the file at the line under the cursor in `$EDITOR` (default `nvim`) in a split below |
| `Space` / `v` | Start or clear a selection anchored at the cursor |
| `Enter` (on an uncommented line) | Send the selected diff lines (or the cursor line) to the originating pane |
| `Shift+Enter` | Send all comments to the originating pane, grouped as `path:line: comment` |
| `b` | Toggle the diff mode: uncommitted changes ⇄ uncommitted changes plus everything since the merge base with the parent branch (the branch's upstream, falling back to the repository's default branch) |
| `s` | Toggle between the unified and side-by-side layouts |
| `h` / `l`, left / right arrows | Focus the old / new column (side-by-side only). Comments, `e` and sends act on the focused column |
| `<` / `>` | Scroll the columns horizontally (side-by-side only) |
| `r` | Recompute the diff |
| `q` / `Esc` | Close the review pane |

Inside the inline comment editor: type freely across multiple lines, `Enter`
inserts a newline, `Shift+Enter` saves and closes, and `Esc` discards. Saving an
empty comment removes it.

## SSH / remote repositories

If the pane you open the review from is working in a directory on a remote host
(for example you ran `ssh somehost` and `cd`'d into a repo there), the diff is
computed by running `git` on that host over SSH, using
`ssh -o BatchMode=yes`. This only works when you have non-interactive
(key/agent) SSH access to the host — if a password would be required, it fails
fast and the pane shows:

> Review requires a local git repository; this pane's working directory is on `somehost`.

For remote repositories, untracked files are not shown, and `e` (open in editor)
is disabled.

The mouse works too: scroll wheel to scroll, click a line to move the cursor,
**double-click a line to open an inline comment on it**, click a file header to
collapse/expand it, and click-drag to select a range of lines.
