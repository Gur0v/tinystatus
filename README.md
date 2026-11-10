# tinystatus

A tiny, opinionated status line generator for `swaybar`. No config. No surprises.

It watches three things: default sink volume, active keyboard layout, and the clock. When something changes, it prints a line:

```text
42% us 2026-04-24 09:49:57 PM
```

That's the whole program.

## Implementation

Event-driven Rust. No polling, no shelling out.

- `swayipc-async` for keyboard layout events
- `libpulse-binding` for volume and mute changes
- `src/layout.rs`, `src/volume.rs`, `src/clock.rs` feed into `src/main.rs`, which prints the line

## Philosophy

Small codebase. Hardcoded behavior on purpose. Talks to real APIs, not wrapper commands. Not a framework, not extensible by design. Does one job and stops.

Want to add a metric? The source is the plugin system. Your imagination is the limit.

## Scope

Supports `sway` and PulseAudio/PipeWire. Plain text output only. No i3, no X11, no shell fallbacks.

For X11/i3, use [v0.2.0](https://github.com/Gur0v/tinystatus/releases/tag/v0.2.0).

## Build

**Arch:**
```sh
sudo pacman -S rust pipewire-pulse libpulse
```

**Debian / Ubuntu:**
```sh
sudo apt install cargo libpulse-dev pipewire-pulse
```

```sh
cargo build --release
# binary: target/release/tinystatus
```

## Usage

```conf
bar {
    status_command /path/to/tinystatus
}
```

Install system-wide:

```sh
sudo install -m755 target/release/tinystatus /usr/local/bin/tinystatus
```

## Notes

Layout mappings: `English (US)` → `us`, `Russian` → `ru`, `Ukrainian` → `ua`. Anything else truncates to 3 lowercase ASCII characters. No PulseAudio → `??%`. No Sway IPC → `??`.
