---
tags:
  - tab_bar
---
# `tab_bar_placement = nil`

{{since('nightly')}}

Controls where the tab bar is rendered. Accepts one of:

* `"Top"` - horizontal tab bar at the top of the window.
* `"Bottom"` - horizontal tab bar at the bottom (equivalent to
  [tab_bar_at_bottom](tab_bar_at_bottom.md) `= true`).
* `"Left"` - a **vertical** tab bar down the left side of the window.
* `"Right"` - a **vertical** tab bar down the right side of the window.

When left unset (the default), the placement is derived from
[tab_bar_at_bottom](tab_bar_at_bottom.md).

`"Left"` and `"Right"` render a vertical sidebar of stacked tabs and require
[use_fancy_tab_bar](use_fancy_tab_bar.md) `= true`; with the retro tab bar they
fall back to a horizontal bar.

```lua
config.use_fancy_tab_bar = true
config.tab_bar_placement = 'Left'
```

The width of a vertical tab bar is set by [tab_bar_width](tab_bar_width.md), and
you can drag its inner edge to resize it (the width is remembered across
sessions). See also [tab_bar_collapsible](tab_bar_collapsible.md) and
[show_tab_icons](show_tab_icons.md).
