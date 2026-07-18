---
tags:
  - tab_bar
---
# `tab_bar_collapsible = false`

{{since('nightly')}}

When `true`, a vertical tab bar (see [tab_bar_placement](tab_bar_placement.md))
can be collapsed. Hovering the middle of the tab bar's inner edge reveals a small
button; clicking it collapses the tab bar, hiding it entirely so the terminal
area fills the freed space. When collapsed, hovering the middle of that window
edge reveals the button again to expand it.

The default is `false`.

```lua
config.tab_bar_placement = 'Left'
config.tab_bar_collapsible = true
```
