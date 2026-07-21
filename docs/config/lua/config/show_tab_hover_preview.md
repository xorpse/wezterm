---
tags:
  - tab_bar
---
# `show_tab_hover_preview = true`

{{since('nightly')}}

When `true`, hovering a tab in a vertical tab bar (see
[tab_bar_placement](tab_bar_placement.md)) reveals a floating card that previews
the tab's active pane without changing focus. The card shows the pane title, a
metadata line (foreground program, pane count, and working directory) and a short
preview of the pane's most recent output.

The card appears after a short delay controlled by
[tab_hover_preview_delay_ms](tab_hover_preview_delay_ms.md), stays open while the
pointer moves from the tab into the card, and closes once the pointer leaves both.

The default is `true`.

```lua
config.tab_bar_placement = 'Left'
config.show_tab_hover_preview = true
```
