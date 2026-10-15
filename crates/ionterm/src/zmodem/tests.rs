use std::io::Write;
use std::sync::mpsc;

use super::*;

/// A PTY writer that collects what a session writes.
fn capture() -> (SharedWriter, Arc<Mutex<Vec<u8>>>) {
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            lock(&self.0).extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let bytes = Arc::new(Mutex::new(Vec::new()));
    (Arc::new(Mutex::new(Box::new(Sink(bytes.clone())))), bytes)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ionterm-zmodem-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn detector_holds_back_only_header_starts() {
    let mut detector = Detector::default();
    let (shown, found) = detector.scan(b"hello **");
    assert_eq!((&*shown, found.is_none()), (&b"hello **"[..], true));
    let header = wake_receiver();
    let (shown, found) = detector.scan(&header[..6]);
    assert_eq!((&*shown, found.is_none()), (&b""[..], true));
    let (shown, found) = detector.scan(&header[6..]);
    assert!(shown.is_empty());
    assert_eq!(found.map(|f| f.role), Some(Role::Receive));
    // Something that only looks like a header goes to the terminal.
    let (shown, found) = detector.scan(b"x**\x18B00zz rest");
    assert_eq!((&*shown, found.is_none()), (&b"x**\x18B00zz rest"[..], true));
}

/// Our sender and our receiver talking to each other.
#[test]
fn sends_to_itself() {
    let source = temp_dir("loop-src");
    let downloads = temp_dir("loop-dst");
    let big: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 256) as u8).collect();
    std::fs::write(source.join("big.bin"), &big).unwrap();
    std::fs::write(source.join("empty"), b"").unwrap();
    std::fs::write(source.join("with space's.txt"), b"\x18\x11\x13@\r@\rcontrol bytes").unwrap();
    std::fs::write(downloads.join("empty"), b"taken").unwrap();

    let (send_writer, to_receiver) = capture();
    let (receive_writer, to_sender) = capture();
    let send_shared = Shared::new(PathBuf::new());
    send_shared.queue_files(vec![source.join("big.bin"), source.join("empty"), source.join("with space's.txt")]);
    let receive_shared = Shared::new(downloads.clone());
    let (events_tx, events) = mpsc::channel();
    let tx = events_tx.clone();
    let mut sender = Session::new(Role::Send, send_writer, send_shared, move |e| tx.send(("send", e)).unwrap());
    let tx = events_tx;
    let mut receiver = Session::new(Role::Receive, receive_writer, receive_shared, move |e| tx.send(("receive", e)).unwrap());

    // `sz` starts with ZRQINIT; the receiver's ZRINIT answer starts the sender.
    assert!(receiver.feed(&wake_receiver()).is_none());
    let (mut sender_done, mut receiver_done) = (false, false);
    for _ in 0..1000 {
        if sender_done && receiver_done {
            break;
        }
        let bytes = std::mem::take(&mut *lock(&to_sender));
        if !sender_done && !bytes.is_empty() {
            sender_done = sender.feed(&bytes).is_some();
        }
        let bytes = std::mem::take(&mut *lock(&to_receiver));
        if !receiver_done && !bytes.is_empty() {
            receiver_done = receiver.feed(&bytes).is_some();
        }
    }
    assert!(sender_done && receiver_done, "the sessions didn't finish");
    assert_eq!(std::fs::read(downloads.join("big.bin")).unwrap(), big);
    assert_eq!(std::fs::read(downloads.join("empty (1)")).unwrap(), b"");
    assert_eq!(std::fs::read(downloads.join("empty")).unwrap(), b"taken");
    assert_eq!(std::fs::read(downloads.join("with space's.txt")).unwrap(), b"\x18\x11\x13@\r@\rcontrol bytes");
    let events: Vec<_> = events.try_iter().collect();
    let failures: Vec<_> = events.iter().filter(|(_, e)| matches!(e, Event::FileFinished(Err(_)) | Event::Ended(Some(_)))).collect();
    assert!(failures.is_empty(), "{failures:?}");
    let _ = std::fs::remove_dir_all(source);
    let _ = std::fs::remove_dir_all(downloads);
}

//#region Against lrzsz

/// Runs one lrzsz program in a PTY, with a session on our side as the PTY
/// reader would run it. `IONTERM_LRZSZ` names the folder with `lrzsz-sz`
/// and `lrzsz-rz` (Arch's names).
fn run_lrzsz(program: &str, args: &[&str], cwd: &Path, shared: Arc<Shared>) -> (Vec<Event>, Vec<u8>) {
    run_lrzsz_with(program, args, cwd, shared, |_, _, _| {})
}

/// `run_lrzsz`, calling `react` with each event the way the window would
/// see it, with the PTY writer and the shared state to act on.
fn run_lrzsz_with(
    program: &str,
    args: &[&str],
    cwd: &Path,
    shared: Arc<Shared>,
    react: impl Fn(&Event, &SharedWriter, &Arc<Shared>) + Send + Sync + 'static,
) -> (Vec<Event>, Vec<u8>) {
    let react = Arc::new(react);
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    let dir = std::env::var("IONTERM_LRZSZ").expect("IONTERM_LRZSZ");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(Path::new(&dir).join(program));
    command.args(args);
    command.cwd(cwd);
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let writer: SharedWriter = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
    let (tx, rx) = mpsc::channel();
    let mut detector = Detector::default();
    let mut session: Option<Session> = None;
    let mut terminal = Vec::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(Instant::now() < deadline, "lrzsz took too long");
        let n = match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let mut data = buffer[..n].to_vec();
        loop {
            if let Some(active) = &mut session {
                match active.feed(&data) {
                    Some(rest) => {
                        session = None;
                        data = rest;
                    }
                    None => break,
                }
            }
            let (shown, found) = detector.scan(&data);
            terminal.extend_from_slice(&shown);
            match found {
                Some(zmodem_start) => {
                    let (role, start) = (zmodem_start.role, zmodem_start.bytes);
                    let (tx, react, writer_for_react, shared_for_react) = (tx.clone(), react.clone(), writer.clone(), shared.clone());
                    session = Some(Session::new(role, writer.clone(), shared.clone(), move |e| {
                        react(&e, &writer_for_react, &shared_for_react);
                        tx.send(e).unwrap();
                    }));
                    data = start;
                }
                None => break,
            }
        }
    }
    let _ = child.wait();
    (rx.try_iter().collect(), terminal)
}

/// `IONTERM_LRZSZ=<dir> cargo test -p ionterm lrzsz -- --ignored`
#[test]
#[ignore]
fn receives_from_lrzsz() {
    let source = temp_dir("sz-src");
    let downloads = temp_dir("sz-dst");
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    std::fs::write(source.join("big.bin"), &big).unwrap();
    std::fs::write(source.join("note.txt"), b"hi\n").unwrap();
    let shared = Shared::new(downloads.clone());
    let (events, terminal) = run_lrzsz("lrzsz-sz", &["big.bin", "note.txt"], &source, shared);
    eprintln!("terminal: {:?}", String::from_utf8_lossy(&terminal));
    eprintln!("events: {:?}", events.iter().filter(|e| !matches!(e, Event::Progress(_))).collect::<Vec<_>>());
    assert!(matches!(events.last(), Some(Event::Ended(None))));
    assert_eq!(std::fs::read(downloads.join("big.bin")).unwrap(), big);
    assert_eq!(std::fs::read(downloads.join("note.txt")).unwrap(), b"hi\n");
    let _ = std::fs::remove_dir_all(source);
    let _ = std::fs::remove_dir_all(downloads);
}

#[test]
#[ignore]
fn sends_to_lrzsz() {
    let source = temp_dir("rz-src");
    let target = temp_dir("rz-dst");
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i.wrapping_mul(2_246_822_519) >> 11) as u8).collect();
    std::fs::write(source.join("big.bin"), &big).unwrap();
    std::fs::write(source.join("all bytes"), (0..=255u8).collect::<Vec<_>>()).unwrap();
    let shared = Shared::new(PathBuf::new());
    shared.queue_files(vec![source.join("big.bin"), source.join("all bytes")]);
    let (events, terminal) = run_lrzsz("lrzsz-rz", &[], &target, shared);
    eprintln!("terminal: {:?}", String::from_utf8_lossy(&terminal));
    eprintln!("events: {:?}", events.iter().filter(|e| !matches!(e, Event::Progress(_))).collect::<Vec<_>>());
    assert!(matches!(events.last(), Some(Event::Ended(None))));
    assert_eq!(std::fs::read(target.join("big.bin")).unwrap(), big);
    assert_eq!(std::fs::read(target.join("all bytes")).unwrap(), (0..=255u8).collect::<Vec<_>>());
    let _ = std::fs::remove_dir_all(source);
    let _ = std::fs::remove_dir_all(target);
}

//#endregion

/// `rz` with nothing queued: the session asks, the "picker" answers a
/// second later, and the ZRQINIT nudge gets `rz` going again at once.
#[test]
#[ignore]
fn sends_to_lrzsz_after_asking() {
    let source = temp_dir("ask-src");
    let target = temp_dir("ask-dst");
    std::fs::write(source.join("picked.txt"), b"picked later").unwrap();
    let picked = source.join("picked.txt");
    let started = Instant::now();
    let (events, _) = run_lrzsz_with("lrzsz-rz", &[], &target, Shared::new(PathBuf::new()), move |event, writer, shared| {
        if matches!(event, Event::NeedFiles) {
            let (writer, shared, picked) = (writer.clone(), shared.clone(), picked.clone());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(1));
                shared.queue_files(vec![picked]);
                lock(&writer).write_all(&wake_receiver()).unwrap();
            });
        }
    });
    assert!(matches!(events.first(), Some(Event::NeedFiles)), "{events:?}");
    assert!(matches!(events.last(), Some(Event::Ended(None))), "{events:?}");
    assert_eq!(std::fs::read(target.join("picked.txt")).unwrap(), b"picked later");
    // Well before rz's own 10 s retry.
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    let _ = std::fs::remove_dir_all(source);
    let _ = std::fs::remove_dir_all(target);
}

/// Cancelling a download halfway stops `sz` and leaves no file behind.
#[test]
#[ignore]
fn cancels_a_download_from_lrzsz() {
    let source = temp_dir("cancel-src");
    let downloads = temp_dir("cancel-dst");
    std::fs::write(source.join("huge.bin"), vec![0x5au8; 200_000_000]).unwrap();
    let shared = Shared::new(downloads.clone());
    let shared_after = shared.clone();
    let (events, terminal) = run_lrzsz_with("lrzsz-sz", &["huge.bin", "huge.bin"], &source, shared, |event, writer, shared| {
        if let Event::Progress(bytes) = event
            && *bytes > 1_000_000
            && !shared.cancel.load(Ordering::Acquire)
        {
            // What the window does when the transfer's × is clicked.
            shared.cancel(|bytes| lock(writer).write_all(bytes).unwrap());
        }
    });
    let shown = String::from_utf8_lossy(&terminal);
    eprintln!("terminal: {shown:?}");
    // The data still in flight was dropped, not shown, and `sz` wrapped up.
    assert!(!shown.contains("ZZZZ"), "data reached the terminal");
    assert!(events.iter().any(|e| matches!(e, Event::Ended(Some(_)))), "{events:?}");
    assert!(!shared_after.is_active(), "the session didn't end");
    let left: Vec<_> = std::fs::read_dir(&downloads).unwrap().flatten().map(|e| e.file_name()).collect();
    assert!(left.is_empty(), "left behind: {left:?}");
    let _ = std::fs::remove_dir_all(source);
    let _ = std::fs::remove_dir_all(downloads);
}


#[test]
fn skips_only_protocol_after_a_cancel() {
    assert_eq!(skip_frame_tail(b"\x18h\xe7\x06k\x18\xd1\x1b]133;D\x1b\\"), b"\x1b]133;D\x1b\\");
    assert_eq!(skip_frame_tail(b"plain text\r\n"), b"plain text\r\n");
    assert_eq!(skip_frame_tail(b"\x1b[K"), b"\x1b[K");
}
