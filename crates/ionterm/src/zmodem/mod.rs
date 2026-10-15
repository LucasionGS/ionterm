//! ZMODEM transfers started from the terminal: `sz <file>` on the server
//! sends files to ionterm, `rz` receives files from it.
//!
//! The PTY reader watches the output for the hex header either program
//! opens with (`Detector`), then hands the bytes to a `Session` instead of
//! the terminal until the transfer ends. Replies go straight to the PTY.
//! Implemented here rather than with lrzsz, which Windows doesn't have.
//!
//! - Received files go to the download folder, renamed `name (1).ext` when
//!   the name is taken (the sender can't wait for a question).
//! - `rz` sends the files dropped on the pane before it ran, or asks for
//!   them with the file picker (`Event::NeedFiles`).
//! - Commands the server asks to run (ZCOMMAND) are refused.
//! - Cancelling a download answers the sender with ZSKIP, which makes it
//!   stop and wrap up the session normally; the data already on its way is
//!   decoded and dropped, so nothing reaches the screen. Only if the sender
//!   doesn't wrap up in time does ionterm abort with CANs.

pub mod frame;

use std::borrow::Cow;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use frame::{Decoder, End, Escaper, Header, Kind, Token};

use crate::files::transfer::{self, Direction};
use crate::pty::SharedWriter;

/// What makes the other side give up: CANs, then backspaces to erase them
/// from a shell's line in case nothing was listening.
pub const ABORT: &[u8] = &[24, 24, 24, 24, 24, 24, 24, 24, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8];
/// Files dropped for the next `rz` wait this long.
const QUEUED_FILES_TIME: Duration = Duration::from_secs(600);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// Bytes per data subpacket when sending.
const CHUNK: usize = 8 * 1024;
/// After cancelling a download, how long the sender gets to wrap up before
/// it's aborted the hard way.
const CANCEL_TIME: Duration = Duration::from_secs(5);
/// A ZRINIT this long after our ZFILE means the ZFILE got lost; sooner, it
/// was already on its way and is answered by the ZFILE anyway.
const REOFFER_AFTER: Duration = Duration::from_secs(3);

/// The ZRQINIT header: sent to a waiting `rz`, it makes it repeat its
/// ZRINIT at once, so a session waiting for the file picker resumes.
pub fn wake_receiver() -> Vec<u8> {
    frame::hex_header(&Header::new(Kind::RqInit))
}

/// Per-pane state shared between the PTY reader and the window.
pub struct Shared {
    /// Set by the window to stop the running session.
    pub cancel: AtomicBool,
    /// A session is running: keyboard input must not reach the PTY.
    active: AtomicBool,
    /// `rz` is waiting for files to be chosen.
    waiting: AtomicBool,
    /// A session receives (`sz` sends): cancelling is the session's job.
    receiving: AtomicBool,
    /// While a cancelled download winds down: when to stop waiting for it
    /// even if nothing more arrives, so typing works again.
    cancel_until: Mutex<Option<Instant>>,
    /// Files waiting for the next `rz`, and since when.
    queued: Mutex<Option<(Vec<PathBuf>, Instant)>>,
    download_dir: Mutex<PathBuf>,
}

impl Shared {
    pub fn new(download_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            cancel: AtomicBool::new(false),
            active: AtomicBool::new(false),
            waiting: AtomicBool::new(false),
            receiving: AtomicBool::new(false),
            cancel_until: Mutex::new(None),
            queued: Mutex::new(None),
            download_dir: Mutex::new(download_dir),
        })
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire) && lock(&self.cancel_until).is_none_or(|until| Instant::now() < until)
    }

    /// Stops the running session. A download is wound down by the session
    /// itself (ZSKIP); anything else is told to give up with CANs, written
    /// with `write`.
    pub fn cancel(&self, write: impl FnOnce(&[u8])) {
        self.cancel.store(true, Ordering::Release);
        if !self.receiving.load(Ordering::Acquire) {
            write(ABORT);
        }
    }

    /// Whether a running `rz` waits for files (queue some, then nudge it
    /// with `wake_receiver`).
    pub fn is_waiting(&self) -> bool {
        self.waiting.load(Ordering::Acquire)
    }

    /// Files for the next `rz` to receive.
    pub fn queue_files(&self, paths: Vec<PathBuf>) {
        *lock(&self.queued) = Some((paths, Instant::now()));
    }

    fn take_files(&self) -> Option<Vec<PathBuf>> {
        let (paths, at) = lock(&self.queued).take()?;
        (at.elapsed() < QUEUED_FILES_TIME).then_some(paths)
    }

    pub fn set_download_dir(&self, dir: PathBuf) {
        *lock(&self.download_dir) = dir;
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What a session tells the window.
#[derive(Debug)]
pub enum Event {
    /// `rz` is waiting and no files were queued: ask for some.
    NeedFiles,
    FileStarted {
        direction: Direction,
        name: String,
        local_path: PathBuf,
        size: Option<u64>,
    },
    /// Bytes of the current file so far.
    Progress(u64),
    FileFinished(Result<(), String>),
    /// The session is over; `Some` says why it failed.
    Ended(Option<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// `sz` on the server sends; we receive.
    Receive,
    /// `rz` on the server receives; we send.
    Send,
}

//#region Detector

/// A transfer starting: who sends, and the bytes from its first header on.
pub struct Start {
    pub role: Role,
    pub bytes: Vec<u8>,
}

/// Finds the start of a transfer in terminal output: a complete, valid
/// ZRQINIT (from `sz`) or ZRINIT (from `rz`) hex header.
#[derive(Default)]
pub struct Detector {
    /// A possible header cut off at the end of the last read, held back
    /// from the terminal until it's known.
    probe: Vec<u8>,
}

impl Detector {
    /// Splits output into what the terminal shows and, when a transfer
    /// starts, the role and the bytes from its header on.
    pub fn scan<'a>(&mut self, data: &'a [u8]) -> (Cow<'a, [u8]>, Option<Start>) {
        let buffer: Cow<'a, [u8]> = if self.probe.is_empty() {
            Cow::Borrowed(data)
        } else {
            let mut joined = std::mem::take(&mut self.probe);
            joined.extend_from_slice(data);
            Cow::Owned(joined)
        };
        // Headers start `**` ZDLE; ZDLE is rare in normal output.
        let mut from = 0;
        while let Some(found) = buffer[from..].windows(3).position(|w| w == b"**\x18") {
            let at = from + found;
            match frame::probe_hex_header(&buffer[at..]) {
                Some(Some((header, _))) => {
                    let role = match header.kind {
                        Kind::RqInit => Some(Role::Receive),
                        Kind::RInit => Some(Role::Send),
                        _ => None,
                    };
                    if let Some(role) = role {
                        let bytes = buffer[at..].to_vec();
                        return (cut(buffer, at), Some(Start { role, bytes }));
                    }
                    from = at + 1;
                }
                Some(None) => from = at + 1,
                None => {
                    // Cut off: wait for the rest before showing it.
                    self.probe = buffer[at..].to_vec();
                    return (cut(buffer, at), None);
                }
            }
        }
        // A header split right after its ZDLE.
        if buffer.ends_with(b"**\x18") {
            let at = buffer.len() - 3;
            self.probe = buffer[at..].to_vec();
            return (cut(buffer, at), None);
        }
        (buffer, None)
    }
}

/// The first `len` bytes of a buffer, borrowed when it was.
fn cut(buffer: Cow<'_, [u8]>, len: usize) -> Cow<'_, [u8]> {
    match buffer {
        Cow::Borrowed(b) => Cow::Borrowed(&b[..len]),
        Cow::Owned(mut v) => {
            v.truncate(len);
            Cow::Owned(v)
        }
    }
}

//#endregion

//#region Session

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    // Receiving.
    /// Waiting for ZFILE (or ZFIN).
    WaitFile,
    /// Receiving a file's data.
    Receiving,
    /// Answered ZFIN; the sender ends with "OO".
    ExpectOver,
    // Sending.
    /// Waiting for `rz` to be ready and for files to send.
    WaitReceiver,
    /// Sent ZFILE, waiting for where to start (ZRPOS) or ZSKIP.
    WaitPosition,
    /// Sent a file's ZEOF, waiting for ZRINIT (or a ZRPOS to resend from).
    WaitFileDone,
    /// Sent ZFIN, waiting for the receiver's.
    WaitFin,
}

/// A file being received.
struct Incoming {
    file: File,
    part: PathBuf,
    path: PathBuf,
    offset: u64,
    mtime: Option<SystemTime>,
    /// Data after a damaged packet is dropped until the sender restarts
    /// from our position.
    skipping: bool,
}

/// A file being sent.
struct Outgoing {
    file: File,
    size: u64,
    /// The ZFILE subpacket, to offer the file again if it got lost.
    info: Vec<u8>,
    offered_at: Instant,
}

/// What handling a token leads to.
enum Flow {
    Continue,
    /// The sender is done; it says "OO" next.
    ExpectOver,
    Finished(Option<String>),
}

pub struct Session {
    role: Role,
    state: State,
    decoder: Decoder,
    escaper: Escaper,
    /// Use CRC-32 (the receiver supports it).
    crc32: bool,
    writer: SharedWriter,
    shared: Arc<Shared>,
    events: Box<dyn FnMut(Event) + Send>,
    /// The header the next data subpacket belongs to.
    data_for: Option<Kind>,
    incoming: Option<Incoming>,
    queue: VecDeque<PathBuf>,
    outgoing: Option<Outgoing>,
    asked_for_files: bool,
    last_progress: Option<Instant>,
    /// The window was told the session is over.
    reported: bool,
    /// Cancelled while receiving: every file is skipped until the sender
    /// says ZFIN, or this deadline passes.
    cancelling: Option<Instant>,
}

impl Session {
    /// Starts a session; feed it the bytes the detector handed over.
    pub fn new(role: Role, writer: SharedWriter, shared: Arc<Shared>, events: impl FnMut(Event) + Send + 'static) -> Self {
        shared.active.store(true, Ordering::Release);
        shared.cancel.store(false, Ordering::Release);
        shared.receiving.store(role == Role::Receive, Ordering::Release);
        Self {
            role,
            state: match role {
                Role::Receive => State::WaitFile,
                Role::Send => State::WaitReceiver,
            },
            decoder: Decoder::new(),
            escaper: Escaper::default(),
            crc32: false,
            writer,
            shared,
            events: Box::new(events),
            data_for: None,
            incoming: None,
            queue: VecDeque::new(),
            outgoing: None,
            asked_for_files: false,
            last_progress: None,
            reported: false,
            cancelling: None,
        }
    }

    /// Takes bytes from the PTY. Returns what's left for the terminal once
    /// the session is over.
    pub fn feed(&mut self, bytes: &[u8]) -> Option<Vec<u8>> {
        if self.shared.cancel.swap(false, Ordering::AcqRel) {
            self.report_end(Some("Cancelled.".into()));
            if self.role == Role::Send {
                // The window told `rz` to give up; what follows is the shell's.
                self.mark_done();
                let mut rest = self.decoder.take_rest();
                rest.extend_from_slice(bytes);
                return Some(rest);
            }
            self.start_cancel();
        }
        if self.cancelling.is_some_and(|until| Instant::now() >= until) {
            // The sender didn't wrap up: abort, and give the terminal back.
            tracing::debug!("ZMODEM sender ignored the cancel; aborting");
            self.write(ABORT);
            self.mark_done();
            let mut rest = self.decoder.take_rest();
            rest.extend_from_slice(bytes);
            return Some(rest);
        }
        if self.state == State::ExpectOver {
            // "OO", possibly split, then the shell again.
            let over = bytes.iter().take(2).take_while(|&&b| b == b'O').count();
            self.finish(None);
            return Some(bytes[over..].to_vec());
        }
        self.decoder.push(bytes);
        while let Some(token) = self.decoder.next() {
            match self.handle(token) {
                Flow::Continue => {}
                Flow::ExpectOver => {
                    self.state = State::ExpectOver;
                    let rest = self.decoder.take_rest();
                    let rest = trim_line_end(&rest);
                    return if rest.is_empty() { None } else { self.feed(rest) };
                }
                Flow::Finished(error) => {
                    self.finish(error);
                    let rest = self.decoder.take_rest();
                    return Some(skip_frame_tail(trim_line_end(&rest)).to_vec());
                }
            }
        }
        None
    }

    /// Cancels a download: the current file is dropped, and the sender is
    /// asked to skip it (and any after it) and finish.
    fn start_cancel(&mut self) {
        tracing::debug!("ZMODEM cancelled; asking the sender to skip");
        let until = Instant::now() + CANCEL_TIME;
        self.cancelling = Some(until);
        *lock(&self.shared.cancel_until) = Some(until);
        self.data_for = None;
        self.decoder.expect_header();
        self.write_hex(Header::new(Kind::Skip));
    }

    fn handle(&mut self, token: Token) -> Flow {
        match token {
            Token::Cancelled => Flow::Finished(Some("Cancelled on the server.".into())),
            Token::Header(header) => match self.role {
                Role::Receive => self.receive_header(header),
                Role::Send => self.send_header(header),
            },
            Token::Data { data, end, ok } => self.receive_data(data, end, ok),
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        let mut writer = lock(&self.writer);
        if let Err(error) = writer.write_all(bytes).and_then(|()| writer.flush()) {
            tracing::debug!("ZMODEM write failed: {error}");
        }
    }

    fn write_hex(&mut self, header: Header) {
        self.write(&frame::hex_header(&header));
    }

    fn write_binary(&mut self, header: Header) {
        let bytes = frame::binary_header(&header, self.crc32, &mut self.escaper);
        self.write(&bytes);
    }

    fn progress(&mut self, bytes: u64, force: bool) {
        let now = Instant::now();
        if force || self.last_progress.is_none_or(|at| now.duration_since(at) >= PROGRESS_INTERVAL) {
            self.last_progress = Some(now);
            (self.events)(Event::Progress(bytes));
        }
    }

    /// Ends the session: an unfinished file is dropped (received) or
    /// reported (sent).
    fn finish(&mut self, error: Option<String>) {
        self.report_end(error);
        self.mark_done();
    }

    fn mark_done(&self) {
        self.shared.waiting.store(false, Ordering::Release);
        self.shared.receiving.store(false, Ordering::Release);
        *lock(&self.shared.cancel_until) = None;
        self.shared.active.store(false, Ordering::Release);
    }

    /// The connection closed in the middle of the session.
    pub fn abandon(&mut self) {
        self.finish(Some("The connection closed.".into()));
    }

    /// Drops an unfinished file and tells the window the session is over.
    fn report_end(&mut self, error: Option<String>) {
        if std::mem::replace(&mut self.reported, true) {
            return;
        }
        if let Some(incoming) = self.incoming.take() {
            drop(incoming.file);
            let _ = std::fs::remove_file(&incoming.part);
            (self.events)(Event::FileFinished(Err(error.clone().unwrap_or_else(|| "Interrupted.".into()))));
        }
        if self.outgoing.take().is_some() {
            (self.events)(Event::FileFinished(Err(error.clone().unwrap_or_else(|| "Interrupted.".into()))));
        }
        (self.events)(Event::Ended(error));
    }

    //#region Receiving

    fn receive_header(&mut self, header: Header) -> Flow {
        match header.kind {
            Kind::RqInit => {
                self.write_hex(Header::with_flags(Kind::RInit, frame::CANFDX | frame::CANOVIO | frame::CANFC32, 0));
            }
            Kind::SInit => self.data_for = Some(Kind::SInit),
            Kind::File => self.data_for = Some(Kind::File),
            // A skipped file's data still on its way, and its end.
            Kind::Data | Kind::Eof if self.cancelling.is_some() => self.decoder.expect_header(),
            Kind::Data => {
                let Some(incoming) = &mut self.incoming else {
                    // Data without a file: start over.
                    self.write_hex(Header::with_flags(Kind::RInit, frame::CANFDX | frame::CANOVIO | frame::CANFC32, 0));
                    self.decoder.expect_header();
                    return Flow::Continue;
                };
                if u64::from(header.position()) == incoming.offset {
                    incoming.skipping = false;
                    self.data_for = Some(Kind::Data);
                } else {
                    incoming.skipping = true;
                    let offset = incoming.offset as u32;
                    self.decoder.expect_header();
                    self.write_hex(Header::with_position(Kind::RPos, offset));
                }
            }
            Kind::Eof => {
                let at_end = self.incoming.as_ref().is_some_and(|i| u64::from(header.position()) == i.offset);
                // An early ZEOF crossed our ZRPOS: the sender will resend.
                if at_end {
                    let result = self.save_file();
                    (self.events)(Event::FileFinished(result.clone()));
                    if let Err(error) = result {
                        self.write(ABORT);
                        return Flow::Finished(Some(error));
                    }
                    self.state = State::WaitFile;
                    self.write_hex(Header::with_flags(Kind::RInit, frame::CANFDX | frame::CANOVIO | frame::CANFC32, 0));
                }
            }
            Kind::Fin => {
                self.write_hex(Header::new(Kind::Fin));
                return Flow::ExpectOver;
            }
            Kind::Command => {
                self.write(ABORT);
                return Flow::Finished(Some("The server asked to run a command, which ionterm refuses.".into()));
            }
            Kind::Abort | Kind::FErr | Kind::Can => return Flow::Finished(Some("The sender gave up.".into())),
            _ => {}
        }
        Flow::Continue
    }

    fn receive_data(&mut self, data: Vec<u8>, end: End, ok: bool) -> Flow {
        match self.data_for {
            Some(Kind::SInit) => {
                self.data_for = None;
                self.write_hex(Header::new(Kind::Ack));
            }
            Some(Kind::File) if self.cancelling.is_some() => {
                self.data_for = None;
                self.write_hex(Header::new(Kind::Skip));
            }
            Some(Kind::File) => {
                self.data_for = None;
                if !ok {
                    self.write_hex(Header::new(Kind::Nak));
                    return Flow::Continue;
                }
                if let Err(error) = self.start_file(&data) {
                    self.write(ABORT);
                    return Flow::Finished(Some(error));
                }
                self.state = State::Receiving;
                self.write_hex(Header::with_position(Kind::RPos, 0));
            }
            Some(Kind::Data) => {
                let Some(incoming) = &mut self.incoming else { return Flow::Continue };
                if incoming.skipping {
                    return Flow::Continue;
                }
                if !ok {
                    incoming.skipping = true;
                    let offset = incoming.offset as u32;
                    self.data_for = None;
                    self.write_hex(Header::with_position(Kind::RPos, offset));
                    return Flow::Continue;
                }
                if let Err(error) = incoming.file.write_all(&data) {
                    self.write(ABORT);
                    return Flow::Finished(Some(format!("Could not save: {error}")));
                }
                incoming.offset += data.len() as u64;
                let offset = incoming.offset;
                if matches!(end, End::Q | End::W) {
                    self.write_hex(Header::with_position(Kind::Ack, offset as u32));
                }
                if !end.more() {
                    self.data_for = None;
                }
                self.progress(offset, false);
            }
            _ => {}
        }
        Flow::Continue
    }

    /// Opens a new file from ZFILE's info: `name NUL size mtime mode … NUL`,
    /// numbers in decimal except the octal mtime and mode.
    fn start_file(&mut self, info: &[u8]) -> Result<(), String> {
        let mut parts = info.split(|&b| b == 0);
        let raw_name = String::from_utf8_lossy(parts.next().unwrap_or_default()).into_owned();
        let fields = String::from_utf8_lossy(parts.next().unwrap_or_default()).into_owned();
        let mut fields = fields.split_whitespace();
        let size = fields.next().and_then(|s| s.parse::<u64>().ok());
        let mtime = fields
            .next()
            .and_then(|s| u64::from_str_radix(s, 8).ok())
            .filter(|&t| t > 0)
            .map(|t| SystemTime::UNIX_EPOCH + Duration::from_secs(t));

        let name = safe_name(&raw_name);
        let dir = lock(&self.shared.download_dir).clone();
        std::fs::create_dir_all(&dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
        let mut path = dir.join(&name);
        if std::fs::symlink_metadata(&path).is_ok() {
            path = transfer::free_local_path(&dir, &name);
        }
        let mut part = path.clone().into_os_string();
        part.push(".part");
        let part = PathBuf::from(part);
        let file = File::create(&part).map_err(|e| format!("Could not save {}: {e}", path.display()))?;
        (self.events)(Event::FileStarted {
            direction: Direction::Download,
            name: path.file_name().map_or(name, |n| n.to_string_lossy().into_owned()),
            local_path: path.clone(),
            size,
        });
        self.last_progress = None;
        self.incoming = Some(Incoming {
            file,
            part,
            path,
            offset: 0,
            mtime,
            skipping: false,
        });
        Ok(())
    }

    fn save_file(&mut self) -> Result<(), String> {
        let incoming = self.incoming.take().ok_or("No file was being received.")?;
        self.progress(incoming.offset, true);
        let Incoming {
            file, part, path, mtime, ..
        } = incoming;
        let result = (|| -> std::io::Result<()> {
            file.sync_all()?;
            if let Some(mtime) = mtime {
                let _ = file.set_modified(mtime);
            }
            drop(file);
            std::fs::rename(&part, &path)
        })();
        result.map_err(|e| {
            let _ = std::fs::remove_file(&part);
            format!("Could not save {}: {e}", path.display())
        })
    }

    //#endregion

    //#region Sending

    fn send_header(&mut self, header: Header) -> Flow {
        match (header.kind, self.state) {
            (Kind::RInit, State::WaitReceiver) => {
                self.crc32 = header.f0() & frame::CANFC32 != 0;
                self.escaper.escape_controls = header.f0() & frame::ESCCTL != 0;
                if self.queue.is_empty() {
                    match self.shared.take_files() {
                        Some(paths) => {
                            self.shared.waiting.store(false, Ordering::Release);
                            self.queue.extend(paths);
                        }
                        None => {
                            if !self.asked_for_files {
                                self.asked_for_files = true;
                                self.shared.waiting.store(true, Ordering::Release);
                                (self.events)(Event::NeedFiles);
                            }
                            return Flow::Continue;
                        }
                    }
                }
                return self.next_file();
            }
            // Our ZFILE got lost or damaged: offer the file again.
            (Kind::Nak, State::WaitPosition) => self.offer_file(),
            (Kind::RInit, State::WaitPosition) => {
                if self.outgoing.as_ref().is_some_and(|o| o.offered_at.elapsed() >= REOFFER_AFTER) {
                    self.offer_file();
                }
            }
            (Kind::RPos, State::WaitPosition | State::WaitFileDone) => return self.send_data(u64::from(header.position())),
            (Kind::Skip, State::WaitPosition) => {
                self.outgoing = None;
                (self.events)(Event::FileFinished(Err("Skipped: the server didn't want it (already there?).".into())));
                return self.next_file();
            }
            (Kind::Crc, State::WaitPosition) => {
                let crc = self.outgoing.as_mut().and_then(|o| file_crc(&mut o.file).ok()).unwrap_or(0);
                self.write_hex(Header::with_position(Kind::Crc, crc));
            }
            (Kind::RInit, State::WaitFileDone) => {
                if let Some(outgoing) = self.outgoing.take() {
                    self.progress(outgoing.size, true);
                    (self.events)(Event::FileFinished(Ok(())));
                }
                return self.next_file();
            }
            (Kind::Fin, State::WaitFin) => {
                self.write(b"OO");
                return Flow::Finished(None);
            }
            (Kind::Abort | Kind::FErr | Kind::Can, _) => return Flow::Finished(Some("The receiver gave up.".into())),
            _ => {}
        }
        Flow::Continue
    }

    /// Sends ZFILE for the current file.
    fn offer_file(&mut self) {
        let Some(info) = self.outgoing.as_mut().map(|o| {
            o.offered_at = Instant::now();
            o.info.clone()
        }) else {
            return;
        };
        self.write_binary(Header::with_flags(Kind::File, frame::ZCBIN, 0));
        let packet = frame::subpacket(&info, End::W, self.crc32, &mut self.escaper);
        self.write(&packet);
    }

    /// Offers the next queued file, or ends the batch with ZFIN.
    fn next_file(&mut self) -> Flow {
        while let Some(path) = self.queue.pop_front() {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let opened = File::open(&path).and_then(|file| Ok((file.metadata()?, file)));
            let (meta, file) = match opened {
                Ok((meta, _)) if meta.is_dir() => {
                    self.file_failed(&name, &path, "Folders can't be sent with rz; drop it on a connection opened by ionterm.");
                    continue;
                }
                Ok(pair) => pair,
                Err(error) => {
                    self.file_failed(&name, &path, &format!("Could not open it: {error}"));
                    continue;
                }
            };
            let size = meta.len();
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            #[cfg(unix)]
            let mode = std::os::unix::fs::MetadataExt::mode(&meta);
            #[cfg(not(unix))]
            let mode = 0o100644;
            let left: u64 = self.queue.iter().filter_map(|p| p.metadata().ok()).map(|m| m.len()).sum();
            let mut info = name.clone().into_bytes();
            info.push(0);
            info.extend(format!("{size} {mtime:o} {mode:o} 0 {} {}", self.queue.len() + 1, size + left).into_bytes());
            info.push(0);

            (self.events)(Event::FileStarted {
                direction: Direction::Upload,
                name,
                local_path: path.clone(),
                size: Some(size),
            });
            self.last_progress = None;
            self.outgoing = Some(Outgoing {
                file,
                size,
                info,
                offered_at: Instant::now(),
            });
            self.offer_file();
            self.state = State::WaitPosition;
            return Flow::Continue;
        }
        self.state = State::WaitFin;
        self.write_hex(Header::new(Kind::Fin));
        Flow::Continue
    }

    fn file_failed(&mut self, name: &str, path: &Path, error: &str) {
        (self.events)(Event::FileStarted {
            direction: Direction::Upload,
            name: name.to_string(),
            local_path: path.to_path_buf(),
            size: None,
        });
        (self.events)(Event::FileFinished(Err(error.to_string())));
    }

    /// Streams the current file from `position`, then sends ZEOF. Blocks
    /// the reader until done; the receiver only talks back on errors, and
    /// its ZRPOS waits in the PTY until then.
    fn send_data(&mut self, position: u64) -> Flow {
        let Some(mut outgoing) = self.outgoing.take() else { return Flow::Continue };
        let size = outgoing.size;
        if let Err(error) = outgoing.file.seek(SeekFrom::Start(position)) {
            self.write(ABORT);
            self.outgoing = Some(outgoing);
            return Flow::Finished(Some(format!("Could not read the file: {error}")));
        }
        self.write_binary(Header::with_position(Kind::Data, position as u32));
        let mut offset = position;
        let mut buffer = vec![0u8; CHUNK];
        loop {
            if self.shared.cancel.swap(false, Ordering::AcqRel) {
                self.outgoing = Some(outgoing);
                return Flow::Finished(Some("Cancelled.".into()));
            }
            let n = match read_full(&mut outgoing.file, &mut buffer) {
                Ok(n) => n,
                Err(error) => {
                    self.write(ABORT);
                    self.outgoing = Some(outgoing);
                    return Flow::Finished(Some(format!("Could not read the file: {error}")));
                }
            };
            offset += n as u64;
            let end = if n < CHUNK || offset >= size { End::E } else { End::G };
            let packet = frame::subpacket(&buffer[..n], end, self.crc32, &mut self.escaper);
            self.write(&packet);
            self.progress(offset, false);
            if end == End::E {
                break;
            }
        }
        self.write_binary(Header::with_position(Kind::Eof, offset as u32));
        self.outgoing = Some(outgoing);
        self.state = State::WaitFileDone;
        Flow::Continue
    }

    //#endregion
}

/// Reads until the buffer is full or the file ends.
fn read_full(file: &mut File, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// The CRC-32 of a whole file, for a receiver deciding whether to resume.
fn file_crc(file: &mut File) -> std::io::Result<u32> {
    file.seek(SeekFrom::Start(0))?;
    let mut crc = 0xFFFF_FFFF;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        crc = frame::crc32_update(crc, &buffer[..n]);
    }
    Ok(crc ^ 0xFFFF_FFFF)
}

/// A sent file name made safe: no folders, nothing hidden by accident of
/// `..`, and characters Windows can't store replaced.
fn safe_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or_default().trim();
    let name = transfer::local_name(base);
    if name.is_empty() || name == "." || name == ".." { "download".into() } else { name }
}

/// The last bytes of a frame that a cancelled sender flushes after its CANs.
/// The shell's output starts at a line end or an escape sequence; anything
/// before that within a few bytes is protocol.
///
/// Example: `ZDLE h \xe7 \x06 k ZDLE \xd1` then `\e]133;D…`
fn skip_frame_tail(bytes: &[u8]) -> &[u8] {
    match bytes.iter().take(64).position(|&b| matches!(b, b'\r' | b'\n' | 0x1b)) {
        Some(at) if bytes[..at].iter().any(|&b| b == frame::ZDLE || !b.is_ascii()) => &bytes[at..],
        _ => bytes,
    }
}

/// The line end after a hex header (CR, LF with or without its high bit,
/// XON), which isn't the shell's.
fn trim_line_end(bytes: &[u8]) -> &[u8] {
    let skip = bytes.iter().take(3).take_while(|&&b| matches!(b, b'\r' | b'\n' | 0x8a | 0x11)).count();
    &bytes[skip..]
}

//#endregion

#[cfg(test)]
mod tests;
