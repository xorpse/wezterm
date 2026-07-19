---
tags:
  - tab_bar
---
# `tab_bar_search = true`

{{since('nightly')}}

When `true`, a vertical tab bar (see [tab_bar_placement](tab_bar_placement.md))
shows a search field at the top of the tab column. Clicking the field focuses it;
as you type, the tab list is filtered to only those tabs whose title contains the
query, matched case-insensitively as a substring. The original tab order is
preserved and an empty state is shown when nothing matches.

While the field is focused, `Backspace` edits the query, `Ctrl-U` clears it, and
`Escape` clears the query and returns focus to the active pane. The query is
retained if the tab bar is collapsed and re-opened.

The default is `true`.

```lua
config.tab_bar_placement = 'Left'
config.tab_bar_search = true
```
