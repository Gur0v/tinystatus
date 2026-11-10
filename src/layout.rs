use std::env;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::SyncSender;
use std::time::Instant;

use serde_json::Value;

use crate::status::Update;
use crate::watch::{self, TIMEOUT};

const SUBSCRIBE: u32 = 2;
const GET_INPUTS: u32 = 100;
const INPUT_EVENT: u32 = 0x8000_0015;
const MAX_PAYLOAD: usize = 1024 * 1024;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn connect() -> io::Result<UnixStream> {
    let path = env::var_os("SWAYSOCK").ok_or_else(|| invalid("SWAYSOCK is not set"))?;
    connect_path(&path, TIMEOUT)
}

fn connect_path(path: &std::ffi::OsStr, timeout: std::time::Duration) -> io::Result<UnixStream> {
    // SAFETY: sockaddr_un contains only integers and bytes, all valid when zeroed.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_bytes();
    if bytes.is_empty() || bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
        return Err(invalid("invalid sway socket path"));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (out, &byte) in address.sun_path.iter_mut().zip(bytes) {
        *out = byte as libc::c_char;
    }
    let deadline = Instant::now() + timeout;
    loop {
        // Linux AF_UNIX nonblocking connect returns EAGAIN when the listener's
        // backlog is full. Retry a fresh socket rather than block on that listener.
        // SAFETY: socket has no pointer arguments and returns an owned fd or -1.
        let fd = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a newly created descriptor owned exclusively here.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: address is initialized, NUL terminated and lives through connect.
        let result = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                std::ptr::from_ref(&address).cast(),
                std::mem::size_of_val(&address) as libc::socklen_t,
            )
        };
        if result == 0 {
            let stream = UnixStream::from(fd);
            stream.set_nonblocking(false)?;
            stream.set_write_timeout(Some(TIMEOUT))?;
            return Ok(stream);
        }
        let error = io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::EAGAIN | libc::EINTR)) {
            return Err(error);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "sway connection timed out",
            ));
        }
        drop(fd);
        std::thread::sleep(remaining.min(std::time::Duration::from_millis(20)));
    }
}

fn request(stream: &mut UnixStream, kind: u32, payload: &[u8]) -> io::Result<()> {
    // One small write avoids a syscall for each header field.
    let mut packet = Vec::with_capacity(14 + payload.len());
    packet.extend_from_slice(b"i3-ipc");
    packet.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    packet.extend_from_slice(&kind.to_ne_bytes());
    packet.extend_from_slice(payload);
    stream.write_all(&packet)
}

struct DeadlineReader<'a> {
    stream: &'a UnixStream,
    deadline: Instant,
}

impl Read for DeadlineReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "sway reply timed out"))?;
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(buf)
    }
}

fn receive(stream: &mut impl Read) -> io::Result<(u32, Value)> {
    let mut header = [0; 14];
    stream.read_exact(&mut header)?;
    if &header[..6] != b"i3-ipc" {
        return Err(invalid("invalid sway IPC header"));
    }
    let len = u32::from_ne_bytes(header[6..10].try_into().unwrap()) as usize;
    let kind = u32::from_ne_bytes(header[10..14].try_into().unwrap());
    if len > MAX_PAYLOAD {
        return Err(invalid("sway IPC message exceeds 1 MiB"));
    }
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload)?;
    let value = serde_json::from_slice(&payload).map_err(|_| invalid("invalid sway IPC JSON"))?;
    Ok((kind, value))
}

fn code(name: &str) -> String {
    for (part, code) in [
        ("English (US)", "us"),
        ("Russian", "ru"),
        ("Ukrainian", "ua"),
    ] {
        if name.contains(part) {
            return code.into();
        }
    }
    let code: String = name
        .bytes()
        .filter(u8::is_ascii_alphabetic)
        .take(3)
        .map(|byte| char::from(byte.to_ascii_lowercase()))
        .collect();
    if code.is_empty() { "??".into() } else { code }
}

struct Selection {
    identifier: Option<String>,
    code: String,
}

fn from_inputs(inputs: &Value) -> io::Result<Selection> {
    let inputs = inputs
        .as_array()
        .ok_or_else(|| invalid("invalid sway input list"))?;
    for input in inputs {
        if input["type"] != "keyboard" {
            continue;
        }
        let active = input["xkb_active_layout_name"]
            .as_str()
            .filter(|name| !name.is_empty());
        let name = active.or_else(|| {
            let names = input["xkb_layout_names"].as_array()?;
            let index = input["xkb_active_layout_index"]
                .as_u64()
                .and_then(|index| usize::try_from(index).ok())
                .unwrap_or(0);
            names.get(index).or_else(|| names.first())?.as_str()
        });
        if let Some(name) = name.filter(|name| !name.is_empty()) {
            return Ok(Selection {
                identifier: input["identifier"].as_str().map(str::to_owned),
                code: code(name),
            });
        }
    }
    Ok(Selection {
        identifier: None,
        code: "??".into(),
    })
}

impl Selection {
    // Only a layout switch on the selected device can be applied without a query.
    // Topology/keymap changes and incomplete events need a fresh input list.
    fn apply_layout_event(&mut self, event: &Value) -> bool {
        if event["change"] != "xkb_layout" || self.identifier.is_none() {
            return false;
        }
        let input = &event["input"];
        let Some(identifier) = input["identifier"].as_str() else {
            return false;
        };
        if self.identifier.as_deref() != Some(identifier) {
            return true;
        }
        let Some(name) = input["xkb_active_layout_name"]
            .as_str()
            .filter(|name| !name.is_empty())
        else {
            return false;
        };
        self.code = code(name);
        true
    }
}

fn query(stream: &mut UnixStream) -> io::Result<Selection> {
    let deadline = Instant::now() + TIMEOUT;
    request(stream, GET_INPUTS, b"")?;
    loop {
        let (kind, inputs) = receive(&mut DeadlineReader { stream, deadline })?;
        match kind {
            GET_INPUTS => return from_inputs(&inputs),
            // Subscribed events can arrive before the query response. The reply
            // contains the state after those events, so no extra query is needed.
            INPUT_EVENT => {}
            _ => return Err(invalid("unexpected sway reply")),
        }
    }
}

pub fn current() -> io::Result<String> {
    Ok(query(&mut connect()?)?.code)
}

fn run(tx: &SyncSender<Update>) -> io::Result<()> {
    let mut stream = connect()?;
    request(&mut stream, SUBSCRIBE, br#"["input"]"#)?;
    let (kind, reply) = receive(&mut DeadlineReader {
        stream: &stream,
        deadline: Instant::now() + TIMEOUT,
    })?;
    if kind != SUBSCRIBE || reply["success"] != true {
        return Err(invalid("sway input subscription failed"));
    }
    // Subscribe before querying so startup changes are not lost.
    let mut selected = query(&mut stream)?;
    let mut previous = selected.code.clone();
    tx.send(Update::Layout(previous.clone()))
        .map_err(|_| invalid("output closed"))?;
    loop {
        stream.set_read_timeout(None)?;
        let mut first = [0];
        stream.read_exact(&mut first)?;
        let reader = DeadlineReader {
            stream: &stream,
            deadline: Instant::now() + TIMEOUT,
        };
        let (kind, event) = receive(&mut first.as_slice().chain(reader))?;
        if kind != INPUT_EVENT {
            return Err(invalid("unexpected sway event"));
        }
        if event["input"]["type"] != "keyboard" {
            continue;
        }
        if !selected.apply_layout_event(&event) {
            selected = query(&mut stream)?;
        }
        if selected.code != previous {
            tx.send(Update::Layout(selected.code.clone()))
                .map_err(|_| invalid("output closed"))?;
            previous.clone_from(&selected.code);
        }
    }
}

pub fn watch(tx: SyncSender<Update>) {
    watch::watch(tx, "sway", || Update::Layout("??".into()), run);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_keyboard_and_fallback_layout() {
        let inputs = json!([
            {"type": "pointer"},
            {"type": "keyboard", "xkb_layout_names": ["English (US)", "Ukrainian"], "xkb_active_layout_index": 1}
        ]);
        assert_eq!(from_inputs(&inputs).unwrap().code, "ua");
        assert_eq!(from_inputs(&json!([])).unwrap().code, "??");
        assert!(from_inputs(&json!({})).is_err());
    }

    #[test]
    fn active_name_takes_precedence() {
        let inputs = json!([{"type": "keyboard", "xkb_active_layout_name": "Russian", "xkb_layout_names": ["English (US)"]}]);
        assert_eq!(from_inputs(&inputs).unwrap().code, "ru");
    }

    #[test]
    fn layout_events_only_update_the_selected_keyboard() {
        let mut selected = from_inputs(&json!([{"type":"keyboard", "identifier":"first", "xkb_active_layout_name":"English (US)"}])).unwrap();
        assert!(selected.apply_layout_event(&json!({"change":"xkb_layout", "input":{"identifier":"second", "xkb_active_layout_name":"Russian"}})));
        assert_eq!(selected.code, "us");
        assert!(selected.apply_layout_event(&json!({"change":"xkb_layout", "input":{"identifier":"first", "xkb_active_layout_name":"Russian"}})));
        assert_eq!(selected.code, "ru");
        assert!(
            !selected
                .apply_layout_event(&json!({"change":"removed", "input":{"identifier":"first"}}))
        );
        assert!(
            !selected.apply_layout_event(
                &json!({"change":"xkb_layout", "input":{"identifier":"first"}})
            )
        );
    }

    #[test]
    fn full_socket_backlog_has_a_connection_deadline() {
        let path =
            std::env::temp_dir().join(format!("tinystatus-backlog-{}.sock", std::process::id()));
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        // SAFETY: listener owns a live listening socket; backlog zero is valid on Linux.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let _queued =
            connect_path(path.as_os_str(), std::time::Duration::from_millis(200)).unwrap();
        let started = Instant::now();
        let error =
            connect_path(path.as_os_str(), std::time::Duration::from_millis(100)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        drop(listener);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn layout_codes_are_bounded_ascii() {
        for (name, expected) in [
            ("English (US)", "us"),
            ("Russian", "ru"),
            ("Ukrainian", "ua"),
            ("German", "ger"),
            ("\n\x1b[31mFrench", "mfr"),
            ("日本語", "??"),
            ("", "??"),
        ] {
            assert_eq!(code(name), expected);
        }
    }

    #[test]
    fn rejects_oversized_invalid_and_truncated_packets() {
        let mut packet = b"i3-ipc".to_vec();
        packet.extend_from_slice(&((MAX_PAYLOAD + 1) as u32).to_ne_bytes());
        packet.extend_from_slice(&GET_INPUTS.to_ne_bytes());
        assert_eq!(
            receive(&mut packet.as_slice()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(receive(&mut b"not an IPC msg!".as_slice()).is_err());
        packet[6..10].copy_from_slice(&10u32.to_ne_bytes());
        assert!(receive(&mut packet.as_slice()).is_err());
    }

    #[test]
    fn rejects_invalid_json_and_excessive_nesting() {
        for payload in [b"{".to_vec(), [vec![b'['; 256], vec![b']'; 256]].concat()] {
            let mut packet = b"i3-ipc".to_vec();
            packet.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
            packet.extend_from_slice(&GET_INPUTS.to_ne_bytes());
            packet.extend_from_slice(&payload);
            assert_eq!(
                receive(&mut packet.as_slice()).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn deadline_applies_to_the_entire_packet() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let deadline = Instant::now() + std::time::Duration::from_millis(120);
        let sender = std::thread::spawn(move || {
            for byte in b"i3-ipc" {
                if peer.write_all(&[*byte]).is_err() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
        });
        let started = Instant::now();
        assert!(
            receive(&mut DeadlineReader {
                stream: &stream,
                deadline
            })
            .is_err()
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(300));
        drop(stream);
        sender.join().unwrap();
    }
}
