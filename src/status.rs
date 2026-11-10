use std::fmt::Write;
use std::mem::MaybeUninit;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Volume {
    pub percent: u16,
    pub muted: bool,
}

pub enum Update {
    Volume(Option<Volume>),
    Layout(String),
}

#[derive(Default)]
pub struct Status {
    pub volume: Option<Volume>,
    pub layout: Option<String>,
}

impl Status {
    pub fn update(&mut self, update: Update) {
        match update {
            Update::Volume(volume) => self.volume = volume,
            Update::Layout(layout) => self.layout = Some(layout),
        }
    }

    pub fn render(&self, line: &mut String) {
        line.clear();
        match self.volume {
            Some(volume) => {
                let percent = if volume.muted { 0 } else { volume.percent };
                write!(line, "{percent}%").unwrap();
            }
            None => line.push_str("??%"),
        }
        write!(line, " {} ", self.layout.as_deref().unwrap_or("??")).unwrap();
        let mut local = MaybeUninit::<libc::tm>::uninit();
        // SAFETY: time accepts a null output pointer. localtime_r initializes
        // local on success; both pointers remain valid throughout the call.
        let local = unsafe {
            let raw = libc::time(std::ptr::null_mut());
            if raw == -1 || libc::localtime_r(&raw, local.as_mut_ptr()).is_null() {
                line.push_str("??");
                return;
            }
            local.assume_init()
        };
        write_clock(line, &local);
    }
}

fn write_clock(out: &mut String, local: &libc::tm) {
    write!(
        out,
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {}",
        i64::from(local.tm_year) + 1900,
        local.tm_mon + 1,
        local.tm_mday,
        match local.tm_hour % 12 {
            0 => 12,
            hour => hour,
        },
        local.tm_min,
        local.tm_sec,
        if local.tm_hour < 12 { "AM" } else { "PM" },
    )
    .unwrap();
}

pub fn until_next_second() -> Duration {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Duration::from_secs(1) - Duration::from_nanos(u64::from(elapsed.subsec_nanos()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_and_muted_output() {
        let mut status = Status::default();
        let mut line = String::new();
        status.render(&mut line);
        assert!(line.starts_with("??% ?? "));
        status.update(Update::Volume(Some(Volume {
            percent: 42,
            muted: true,
        })));
        status.update(Update::Layout("us".into()));
        status.render(&mut line);
        assert!(line.starts_with("0% us "));
        assert!(!line.chars().any(char::is_control));
    }

    #[test]
    fn formats_midnight_noon_and_evening() {
        // SAFETY: zero is valid for every integer and pointer field of tm.
        let mut local: libc::tm = unsafe { std::mem::zeroed() };
        local.tm_year = 126;
        local.tm_mon = 10;
        local.tm_mday = 10;
        for (hour, suffix) in [(0, "12:00:00 AM"), (12, "12:00:00 PM"), (23, "11:00:00 PM")] {
            local.tm_hour = hour;
            let mut out = String::new();
            write_clock(&mut out, &local);
            assert_eq!(out, format!("2026-11-10 {suffix}"));
        }
    }

    #[test]
    fn does_not_truncate_large_percentages() {
        let status = Status {
            volume: Some(Volume {
                percent: 1000,
                muted: false,
            }),
            layout: None,
        };
        let mut line = String::new();
        status.render(&mut line);
        assert!(line.starts_with("1000% ?? "));
    }
}
