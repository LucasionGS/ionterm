//! Uploads and downloads: each runs on its own thread, streaming through an
//! `ssh` child process, and reports progress to the event loop.
//!
//! Files stream through `cat`; folders as a tar archive, packed and unpacked
//! here with the `tar` crate so no local `tar` is needed (Windows). Files land
//! under a `.part` name first and are renamed when complete, so a cancelled or
//! failed transfer never leaves a truncated file under the real name.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use winit::event_loop::EventLoopProxy;

use crate::app::UserEvent;
use crate::files::remote::{self, Remote};
use crate::layout::PaneId;

pub type TransferId = u64;

/// How many transfers run at once; the rest wait their turn.
const MAX_RUNNING: usize = 3;
/// Progress is reported at most this often per transfer.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// How long finished transfers stay in the list.
const LINGER: Duration = Duration::from_secs(6);
/// Failed ones stay longer, so the error can be read.
const LINGER_FAILED: Duration = Duration::from_secs(20);
const CHUNK: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Download,
    Upload,
}

/// What to transfer, and where to.
#[derive(Clone, Debug)]
pub struct TransferSpec {
    pub direction: Direction,
    pub remote: Remote,
    /// The remote file or folder: the source of a download, the destination
    /// of an upload (its final name).
    pub remote_path: String,
    /// The local file or folder: the destination of a download (its final
    /// name), the source of an upload.
    pub local_path: PathBuf,
    pub is_dir: bool,
    /// Download into an existing folder, keeping what's there.
    pub merge: bool,
}

impl TransferSpec {
    /// The name shown in the transfer list.
    pub fn name(&self) -> String {
        match self.direction {
            Direction::Download => self
                .local_path
                .file_name()
                .map_or_else(|| self.remote_path.clone(), |n| n.to_string_lossy().into_owned()),
            Direction::Upload => self.remote_path.rsplit('/').next().unwrap_or_default().to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferState {
    Queued,
    Running,
    Done,
    Failed(String),
    Cancelled,
}

#[derive(Debug)]
pub enum TransferEvent {
    /// Bytes moved so far, and the total once it's known.
    Progress(u64, Option<u64>),
    Finished(Result<(), String>),
}

pub struct Transfer {
    pub id: TransferId,
    /// The pane whose connection it uses; its panel shows it.
    pub pane: PaneId,
    pub name: String,
    pub direction: Direction,
    pub is_dir: bool,
    pub local_path: PathBuf,
    pub remote_path: String,
    pub bytes: u64,
    pub total: Option<u64>,
    pub state: TransferState,
    /// Bytes per second, smoothed.
    pub speed: f64,
    last_sample: Option<(Instant, u64)>,
    finished_at: Option<Instant>,
    spec: Option<TransferSpec>,
    control: Arc<Control>,
}

impl Transfer {
    /// Between 0 and 1, when the size is known.
    pub fn fraction(&self) -> Option<f32> {
        if self.state == TransferState::Done {
            return Some(1.0);
        }
        let total = self.total.filter(|&t| t > 0)?;
        // Folder sizes are estimates; never show done before it is.
        Some((self.bytes as f64 / total as f64).min(0.99) as f32)
    }

    pub fn is_active(&self) -> bool {
        matches!(self.state, TransferState::Queued | TransferState::Running)
    }
}

/// Shared between a transfer's thread and the UI, so the UI can stop it.
#[derive(Default)]
struct Control {
    cancelled: AtomicBool,
    child: Mutex<Option<Child>>,
}

impl Control {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(child) = self.child.lock().expect("transfer lock poisoned").as_mut() {
            let _ = child.kill();
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// What a finished transfer means for the rest of the app.
pub struct Finished {
    pub pane: PaneId,
    pub direction: Direction,
    pub remote_path: String,
    pub local_path: PathBuf,
    pub ok: bool,
}

/// Every transfer in the window: queued, running and recently finished.
pub struct Transfers {
    pub list: Vec<Transfer>,
    next_id: TransferId,
    proxy: EventLoopProxy<UserEvent>,
}

impl Transfers {
    pub fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        Self {
            list: Vec::new(),
            next_id: 1,
            proxy,
        }
    }

    /// Queues a transfer, starting it when a slot is free.
    pub fn add(&mut self, pane: PaneId, spec: TransferSpec) -> TransferId {
        let id = self.next_id;
        self.next_id += 1;
        self.list.push(Transfer {
            id,
            pane,
            name: spec.name(),
            direction: spec.direction,
            is_dir: spec.is_dir,
            local_path: spec.local_path.clone(),
            remote_path: spec.remote_path.clone(),
            bytes: 0,
            total: None,
            state: TransferState::Queued,
            speed: 0.0,
            last_sample: None,
            finished_at: None,
            spec: Some(spec),
            control: Arc::new(Control::default()),
        });
        self.pump();
        id
    }

    /// Starts queued transfers while there are free slots.
    fn pump(&mut self) {
        let mut running = self.list.iter().filter(|t| t.state == TransferState::Running).count();
        for transfer in &mut self.list {
            if running >= MAX_RUNNING {
                break;
            }
            if transfer.state != TransferState::Queued {
                continue;
            }
            let Some(spec) = transfer.spec.take() else { continue };
            transfer.state = TransferState::Running;
            transfer.last_sample = Some((Instant::now(), 0));
            running += 1;
            let (id, control, proxy) = (transfer.id, transfer.control.clone(), self.proxy.clone());
            let spawned = std::thread::Builder::new()
                .name(format!("transfer-{id}"))
                .spawn(move || run(id, spec, control, proxy));
            if let Err(error) = spawned {
                transfer.state = TransferState::Failed(format!("Could not start: {error}"));
                transfer.finished_at = Some(Instant::now());
            }
        }
    }

    pub fn cancel(&mut self, id: TransferId) {
        let Some(transfer) = self.list.iter_mut().find(|t| t.id == id) else { return };
        match transfer.state {
            TransferState::Queued => {
                transfer.state = TransferState::Cancelled;
                transfer.finished_at = Some(Instant::now());
                transfer.spec = None;
            }
            TransferState::Running => transfer.control.cancel(),
            _ => {}
        }
    }

    /// Applies a report from a transfer's thread.
    pub fn event(&mut self, id: TransferId, event: TransferEvent) -> Option<Finished> {
        let transfer = self.list.iter_mut().find(|t| t.id == id)?;
        match event {
            TransferEvent::Progress(bytes, total) => {
                let now = Instant::now();
                if let Some((at, before)) = transfer.last_sample {
                    let dt = now.duration_since(at).as_secs_f64();
                    if dt >= 0.25 {
                        let rate = bytes.saturating_sub(before) as f64 / dt;
                        transfer.speed = if transfer.speed == 0.0 { rate } else { transfer.speed * 0.6 + rate * 0.4 };
                        transfer.last_sample = Some((now, bytes));
                    }
                }
                transfer.bytes = bytes;
                if total.is_some() {
                    transfer.total = total;
                }
                None
            }
            TransferEvent::Finished(result) => {
                transfer.state = match result {
                    Ok(()) => TransferState::Done,
                    Err(_) if transfer.control.is_cancelled() => TransferState::Cancelled,
                    Err(message) => TransferState::Failed(message),
                };
                transfer.finished_at = Some(Instant::now());
                let finished = Finished {
                    pane: transfer.pane,
                    direction: transfer.direction,
                    remote_path: transfer.remote_path.clone(),
                    local_path: transfer.local_path.clone(),
                    ok: transfer.state == TransferState::Done,
                };
                self.pump();
                Some(finished)
            }
        }
    }

    /// Drops finished transfers that have been shown long enough. Returns
    /// whether the list changed, and when to check again.
    pub fn expire(&mut self, now: Instant) -> (bool, Option<Instant>) {
        let linger = |t: &Transfer| match t.state {
            TransferState::Failed(_) => LINGER_FAILED,
            _ => LINGER,
        };
        let before = self.list.len();
        self.list
            .retain(|t| t.finished_at.is_none_or(|at| now.duration_since(at) < linger(t)));
        let next = self.list.iter().filter_map(|t| t.finished_at.map(|at| at + linger(t))).min();
        (self.list.len() != before, next)
    }

    /// Removes one finished transfer from the list.
    pub fn dismiss(&mut self, id: TransferId) {
        self.list.retain(|t| t.id != id || t.is_active());
    }

    pub fn for_pane(&self, pane: PaneId) -> impl Iterator<Item = &Transfer> {
        self.list.iter().filter(move |t| t.pane == pane)
    }
}

impl Drop for Transfers {
    fn drop(&mut self) {
        for transfer in &self.list {
            transfer.control.cancel();
        }
    }
}

//#region Worker

/// Reports progress (bytes so far, total when known), throttled.
struct Reporter {
    send: Box<dyn FnMut(u64, Option<u64>) + Send>,
    total: Option<u64>,
    last: Option<Instant>,
}

impl Reporter {
    fn new(send: impl FnMut(u64, Option<u64>) + Send + 'static) -> Self {
        Self {
            send: Box::new(send),
            total: None,
            last: None,
        }
    }

    fn report(&mut self, bytes: u64, force: bool) {
        let now = Instant::now();
        if force || self.last.is_none_or(|at| now.duration_since(at) >= PROGRESS_INTERVAL) {
            self.last = Some(now);
            (self.send)(bytes, self.total);
        }
    }
}

fn run(id: TransferId, spec: TransferSpec, control: Arc<Control>, proxy: EventLoopProxy<UserEvent>) {
    let progress = proxy.clone();
    let mut reporter = Reporter::new(move |bytes, total| {
        let _ = progress.send_event(UserEvent::Transfer(id, TransferEvent::Progress(bytes, total)));
    });
    let result = transfer(&spec, &control, &mut reporter);
    let _ = proxy.send_event(UserEvent::Transfer(id, TransferEvent::Finished(result)));
}

/// Runs a transfer to the end on the current thread.
fn transfer(spec: &TransferSpec, control: &Control, reporter: &mut Reporter) -> Result<(), String> {
    let result = match (spec.direction, spec.is_dir) {
        (Direction::Download, false) => download_file(spec, control, reporter),
        (Direction::Download, true) => download_folder(spec, control, reporter),
        (Direction::Upload, false) => upload_file(spec, control, reporter),
        (Direction::Upload, true) => upload_folder(spec, control, reporter),
    };
    match result {
        Err(_) if control.is_cancelled() => Err("Cancelled.".into()),
        other => other,
    }
}

/// Registers a child so cancelling kills it. Fails if already cancelled.
fn adopt(control: &Control, child: Child) -> Result<(), String> {
    let mut slot = control.child.lock().expect("transfer lock poisoned");
    *slot = Some(child);
    if control.is_cancelled() {
        if let Some(child) = slot.as_mut() {
            let _ = child.kill();
        }
        return Err("Cancelled.".into());
    }
    Ok(())
}

fn take_stdout(control: &Control) -> Option<std::process::ChildStdout> {
    control.child.lock().expect("transfer lock poisoned").as_mut()?.stdout.take()
}

fn take_stdin(control: &Control) -> Option<std::process::ChildStdin> {
    control.child.lock().expect("transfer lock poisoned").as_mut()?.stdin.take()
}

/// Waits for the child and turns a failure into its error message. Polls
/// rather than blocking in `wait`, so cancelling can take the lock to kill it.
fn finish_child(control: &Control) -> Result<(), String> {
    loop {
        {
            let mut slot = control.child.lock().expect("transfer lock poisoned");
            let Some(child) = slot.as_mut() else { return Err("The transfer lost its process.".into()) };
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                if control.is_cancelled() {
                    return Err("Cancelled.".into());
                }
                return if status.success() { Ok(()) } else { Err(remote::child_error(child, status.code())) };
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `name.part` next to `path`.
fn part_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

fn download_file(spec: &TransferSpec, control: &Control, reporter: &mut Reporter) -> Result<(), String> {
    // The listing's size is a good guess; ask anyway in case it changed.
    reporter.total = remote_size(&spec.remote, &spec.remote_path);
    let child = spec.remote.spawn_cat(&spec.remote_path).map_err(|e| format!("Could not run ssh: {e}"))?;
    adopt(control, child)?;
    let mut stdout = take_stdout(control).ok_or("No output from ssh.")?;
    if let Some(parent) = spec.local_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Could not create {}: {e}", parent.display()))?;
    }
    let part = part_path(&spec.local_path);
    let copied = (|| -> io::Result<()> {
        let mut file = File::create(&part)?;
        copy_counting(&mut stdout, &mut file, reporter)?;
        file.flush()
    })();
    let result = copied
        .map_err(|e| format!("Could not save: {e}"))
        .and_then(|()| finish_child(control))
        .and_then(|()| std::fs::rename(&part, &spec.local_path).map_err(|e| format!("Could not save: {e}")));
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}

fn download_folder(spec: &TransferSpec, control: &Control, reporter: &mut Reporter) -> Result<(), String> {
    reporter.total = spec.remote.folder_size(&spec.remote_path);
    reporter.report(0, true);
    let child = spec
        .remote
        .spawn_tar_out(&spec.remote_path)
        .map_err(|e| format!("Could not run ssh: {e}"))?;
    adopt(control, child)?;
    let stdout = take_stdout(control).ok_or("No output from ssh.")?;
    let created = !spec.local_path.exists();
    let mut archive = tar::Archive::new(CountingReader {
        inner: stdout,
        count: 0,
        reporter,
    });
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);
    let unpacked = unpack_renamed(&mut archive, &spec.local_path);
    // Drain anything tar left unread so ssh can exit cleanly.
    let _ = io::copy(&mut archive.into_inner(), &mut io::sink());
    let result = unpacked.and_then(|()| finish_child(control));
    if result.is_err() && created && !spec.merge {
        let _ = std::fs::remove_dir_all(&spec.local_path);
    }
    result
}

/// Unpacks an archive of one folder into `dest`, whatever the folder was
/// called on the server. Entries that would land outside `dest` are skipped.
fn unpack_renamed<R: Read>(archive: &mut tar::Archive<R>, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("Could not create {}: {e}", dest.display()))?;
    for entry in archive.entries().map_err(|e| format!("Bad archive: {e}"))? {
        let mut entry = entry.map_err(|e| format!("Bad archive: {e}"))?;
        let path = entry.path().map_err(|e| format!("Bad archive: {e}"))?.into_owned();
        let mut components = path.components().filter(|c| !matches!(c, Component::CurDir));
        // The first component is the folder's own name.
        components.next();
        let rest: PathBuf = components.clone().collect();
        if components.any(|c| !matches!(c, Component::Normal(_))) {
            continue;
        }
        let target = dest.join(&rest);
        if rest.as_os_str().is_empty() {
            continue;
        }
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Hard links name another entry of the archive, which was renamed too.
        if entry.header().entry_type().is_hard_link() {
            let Some(link) = entry.link_name().ok().flatten() else { continue };
            let mut link_components = link.components().filter(|c| !matches!(c, Component::CurDir));
            link_components.next();
            let source = dest.join(link_components.collect::<PathBuf>());
            if std::fs::hard_link(&source, &target).is_err() {
                let _ = std::fs::copy(&source, &target);
            }
            continue;
        }
        entry
            .unpack(&target)
            .map_err(|e| format!("Could not write {}: {e}", target.display()))?;
    }
    Ok(())
}

fn upload_file(spec: &TransferSpec, control: &Control, reporter: &mut Reporter) -> Result<(), String> {
    let mut file = File::open(&spec.local_path).map_err(|e| format!("Could not open {}: {e}", spec.local_path.display()))?;
    reporter.total = file.metadata().ok().map(|m| m.len());
    let child = spec
        .remote
        .spawn_upload(&spec.remote_path)
        .map_err(|e| format!("Could not run ssh: {e}"))?;
    adopt(control, child)?;
    let mut stdin = take_stdin(control).ok_or("Could not write to ssh.")?;
    let copied = copy_counting(&mut file, &mut stdin, reporter);
    drop(stdin);
    let result = finish_child(control).and(copied.map(drop).map_err(|e| format!("Upload interrupted: {e}")));
    if result.is_err() {
        spec.remote.remove_part(&spec.remote_path);
    }
    result
}

fn upload_folder(spec: &TransferSpec, control: &Control, reporter: &mut Reporter) -> Result<(), String> {
    reporter.total = Some(local_size(&spec.local_path));
    reporter.report(0, true);
    let (parent, name) = match spec.remote_path.rsplit_once('/') {
        Some(("", name)) => ("/".to_string(), name.to_string()),
        Some((parent, name)) => (parent.to_string(), name.to_string()),
        None => (".".to_string(), spec.remote_path.clone()),
    };
    let child = spec.remote.spawn_tar_in(&parent).map_err(|e| format!("Could not run ssh: {e}"))?;
    adopt(control, child)?;
    let stdin = take_stdin(control).ok_or("Could not write to ssh.")?;
    let mut builder = tar::Builder::new(CountingWriter {
        inner: stdin,
        count: 0,
        reporter,
    });
    builder.follow_symlinks(false);
    let packed = builder
        .append_dir_all(&name, &spec.local_path)
        .and_then(|()| builder.finish())
        .map_err(|e| format!("Upload interrupted: {e}"));
    drop(builder);
    finish_child(control).and(packed)
}

/// Asks the server for a regular file's size (`None` when it can't tell;
/// devices and pipes have no size, and counting them would never end).
fn remote_size(remote: &Remote, path: &str) -> Option<u64> {
    let path = remote::quote(path);
    let output = remote.run(&format!("[ -f {path} ] && exec wc -c < {path}")).ok()?;
    String::from_utf8_lossy(&output).trim().parse().ok()
}

/// The total size of the files in a local folder.
pub fn local_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path)
        .map(|entries| entries.flatten().map(|e| local_size(&e.path())).sum())
        .unwrap_or(0)
}

fn copy_counting(from: &mut impl Read, to: &mut impl Write, reporter: &mut Reporter) -> io::Result<u64> {
    let mut buffer = vec![0; CHUNK];
    let mut total = 0;
    loop {
        let n = match from.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        to.write_all(&buffer[..n])?;
        total += n as u64;
        reporter.report(total, false);
    }
    reporter.report(total, true);
    Ok(total)
}

struct CountingReader<'a, R> {
    inner: R,
    count: u64,
    reporter: &'a mut Reporter,
}

impl<R: Read> Read for CountingReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count += n as u64;
        self.reporter.report(self.count, n == 0);
        Ok(n)
    }
}

struct CountingWriter<'a, W> {
    inner: W,
    count: u64,
    reporter: &'a mut Reporter,
}

impl<W: Write> Write for CountingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count += n as u64;
        self.reporter.report(self.count, false);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

//#endregion

//#region Names

/// `name (1).ext`, `name (2).ext`, … for the `n`th alternative to `name`.
pub fn numbered_name(name: &str, n: usize) -> String {
    // Keep the extension (and `.tar.gz`-style double ones) at the end.
    let split = match name.find('.') {
        Some(0) | None => name.len(),
        Some(i) if name[i..].starts_with(".tar.") => i,
        Some(_) => name.rfind('.').unwrap_or(name.len()),
    };
    let (stem, ext) = name.split_at(split);
    format!("{stem} ({n}){ext}")
}

/// A remote name made safe for the local filesystem: Windows forbids some
/// characters that Unix allows.
pub fn local_name(name: &str) -> String {
    if cfg!(windows) {
        name.chars()
            .map(|c| if "<>:\"\\|?*".contains(c) || c.is_control() { '_' } else { c })
            .collect()
    } else {
        name.to_string()
    }
}

/// The first `name (n)` in `dir` that doesn't exist yet.
pub fn free_local_path(dir: &Path, name: &str) -> PathBuf {
    (1..)
        .map(|n| dir.join(numbered_name(name, n)))
        .find(|p| std::fs::symlink_metadata(p).is_err())
        .expect("some number is free")
}

/// The folder downloads go to: the configured one, or the system's
/// Downloads folder, or home.
pub fn download_dir(configured: &str) -> PathBuf {
    if !configured.is_empty() {
        let expanded = match configured.strip_prefix("~/") {
            Some(rest) => dirs::home_dir().map_or_else(|| PathBuf::from(configured), |h| h.join(rest)),
            None => PathBuf::from(configured),
        };
        return expanded;
    }
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

//#endregion

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_names() {
        assert_eq!(numbered_name("file.txt", 1), "file (1).txt");
        assert_eq!(numbered_name("backup.tar.gz", 2), "backup (2).tar.gz");
        assert_eq!(numbered_name(".bashrc", 1), ".bashrc (1)");
        assert_eq!(numbered_name("folder", 3), "folder (3)");
        assert_eq!(numbered_name("v1.2.3.zip", 1), "v1.2.3 (1).zip");
    }

    /// Runs a transfer against a "remote" that is `argv` + a shell command.
    fn run_with(argv: &[&str], direction: Direction, remote_path: &str, local_path: &Path, is_dir: bool) -> Result<u64, String> {
        let spec = TransferSpec {
            direction,
            remote: Remote::with_argv(argv.iter().map(|s| s.to_string()).collect()),
            remote_path: remote_path.into(),
            local_path: local_path.into(),
            is_dir,
            merge: false,
        };
        let moved = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let seen = moved.clone();
        let mut reporter = Reporter::new(move |bytes, _| seen.store(bytes, Ordering::Relaxed));
        transfer(&spec, &Control::default(), &mut reporter).map(|()| moved.load(Ordering::Relaxed))
    }

    /// Uploads and downloads a file and a folder through `argv`, using
    /// `remote_dir` on the other side.
    fn round_trip(argv: &[&str], remote_dir: &str) {
        let base = std::env::temp_dir().join(format!("ionterm-rt-{}-{}", std::process::id(), argv.len()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("tree/sub dir")).unwrap();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(base.join("it's big.bin"), &data).unwrap();
        std::fs::write(base.join("tree/sub dir/a.txt"), b"hello").unwrap();
        std::fs::write(base.join("tree/b.txt"), b"bee").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("b.txt", base.join("tree/link")).unwrap();

        let remote_file = format!("{remote_dir}/it's big.bin");
        let sent = run_with(argv, Direction::Upload, &remote_file, &base.join("it's big.bin"), false).unwrap();
        assert_eq!(sent, data.len() as u64);
        let back = base.join("back.bin");
        run_with(argv, Direction::Download, &remote_file, &back, false).unwrap();
        assert_eq!(std::fs::read(&back).unwrap(), data);
        assert!(!part_path(&back).exists());

        let remote_tree = format!("{remote_dir}/tree copy");
        run_with(argv, Direction::Upload, &remote_tree, &base.join("tree"), true).unwrap();
        let tree_back = base.join("tree back");
        run_with(argv, Direction::Download, &remote_tree, &tree_back, true).unwrap();
        assert_eq!(std::fs::read(tree_back.join("sub dir/a.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(tree_back.join("b.txt")).unwrap(), b"bee");
        #[cfg(unix)]
        assert_eq!(std::fs::read_link(tree_back.join("link")).unwrap(), PathBuf::from("b.txt"));

        // A missing remote file fails cleanly and leaves nothing behind.
        let missing = base.join("missing");
        let error = run_with(argv, Direction::Download, &format!("{remote_dir}/nope"), &missing, false).unwrap_err();
        assert!(error.contains("nope") || error.contains("No such"), "{error}");
        assert!(!missing.exists() && !part_path(&missing).exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn round_trips_through_a_local_shell() {
        let dir = std::env::temp_dir().join(format!("ionterm-remote-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        round_trip(&["sh", "-c"], dir.to_str().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Against a real server: `IONTERM_TEST_HOST=myhost cargo test -- --ignored`.
    /// Works in a fresh folder under the server's /tmp and removes it.
    #[test]
    #[ignore]
    fn round_trips_over_ssh() {
        let Ok(host) = std::env::var("IONTERM_TEST_HOST") else { return };
        let argv = ["ssh", "-o", "BatchMode=yes", "-T", host.as_str()];
        let dir = format!("/tmp/ionterm-test-{}", std::process::id());
        let remote = Remote::with_argv(argv.iter().map(|s| s.to_string()).collect());
        remote.mkdir(&dir).unwrap();
        round_trip(&argv, &dir);
        remote.delete(&[dir]).unwrap();
    }

    #[test]
    fn cancelling_kills_the_transfer() {
        let control = Arc::new(Control::default());
        // A "remote" that never ends.
        let spec = TransferSpec {
            direction: Direction::Download,
            remote: Remote::with_argv(vec!["sh".into(), "-c".into()]),
            remote_path: "/dev/zero".into(),
            local_path: std::env::temp_dir().join(format!("ionterm-cancel-{}", std::process::id())),
            is_dir: false,
            merge: false,
        };
        let stopper = control.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            stopper.cancel();
        });
        let mut reporter = Reporter::new(|_, _| {});
        let result = transfer(&spec, &control, &mut reporter);
        assert_eq!(result, Err("Cancelled.".into()));
        assert!(!spec.local_path.exists() && !part_path(&spec.local_path).exists());
    }

    #[test]
    fn unpacks_under_a_new_name() {
        let base = std::env::temp_dir().join(format!("ionterm-tar-{}", std::process::id()));
        let source = base.join("src");
        std::fs::create_dir_all(source.join("inner")).unwrap();
        std::fs::write(source.join("inner/a.txt"), b"aaa").unwrap();
        std::fs::write(source.join("b.txt"), b"bb").unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        builder.append_dir_all("original", &source).unwrap();
        let data = builder.into_inner().unwrap();
        let dest = base.join("renamed (1)");
        unpack_renamed(&mut tar::Archive::new(&data[..]), &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("inner/a.txt")).unwrap(), b"aaa");
        assert_eq!(std::fs::read(dest.join("b.txt")).unwrap(), b"bb");
        assert_eq!(local_size(&source), 5);
        let _ = std::fs::remove_dir_all(&base);
    }
}
