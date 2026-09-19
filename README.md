# desktop-rs

Small Rust-native Linux shell/widget experiment. No GTK or Qt.

Current milestone: config-driven Wayland layer-shell surfaces with software-rendered boxes/text, live clocks, interactive application launcher, compositor workspaces/focused title, PipeWire audio through `pactl`, a StatusNotifier tray backend, and a native `org.freedesktop.Notifications` daemon.

## Run

Requirements:

- Rust 1.86 or newer
- Wayland session
- compositor implementing `wlr-layer-shell` (wlroots compositors, Niri, Hyprland, and KDE Plasma)
- `pipewire-pulse` and `pactl` for `Audio` widgets
- `ext-workspace-v1` for `Workspaces` and `wlr-foreign-toplevel-management-v1` for `ActiveWindow`; widgets stay empty/fallback when unavailable

```sh
cargo run -- bar           # full-width top bar with live clock
cargo run -- daemon        # owns org.freedesktop.Notifications, spawns popups
cargo run -- notification  # one popup; slides in, holds, slides out
cargo run -- launcher      # centered card with exclusive keyboard focus
```

`bar` is default. Stop persistent surfaces with `Ctrl-C` in launching terminal. Vanilla GNOME lacks `wlr-layer-shell`; startup fails with direct protocol error there.

## Compositor integration

The compositor owns keybinds and autostart; desktop-rs owns everything drawn. `scripts/start.sh` starts the two long-lived processes (`daemon` and `bar`) and kills both when it stops:

```
# niri config.kdl
spawn-at-startup "/path/to/desktop-rs/scripts/start.sh"
binds {
    Mod+D { spawn "desktop-rs" "launcher"; }
}
```

```
# hyprland.conf
exec-once = /path/to/desktop-rs/scripts/start.sh
bind = SUPER, D, exec, desktop-rs launcher
```

The script uses `desktop-rs` from `PATH`, falling back to `target/release/desktop-rs`; override with `DESKTOP_RS_BIN`.

The launcher is one-shot: each keypress spawns a fresh process that exits on `Esc`, launch, or focus loss. No IPC socket, so nothing to keep alive.

## Notifications

`desktop-rs daemon` claims `org.freedesktop.Notifications` and re-executes itself as `desktop-rs notification …` per notice, one at a time. Stop `dunst`/`mako`/`swaync` first — D-Bus grants the name to one owner, and the daemon exits with `name already taken on the bus` otherwise.

The `Notification` tree substitutes `{app}`, `{summary}`, and `{body}` inside any `Text` `value`:

```nbcl
Text { value = "{summary}" }
Text { value = "{body}" }
```

A client's `expire_timeout` overrides `animation.hold_ms`; `0` (never expire) is capped at 30s so a stuck notice cannot block the queue.

```sh
notify-send "Build finished" "42 tests passed"
```

## Config and examples

Single entry point:

- `$XDG_CONFIG_HOME/desktop-rs/config.nbcl`
- fallback: `~/.config/desktop-rs/config.nbcl`

First start embeds and writes complete starter bundle without network access:

- `config.nbcl`
- `themes/` with Tokyo Night, Gruvbox, Dracula, and all four Catppuccin flavors
- `examples/minimal.nbcl`, `examples/full.nbcl`, and `examples/launcher.nbcl`

Existing `config.nbcl` prevents all first-run writes. Existing user files never get overwritten or backfilled.

For manual installation from source:

```sh
mkdir -p ~/.config/desktop-rs
cp assets/config.nbcl ~/.config/desktop-rs/
cp -a assets/themes assets/examples ~/.config/desktop-rs/
```

Select palette by changing one import in `config.nbcl`:

```nbcl
import "themes/catppuccin-mocha.nbcl" as theme
```

Available files:

- `tokyo-night.nbcl`
- `gruvbox.nbcl`
- `dracula.nbcl`
- `catppuccin-latte.nbcl`
- `catppuccin-frappe.nbcl`
- `catppuccin-macchiato.nbcl`
- `catppuccin-mocha.nbcl`

Each exports `base`, `surface`, `text`, `muted`, `accent`, `warning`, `error`, and `border`.

## NBCL elements

[NBCL](https://nbcl-lang.github.io/docs) defines all three required surfaces: `Bar`, `Notification`, and `Launcher`. Supported child nodes: `Box`, `Text`, `Clock`, `Workspaces`, `ActiveWindow`, `Audio`, `AppSearch`, and `AppList`.

```nbcl
Bar {
    height = 40
    anchor = ["top", "left", "right"]
    layer = "top"
    exclusive_zone = 40
    background = theme.base
    padding = 6
    gap = 8
    align = "center"

    Workspaces {
        width = "fit"
        color = theme.text
        active_color = theme.accent
        active_background = theme.surface
    }
    ActiveWindow {
        width = "grow"
        color = theme.text
        max_chars = 60
        empty_value = "desktop-rs"
        text_align = "center"
    }
    Audio {
        width = "fit"
        color = theme.text
        step = 5
        max_volume = 100
    }
    Clock {
        width = "fit"
        format = "%a %b %-d  %H:%M:%S"
        color = theme.text
        font_size = 14
    }
}
```

Layout properties:

- `width`, `height`: non-negative integer, `"grow"`, or `"fit"`
- `direction`: `"row"` or `"column"`
- `align`, `text_align`: `"start"`, `"center"`, or `"end"`
- `padding`, `gap`: non-negative integer pixels

Text properties:

- `value`: UTF-8 string
- `color`: `#rrggbb` or `#rrggbbaa`
- `font_size`: integer pixels
- `font_family`: optional system font family
- `format`: `Clock` strftime format, validated while loading config

Dynamic widget properties:

- `ActiveWindow`: `max_chars`, `empty_value`
- `Workspaces`: `active_color`, `active_background`, `urgent_color`, `urgent_background`, `gap`
- `Tray`: `icon_size`, `gap`; primary click activates item, secondary click opens the DBusMenu popup (falling back to a `ContextMenu` D-Bus call when the item exposes no menu), wheel scroll sends vertical `Scroll`
- `Audio`: `step` percentage per wheel notch, `max_volume` safety ceiling; primary click toggles mute
- `AppList`: `rows`, `terminal` argv list for `Terminal=true` entries

Audio backend uses PipeWire through `pipewire-pulse`/`pactl`. Missing `pactl` renders `VOL --`; worker retries subscriptions without terminating bar.

NBCL map literals are whitespace-separated, not comma-separated:

```nbcl
margin = { top = 52 right = 12 }
```

Animation example:

```nbcl
animation = {
    slide = "right"
    distance = 400
    duration_ms = 220
    hold_ms = 4000
}
```

Omit `hold_ms` to slide in and remain visible. Moving phases use `wl_surface.frame`; hold completion uses a timer, avoiding continuous redraw while resting.

## Runtime layout

- `src/config/` — NBCL loading, XDG paths, embedded bundle writer
- `assets/` — default config, themes, copyable examples
- `src/platform/linux/window.rs` — layer-shell settings and templates
- `src/platform/linux/runtime.rs` — calloop, Wayland lifecycle, timers, keyboard/pointer state
- `src/platform/linux/notifications.rs` — `org.freedesktop.Notifications` daemon and popup queue
- `scripts/start.sh` — compositor autostart entry for `daemon` + `bar`
- `src/ui/element.rs` — retained element tree, row/column layout, hit testing
- `src/ui/text.rs` — cosmic-text shaping, fallback, ellipsis, glyph rasterization
- `src/ui/animation.rs` — slide-in/hold/slide-out timing
- `src/ui/style.rs` — colors and surface styles
- `src/ui/paint.rs` — rounded rectangles and premultiplied ARGB composition

## Current limits

- Scale changes trigger redraw, but true HiDPI buffer scaling is pending.
- One process owns one surface; the notification daemon works around this by re-executing itself per popup.
- Notifications show one at a time, without actions, icons, urgency styling, or `NotificationClosed`/`ActionInvoked` signals, so `notify-send --wait` does not return early.
- Properties use loose NBCL registration and typed conversion; unknown-property rejection is pending stable widget schemas.
- Workspace/output association and multi-monitor filtering need broader compositor testing.
- Audio requires `pipewire-pulse` and `pactl`; native PipeWire control is deferred until executable dependency becomes a measured problem.
- StatusNotifier discovery, live snapshots, named/pixmap icons, activation, scroll, and the DBusMenu popup work; menu item icons and nested popup surfaces are not drawn (submenus replace the popup contents in place).

## Next milestones

1. Launcher app-entry icons reusing the shared tray image pipeline.
2. Menu item icons and nested submenu surfaces.
3. Notification actions, icons, and urgency styling.
4. Multi-surface process support and config reload.
5. True HiDPI buffer scaling and strict per-node NBCL schemas.
