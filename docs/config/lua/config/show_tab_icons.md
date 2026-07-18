---
tags:
  - tab_bar
---
# `show_tab_icons = false`

{{since('nightly')}}

When `true`, each tab title is prefixed with a small icon glyph derived from the
tab's active program (for example an editor, git, or shell icon). The glyphs come
from a Nerd Font, so a Nerd Font must be available for them to render.

The default is `false`.

For full control over tab icons and titles, use the
[format-tab-title](../window-events/format-tab-title.md) event instead; its
result takes precedence over this option.

```lua
config.show_tab_icons = true
```
