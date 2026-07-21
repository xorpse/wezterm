---
tags:
  - tab_bar
---
# `tab_bar_width = 24`

{{since('nightly')}}

The width, in cells, of a vertical tab bar (see
[tab_bar_placement](tab_bar_placement.md) `= "Left"` or `"Right"`). Has no effect
on a horizontal tab bar.

The default is `24`.

At runtime you can drag the strip's inner edge to resize it; the dragged width is
persisted and used as the startup width on subsequent launches, taking
precedence over this value.

```lua
config.tab_bar_placement = 'Left'
config.tab_bar_width = 28
```
