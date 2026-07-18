---
tags:
  - appearance
  - tab_bar
---
# `tab_attention`

{{since('nightly')}}

Makes an **inactive** tab pulse its background color when one of the panes in
that tab sets a designated [user var](../../../recipes/passing-data.md) to a
non-empty value. This is useful for drawing your eye to a background tab that
needs attention &mdash; for example a long-running command that has finished, or
an interactive program that is waiting for input.

The pulse fades in and out continuously until the user var is cleared (set to an
empty value) or the tab is focused. The currently active tab never pulses, since
you are already looking at it; focusing a pulsing tab is treated as
acknowledging it and stops the pulse.

The pulse color is chosen by looking up the user var's *value* in the `colors`
table, so a program can request different colors for different states.

`tab_attention` has the following fields:

* `var` - the name of the user var that drives the pulse. When a pane sets this
  var to a non-empty value, its tab pulses. Set to an empty string to disable
  the feature entirely. The default is `"claude_status"`.
* `fade_in_duration_ms` - how long the fade in takes, in milliseconds. The
  default is 400.
* `fade_out_duration_ms` - how long the fade out takes, in milliseconds. The
  default is 400.
* `fade_in_function` - an easing function that affects how the color fades in.
* `fade_out_function` - an easing function that affects how the color fades out.
* `colors` - a table mapping a user var *value* to the pulse color for that
  value. Any value with no matching entry falls back to Zenburn yellow.

The same easing functions as [visual_bell](visual_bell.md) are supported.

The default colors match the Zenburn palette:

```lua
config.tab_attention = {
  var = 'claude_status',
  fade_in_duration_ms = 400,
  fade_out_duration_ms = 400,
  fade_in_function = 'EaseIn',
  fade_out_function = 'EaseOut',
  colors = {
    waiting = '#f0dfaf',
    approval = '#cc9393',
  },
}
```

`tab_attention` requires [use_fancy_tab_bar](use_fancy_tab_bar.md) `= true` and
works with both horizontal and vertical
([tab_bar_placement](tab_bar_placement.md)) tab bars.

## Signaling attention from a program

A program signals attention by setting the user var to a non-empty value, and
clears it by setting it to an empty value. Values are base64 encoded, as
described in [Passing Data from a pane to
Lua](../../../recipes/passing-data.md):

```bash
printf '\033]1337;SetUserVar=claude_status=%s\007' "$(printf waiting | base64)"
printf '\033]1337;SetUserVar=claude_status=%s\007' "$(printf approval | base64)"
printf '\033]1337;SetUserVar=claude_status=\007'
```

The first line pulses the tab with the `waiting` color, the second with the
`approval` color, and the third clears the pulse.

## Example: flashing while an agent waits for you

The default `var` of `claude_status` is chosen to pair with a tool such as
[Claude Code](https://www.anthropic.com/claude-code), whose hooks can set the
var when the agent is waiting for you. Hooks run without a controlling terminal,
so rather than emitting the escape to their own stdout, they resolve the pane's
tty from `$WEZTERM_PANE` and write the escape to it directly.

Save this as `~/.claude/hooks/wezterm-tab-flash.sh` and make it executable:

```bash
#!/usr/bin/env bash
set -euo pipefail
state="${1:-}"
cat >/dev/null 2>&1 || true
[ -n "${WEZTERM_PANE:-}" ] || exit 0
command -v wezterm >/dev/null 2>&1 || exit 0
command -v jq >/dev/null 2>&1 || exit 0
tty=$(wezterm cli list --format json 2>/dev/null \
  | jq -r --argjson p "$WEZTERM_PANE" '.[] | select(.pane_id == $p) | .tty_name' || true)
[ -n "$tty" ] && [ "$tty" != "null" ] || exit 0
value=$(printf '%s' "$state" | base64 | tr -d '\n')
printf '\033]1337;SetUserVar=claude_status=%s\007' "$value" >"$tty" 2>/dev/null || true
```

Then wire it into `~/.claude/settings.json` so it sets the var when Claude is
idle or waiting for approval, and clears it when you submit a prompt:

```json
{
  "hooks": {
    "Notification": [
      { "matcher": "idle_prompt",
        "hooks": [ { "type": "command", "command": "bash ~/.claude/hooks/wezterm-tab-flash.sh waiting" } ] },
      { "matcher": "permission_prompt",
        "hooks": [ { "type": "command", "command": "bash ~/.claude/hooks/wezterm-tab-flash.sh approval" } ] }
    ],
    "UserPromptSubmit": [
      { "matcher": "",
        "hooks": [ { "type": "command", "command": "bash ~/.claude/hooks/wezterm-tab-flash.sh ''" } ] }
    ]
  }
}
```

The tab bar only animates while the wezterm window is focused, so a background
tab in the focused window pulses smoothly; if you switch to another application
entirely, the tab holds its last color until you return to wezterm.

See also [visual_bell](visual_bell.md),
[tab_bar_placement](tab_bar_placement.md), the
[user-var-changed](../window-events/user-var-changed.md) event, and [Passing
Data from a pane to Lua](../../../recipes/passing-data.md).
