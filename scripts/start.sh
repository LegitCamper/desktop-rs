#!/bin/sh
# Starts the long-lived desktop-rs surfaces. Intended as a compositor autostart
# entry: Niri `spawn-at-startup`, Hyprland `exec-once`, or a user systemd unit.
#
# The launcher is NOT started here. Bind it in the compositor instead, so the
# compositor keeps owning keybinds:
#   niri:     binds { Mod+D { spawn "desktop-rs" "launcher"; } }
#   hyprland: bind = SUPER, D, exec, desktop-rs launcher
set -eu

bin="${DESKTOP_RS_BIN:-desktop-rs}"
command -v "$bin" >/dev/null 2>&1 || bin="$(dirname "$0")/../target/release/desktop-rs"

"$bin" daemon &
"$bin" bar &
# Stopping this script stops both surfaces, so a compositor restart leaves none behind.
trap 'kill 0' INT TERM EXIT
wait
