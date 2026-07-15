# Terminal Support and Troubleshooting

Grok Build runs as a full-screen TUI. To draw the interface, it relies on terminal escape sequences for color, clipboard, mouse, and full-screen control. Some terminals, multiplexers, and SSH sessions handle these sequences differently.

## Quick Fixes

### Truecolor / Washed-out or wrong colors

```bash
# Add to ~/.zshrc or ~/.bashrc
export COLORTERM=truecolor
```

Inside tmux or over SSH, also add to your tmux config:

```tmux
# ~/.tmux.conf or ~/.byobu/.tmux.conf
set -g default-terminal "tmux-256color"
set -as terminal-features ",*:RGB"
```

### Recommended tmux settings (clipboard + passthrough)

```tmux
set -g set-clipboard on
set -g allow-passthrough on
```

After editing, run:

```bash
tmux source-file ~/.tmux.conf
# or detach and reattach
```

### Live diagnostics inside Grok

Run this slash command:

```
/terminal-setup
```

The command reports the terminal, multiplexer, **color level**, **available themes**, and clipboard routes Grok detected, then lists any issues and how to fix them. When color is below truecolor, it explains how to unlock the truecolor-only themes (TokyoNight, RosePineMoon, OscuraMidnight) — or notes that Terminal.app is inherently 256-color. The aliases `/terminal-check` and `/terminal-info` run the same command.

---

## Detected Terminals

Grok detects these terminal emulators from environment variables:

- **Apple Terminal** (Terminal.app)
- **Ghostty**
- **iTerm2**
- **Warp**
- **WezTerm**
- **Kitty**
- **Alacritty**
- **Rio**
- **foot** (Wayland-native, Linux)
- **VS Code**, **Cursor**, **Windsurf**, and **Zed** integrated terminals
- **JetBrains** IDE terminals (IntelliJ, PhpStorm, and others)
- **Grok Desktop**
- **VTE**-based terminals (GNOME Terminal, GNOME Console, Tilix)
- **Windows Terminal**

Detection has these limitations:

- Inside tmux, the variables Grok needs to identify the terminal don't reach the pager.
- Over SSH, many terminal variables aren't forwarded.
- tmux's global environment (`tmux -g`) reflects the first client that attached to the server, not your current session.

---

## Common Problems and Fixes

### Problem: Colors look wrong or lack truecolor

**Cause**: `COLORTERM` not set or tmux not configured for 24-bit RGB.

**Fix**: Apply the two settings above, then restart Grok.

**Verify**: Run `/terminal-setup`. Expect `color truecolor` and `themes all`. If `color` is `256` or `basic`, the issues section has the unlock fix.

### Problem: Clipboard problems

Grok writes to the clipboard through up to three routes, which match the **Clipboard routes** section of `/terminal-setup`:

- **native** — Grok always writes to the native OS clipboard first.
- **tmux buffer** — inside tmux, Grok also writes to the tmux paste buffer (`tmux load-buffer`).
- **OSC 52** — Grok emits the OSC 52 escape sequence so the outer terminal updates its clipboard. Grok always emits OSC 52 inside tmux. Outside tmux, it emits OSC 52 on Linux, over SSH, or in a container without a display.

**Linux Wayland**: on compositors that support the data-control protocol (GNOME 48+, KDE, Sway, Hyprland — the `data-control` line in `/terminal-setup` shows `yes`) copies work even if the terminal loses focus mid-copy. On older compositors (GNOME 46/47), keep the terminal focused until the copy toast confirms, and install the `wl-clipboard` package (provides `wl-copy`) for the most reliable route — Grok shows a startup warning when this applies. If data-control misbehaves on your compositor, set `GROK_CLIPBOARD_NO_DATA_CONTROL=1` to stop Grok from speaking that protocol entirely — copies then go through the CLI tools (`wl-copy`/`xclip`).

**Linux X11 selections**: X11 **PRIMARY** and **CLIPBOARD** are separate. Selecting text usually fills PRIMARY; an explicit Copy action fills CLIPBOARD. In Grok:

- An unmodified middle click reads PRIMARY only when `DISPLAY` is non-empty. Pure X11 can fall back to the native arboard reader. XWayland must have `xclip` or `xsel` on `PATH`; Grok deliberately disables the arboard fallback there so it cannot substitute Wayland PRIMARY.
- `Ctrl+V` reads CLIPBOARD only and never falls back to PRIMARY. To fill CLIPBOARD from a shell, run `printf %s "text" | xclip -selection clipboard`.
- `Shift+Insert` remains the terminal-native selected-text paste. Native Wayland PRIMARY behavior is compositor/terminal-specific and is not inferred from `TERM` or an incoming mouse event.

**SSH and selected text**: a remote Grok process usually cannot read the local terminal's PRIMARY or CLIPBOARD selection. Use terminal-native `Shift+Insert`, or hold `Shift` while middle-clicking when your terminal uses that gesture to bypass mouse reporting. The terminal then sends the local selection through the PTY instead of asking the remote process to access it.

**Known limitation — Apple Terminal + SSH**:
Apple Terminal ignores OSC 52, so copying from a Grok session over SSH can't reach your local clipboard. Use the workaround below.

**Temporary workaround**: Use `grok wrap ssh` instead of plain `ssh` (for example, `grok wrap ssh user@host`). It runs the command in a local PTY that intercepts OSC 52 sequences, including tmux-wrapped ones, and writes their contents to your local clipboard. The same command wraps anything else whose clipboard can't reach you — for example `grok wrap docker exec -it <container> bash` or `grok wrap kubectl exec -it <pod> -- bash`.

> **Warning**: `grok wrap` is **experimental** and may misbehave in some setups.

**iTerm2 setting**:
iTerm2 requires explicit permission for OSC 52:

1. iTerm2 → **Settings** → **General** → **Selection**
2. Enable **"Applications in terminal may access clipboard"**

This setting is off by default for security reasons. Without it, OSC 52 writes from Grok (or any TUI) will be ignored.

**Fix for other cases**:
- `set -g set-clipboard on` in tmux config
- For other terminals over SSH, switch to iTerm2, Ghostty, WezTerm, or Kitty for native OSC 52 support

### Problem: Fullscreen / alternate screen not activating (inline mode)

**Cause**: Zellij, tmux control mode (`tmux -CC`), or config set to `never`.

**Fix**:
- In Zellij or control mode, Grok intentionally runs inline (no alt screen).
- Set `[terminal] alt_screen = "always"` in `~/.grok/pager.toml` to force fullscreen.
- Use the CLI flag `--no-alt-screen` to disable alt-screen mode entirely (useful for debugging or when the alternate screen causes issues in your terminal).

### Problem: Zellij keybindings interfere with Grok (Ctrl+g, Ctrl+o, etc.)

Zellij intercepts many Ctrl/Alt key combinations before they reach full-screen TUIs like Grok.

**Best fix** (Zellij 0.41+): Switch to the **"Unlock-First (non-colliding)"** preset:

1. Press `Ctrl+o` → `c` (open Configuration)
2. Go to **"Change Mode Behavior"**
3. Select **"Unlock-First (non-colliding)"**
4. Press `Enter` (or `Ctrl+a` to save permanently)

After this, Zellij starts **locked**. Most keys pass through to Grok. Press `Ctrl+g` to temporarily unlock Zellij when you need its pane/session management.

Zellij recommends this approach for TUI users.

### Problem: `Ctrl+Enter` doesn't interject in WezTerm

**Cause**: WezTerm ships with the Kitty keyboard protocol disabled. Grok relies on it to tell `Ctrl+Enter` (interject) and `Shift+Enter` (send in multiline mode) apart from plain `Enter`. Most other terminals enable the protocol when Grok requests it.

For the same reason, in Apple Terminal, Grok binds `Ctrl+O` to interject.

**Fix**:

Add this after `config = wezterm.config_builder()` in `~/.config/wezterm/wezterm.lua`:

```lua
config.enable_kitty_keyboard = true
```

Reload (`Cmd+Shift+R` or restart WezTerm) and restart `grok`.

**Verify**: Run `/terminal-setup` inside Grok. While a turn is active, you see the interject hint, and `Ctrl+Enter` interjects.

**Quick workaround** (no global change):

```lua
table.insert(config.keys, {
  key = "Enter",
  mods = "CTRL",
  action = wezterm.action.SendString("\x1b[13;5u"),
})
```

### Problem: `Shift+Enter` doesn't insert a newline in VS Code

**Cause**: VS Code's integrated terminal (and the Cursor / Windsurf / Zed
forks) use xterm.js, which only partially implements the Kitty keyboard
protocol — it mis-encodes shifted printable keys (`!@#$%^&*()` arrive as
plain digits). Grok therefore never negotiates the protocol for these
terminals. Without it, xterm.js sends a bare `CR` for `Shift+Enter`,
byte-for-byte identical to plain `Enter`, so the chord can't be told apart
and the prompt submits.

This also affects VS Code reached **over SSH** (e.g. into a devbox or
container): `TERM_PROGRAM` isn't forwarded, so Grok sees an `Unknown`
terminal and skips the protocol for the same reason.

**Fix**: Use **`Alt+Enter`** to insert a newline. xterm.js delivers it
reliably as `ESC`+`CR` regardless of the keyboard protocol, and Grok's
prompt hint bar advertises `Alt+Enter: newline` whenever it detects this
situation. Run `/terminal-setup` to confirm — the `newline` row shows
`Alt+Enter` when `Shift+Enter` is unavailable.

### Problem: Mouse scrolling stops working (native scrollbar takes over)

If Grok's mouse-driven scrolling stops responding and your terminal falls back to its native scrollbar, mouse reporting is off.

**Apple Terminal**: Go to **View > Allow Mouse Reporting** (keyboard shortcut `Cmd+R`) to re-enable it. A checkmark appears next to the option when active.

**iTerm2**: Open **Settings** (`Cmd+,`) → **Profiles** → **Terminal** → ensure **"Enable mouse reporting"** is checked. Alternatively, restart iTerm2.

### Problem: Byobu + GNU screen

Byobu on screen has best-effort support only. Prefer Byobu on tmux.

---

## Still Stuck?

Run `/feedback` to report it.