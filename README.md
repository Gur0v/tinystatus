# tinystatus

A small status line for swaybar on Linux. Shows the default audio sink volume,
keyboard layout, and local time:

```text
42% us 2026-04-24 09:49:57 PM
```

Uses Sway IPC and the PulseAudio library directly, with PipeWire support through
pipewire-pulse. No config file or shell commands. Linux and Sway only.

## Build and install

Requires Rust 1.85 or later, Cargo, a C toolchain, pkg-config, and PulseAudio
development libraries.

Arch Linux:

```sh
sudo pacman -S --needed rust base-devel pkgconf libpulse
```

Debian / Ubuntu:

```sh
sudo apt install build-essential cargo pkg-config libpulse-dev
```

If your distribution ships Rust older than 1.85, install a newer toolchain first.

```sh
cargo build --release --locked
sudo install -m755 target/release/tinystatus /usr/local/bin/tinystatus
```

At runtime, you need Sway and either PulseAudio or PipeWire with pipewire-pulse.

## Usage

Add this to your Sway configuration:

```conf
bar {
    status_command /usr/local/bin/tinystatus
}
```

Running `tinystatus` without arguments prints a new line whenever the displayed
values change. It reads Sway's socket from `SWAYSOCK`, which Sway sets for programs
it starts.

```sh
tinystatus --once     # Print one snapshot and exit
tinystatus --version  # Print the version
tinystatus --help     # Show usage
```

`--once` waits up to three seconds for service state. In normal operation,
tinystatus retries lost connections with delays capped at 30 seconds. It does
not start an audio server.

## Output

- Volume: a percentage, or `0%` when muted. Unavailable audio shows `??%`.
- Layout: the first keyboard with a layout in Sway's input list. `English (US)`
  becomes `us`, `Russian` becomes `ru`, and `Ukrainian` becomes `ua`. Other names
  use their first three ASCII letters in lowercase. Unavailable layouts show `??`.
- Time: local time in `YYYY-MM-DD hh:mm:ss AM/PM` format, updated each second.
  A failed clock read shows `??`.

Diagnostics go to stderr. A closed output pipe is a normal exit. Other output
errors exit with status 1; invalid arguments exit with status 2. Missing services
use placeholders rather than stopping the program.

## License

Licensed under the GNU General Public License, version 3. See [LICENSE](LICENSE)
for the full text.
