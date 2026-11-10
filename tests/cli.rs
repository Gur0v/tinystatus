use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tinystatus"));
    command
        .env_remove("SWAYSOCK")
        .env("PULSE_SERVER", "unix:/nonexistent/tinystatus");
    command
}

struct SocketDir(PathBuf);
impl SocketDir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "tinystatus-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        Self(path)
    }
    fn socket(&self) -> PathBuf {
        self.0.join("sway.sock")
    }
}
impl Drop for SocketDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.socket());
        let _ = std::fs::remove_dir(&self.0);
    }
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Process {
    fn wait(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            assert!(
                Instant::now() < deadline,
                "tinystatus did not exit before deadline"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn reply(stream: &mut UnixStream, kind: u32, payload: &[u8]) -> std::io::Result<()> {
    let mut packet = b"i3-ipc".to_vec();
    packet.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    packet.extend_from_slice(&kind.to_ne_bytes());
    packet.extend_from_slice(payload);
    stream.write_all(&packet)
}

#[test]
fn flags_and_unavailable_services() {
    let version = command().arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(version.stdout, b"tinystatus 0.4.1\n");
    let help = command().arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(help.stdout.starts_with(b"usage: tinystatus"));
    assert_eq!(
        command().arg("--bad").output().unwrap().status.code(),
        Some(2)
    );
    let snapshot = command().arg("--once").output().unwrap();
    assert!(snapshot.status.success());
    assert!(snapshot.stdout.starts_with(b"??% ?? "));
    assert_eq!(
        snapshot
            .stdout
            .iter()
            .filter(|byte| **byte == b'\n')
            .count(),
        1
    );
}

#[test]
fn once_has_a_deadline_when_sway_stalls() {
    let dir = SocketDir::new();
    let _listener = UnixListener::bind(dir.socket()).unwrap();
    let started = Instant::now();
    let mut child = Process(
        command()
            .arg("--once")
            .env("SWAYSOCK", dir.socket())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    child.wait(Duration::from_secs(5));
    let mut output = String::new();
    child
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut output)
        .unwrap();
    assert!(output.starts_with("??% ?? "));
    assert!(started.elapsed() >= Duration::from_secs(2));
}

#[test]
fn sway_reuses_subscription_handles_queued_events_and_reconnects() {
    let dir = SocketDir::new();
    let listener = UnixListener::bind(dir.socket()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(("English (US)", None::<UnixStream>)));
    let connections = Arc::new(AtomicUsize::new(0));
    let queries = Arc::new(AtomicUsize::new(0));
    let server = {
        let stop = stop.clone();
        let state = state.clone();
        let connections = connections.clone();
        let queries = queries.clone();
        thread::spawn(move || {
            let mut handlers = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        connections.fetch_add(1, Ordering::Relaxed);
                        let state = state.clone();
                        let stop = stop.clone();
                        let queries = queries.clone();
                        handlers.push(thread::spawn(move || {
                            stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
                            loop {
                                if stop.load(Ordering::Relaxed) { break; }
                                let mut header = [0; 14];
                                match stream.read_exact(&mut header) {
                                    Ok(()) => {}
                                    Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
                                    Err(_) => break,
                                }
                                assert_eq!(&header[..6], b"i3-ipc");
                                let len = u32::from_ne_bytes(header[6..10].try_into().unwrap()) as usize;
                                let kind = u32::from_ne_bytes(header[10..14].try_into().unwrap());
                                assert!(len < 1024);
                                let mut payload = vec![0; len];
                                stream.read_exact(&mut payload).unwrap();
                                let mut state = state.lock().unwrap();
                                match kind {
                                    2 => {
                                        assert_eq!(payload, br#"["input"]"#);
                                        reply(&mut stream, 2, br#"{"success":true}"#).unwrap();
                                        state.1 = Some(stream.try_clone().unwrap());
                                    }
                                    100 => {
                                        queries.fetch_add(1, Ordering::Relaxed);
                                        // Real IPC can interleave events with command replies.
                                        reply(&mut stream, 0x8000_0015, br#"{"change":"xkb_layout","input":{"type":"keyboard"}}"#).unwrap();
                                        let payload = format!(r#"[{{"type":"keyboard","identifier":"first","xkb_active_layout_name":"{}"}}]"#, state.0);
                                        reply(&mut stream, 100, payload.as_bytes()).unwrap();
                                    }
                                    _ => panic!("unexpected request {kind}"),
                                }
                            }
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(error) => panic!("{error}"),
                }
            }
            for handler in handlers {
                handler.join().unwrap();
            }
        })
    };
    let mut child = Process(
        command()
            .env("SWAYSOCK", dir.socket())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (lines, output) = mpsc::channel();
    let (close, closed) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if closed.try_recv().is_ok() || lines.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let until = |text: &str| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let line = output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            if line.contains(text) {
                break;
            }
        }
    };
    until(" us ");
    {
        let mut state = state.lock().unwrap();
        state.0 = "Russian";
        reply(
            state.1.as_mut().unwrap(),
            0x8000_0015,
            br#"{"change":"xkb_layout","input":{"type":"keyboard","identifier":"first","xkb_active_layout_name":"Russian"}}"#,
        )
        .unwrap();
    }
    until(" ru ");
    assert_eq!(
        queries.load(Ordering::Relaxed),
        1,
        "ordinary layout switches should not query all inputs"
    );
    assert_eq!(
        connections.load(Ordering::Relaxed),
        1,
        "input events should reuse the socket"
    );
    {
        let mut state = state.lock().unwrap();
        state
            .1
            .take()
            .unwrap()
            .shutdown(std::net::Shutdown::Both)
            .unwrap();
        state.0 = "Ukrainian";
    }
    until(" ?? ");
    until(" ua ");
    assert_eq!(connections.load(Ordering::Relaxed), 2);
    close.send(()).unwrap();
    reader.join().unwrap();
    child.wait(Duration::from_secs(3));
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
}

#[test]
fn failed_stderr_does_not_abort_snapshot() {
    // A read-only fd rejects writes, unlike Stdio::null().
    let stderr = std::fs::File::open("/dev/null").unwrap();
    let output = command().arg("--once").stderr(stderr).output().unwrap();
    assert!(output.status.success());
    assert!(output.stdout.starts_with(b"??% ?? "));
}
