use std::fmt::Display;
use std::io::{self, Write};
use std::sync::mpsc::SyncSender;
use std::thread;
use std::time::Duration;

use crate::status::Update;

pub const TIMEOUT: Duration = Duration::from_secs(3);

pub fn log_error(context: &str, error: impl Display) {
    // Ignore stderr errors. Losing a diagnostic must not abort the status line.
    let _ = writeln!(
        io::stderr().lock(),
        "tinystatus: {context}: {:?}",
        error.to_string()
    );
}

pub fn watch<E: Display>(
    tx: SyncSender<Update>,
    name: &str,
    unavailable: impl Fn() -> Update,
    mut run: impl FnMut(&SyncSender<Update>) -> Result<(), E>,
) {
    let mut delay = Duration::from_secs(1);
    loop {
        let started = std::time::Instant::now();
        if let Err(error) = run(&tx) {
            log_error(name, error);
        }
        if tx.send(unavailable()).is_err() {
            return;
        }
        if started.elapsed() >= Duration::from_secs(30) {
            delay = Duration::from_secs(1);
        }
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}
