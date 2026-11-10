#![deny(unsafe_op_in_unsafe_fn, clippy::undocumented_unsafe_blocks)]

#[cfg(not(target_os = "linux"))]
compile_error!("tinystatus supports Linux only");

mod layout;
mod status;
mod volume;
mod watch;

use std::env;
use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Instant;

use status::{Status, Update};

const HELP: &str = "usage: tinystatus [--once | --version | --help]\n\n\
    With no arguments, watch volume, keyboard layout and local time.\n\
    --once     Print one line, waiting up to 3 seconds for services.\n\
    --version  Print the version.\n\
    --help     Print this help.";

#[derive(Debug, Eq, PartialEq)]
enum Mode {
    Watch,
    Once,
    Version,
    Help,
}

fn mode(mut args: impl Iterator<Item = OsString>) -> Result<Mode, &'static str> {
    let mode = match args.next() {
        None => return Ok(Mode::Watch),
        Some(arg) if arg == "--once" => Mode::Once,
        Some(arg) if arg == "--version" => Mode::Version,
        Some(arg) if arg == "--help" => Mode::Help,
        Some(_) => return Err("unknown argument"),
    };
    if args.next().is_some() {
        Err("too many arguments")
    } else {
        Ok(mode)
    }
}

fn write_status(
    status: &Status,
    out: &mut impl Write,
    line: &mut String,
    previous: &mut String,
) -> io::Result<Instant> {
    // Capture the deadline before formatting or flushing can cross a second.
    let deadline = Instant::now() + status::until_next_second();
    status.render(line);
    if line != previous {
        writeln!(out, "{line}")?;
        out.flush()?;
        std::mem::swap(previous, line);
    }
    Ok(deadline)
}

fn run(mode: Mode, out: &mut impl Write) -> io::Result<()> {
    match mode {
        Mode::Help => return writeln!(out, "{HELP}"),
        Mode::Version => return writeln!(out, "tinystatus {}", env!("CARGO_PKG_VERSION")),
        _ => {}
    }
    let once = mode == Mode::Once;
    let (tx, rx) = mpsc::sync_channel(16);
    let volume_tx = tx.clone();
    thread::Builder::new()
        .name("volume".into())
        .spawn(move || {
            if !once {
                return volume::watch(volume_tx);
            }
            let volume = volume::current().unwrap_or_else(|error| {
                watch::log_error("pulse", error);
                None
            });
            let _ = volume_tx.send(Update::Volume(volume));
        })?;
    let layout_tx = tx.clone();
    thread::Builder::new()
        .name("layout".into())
        .spawn(move || {
            if !once {
                return layout::watch(layout_tx);
            }
            let layout = layout::current().unwrap_or_else(|error| {
                watch::log_error("sway", error);
                "??".into()
            });
            let _ = layout_tx.send(Update::Layout(layout));
        })?;
    drop(tx);
    let mut status = Status::default();
    let mut line = String::with_capacity(40);
    if once {
        let deadline = Instant::now() + watch::TIMEOUT;
        for _ in 0..2 {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(update) => status.update(update),
                Err(_) => break,
            }
        }
        status.render(&mut line);
        writeln!(out, "{line}")?;
        return out.flush();
    }
    let mut previous = String::with_capacity(40);
    loop {
        let deadline = write_status(&status, out, &mut line, &mut previous)?;
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(update) => {
                status.update(update);
                // Bound the batch so a busy service cannot starve the clock.
                for update in rx.try_iter().take(16) {
                    status.update(update);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("watchers stopped"));
            }
        }
    }
}

fn main() -> ExitCode {
    let mode = match mode(env::args_os().skip(1)) {
        Ok(mode) => mode,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "tinystatus: {error}\n{HELP}");
            return ExitCode::from(2);
        }
    };
    match run(mode, &mut io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            watch::log_error("output", error);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modes_and_rejects_extra_arguments() {
        for (args, expected) in [
            (vec![], Mode::Watch),
            (vec!["--once"], Mode::Once),
            (vec!["--version"], Mode::Version),
            (vec!["--help"], Mode::Help),
        ] {
            assert_eq!(mode(args.into_iter().map(OsString::from)), Ok(expected));
        }
        for args in [
            vec!["--wat"],
            vec!["--once", "--version"],
            vec!["--help", "extra"],
        ] {
            assert!(mode(args.into_iter().map(OsString::from)).is_err());
        }
    }

    #[test]
    fn reports_output_failures_without_panicking() {
        struct FailedOutput;
        impl Write for FailedOutput {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        for mode in [Mode::Help, Mode::Version] {
            assert_eq!(
                run(mode, &mut FailedOutput).unwrap_err().kind(),
                io::ErrorKind::BrokenPipe
            );
        }
    }

    #[test]
    fn slow_flush_does_not_postpone_the_clock_deadline() {
        struct SlowFlush;
        impl Write for SlowFlush {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                std::thread::sleep(std::time::Duration::from_millis(1100));
                Ok(())
            }
        }
        let deadline = write_status(
            &Status::default(),
            &mut SlowFlush,
            &mut String::new(),
            &mut String::new(),
        )
        .unwrap();
        assert!(deadline <= Instant::now());
    }

    #[test]
    fn version_uses_package_metadata() {
        let mut out = Vec::new();
        run(Mode::Version, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "tinystatus 0.4.1\n");
    }
}
