//! The kitty graphics protocol: <https://sw.kovidgoyal.net/kitty/graphics-protocol/>.
//!
//! Images arrive in APC sequences (`ESC _ G <keys> ; <base64 payload> ESC \`),
//! are decoded to RGBA and stored here. A placement shows an image at a cell;
//! its anchor lives on the row it was placed on, so it scrolls, moves into the
//! scrollback and survives reflow with the text around it.

use std::collections::HashMap;
use std::sync::Arc;

use crate::base64;

/// Total decoded image data kept per terminal before old images are evicted.
const STORAGE_QUOTA: usize = 320 * 1024 * 1024;
/// Largest width or height accepted, in pixels.
const MAX_DIMENSION: u32 = 10_000;
/// Largest compressed or file-backed payload read in one transmission.
const MAX_PAYLOAD: usize = 400 * 1024 * 1024;

/// A decoded image.
#[derive(Clone, Debug)]
pub struct Image {
    pub id: u32,
    pub number: u32,
    pub width: u32,
    pub height: u32,
    /// Straight-alpha RGBA, `width * height * 4` bytes.
    pub rgba: Arc<Vec<u8>>,
    /// Changes whenever the data does, so renderers know to re-upload.
    pub generation: u64,
}

/// Where and how an image is shown.
#[derive(Clone, Debug)]
pub struct Placement {
    pub image_id: u32,
    pub placement_id: u32,
    /// Source rectangle in image pixels: x, y, width, height.
    pub source: [u32; 4],
    /// Size in cells.
    pub cols: u32,
    pub rows: u32,
    /// Pixel offset inside the first cell.
    pub x_offset: u32,
    pub y_offset: u32,
    pub z: i32,
    /// Shown through unicode placeholder cells instead of an anchor.
    pub virtual_placement: bool,
    /// The client gave a size in cells: the image is scaled to fill it.
    /// Otherwise it is drawn at its own pixel size.
    pub scaled: bool,
}

/// A placement's position, stored on the row it starts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Anchor {
    pub placement: u64,
    pub col: usize,
}

/// A placement to draw, as a renderer needs it.
#[derive(Clone, Debug)]
pub struct ImageDraw {
    pub image: Image,
    pub placement: Placement,
    /// Viewport row of the image's top edge; negative when it starts above.
    pub row: isize,
    pub col: usize,
}

/// A parsed `ESC _ G` command.
#[derive(Clone, Debug)]
pub struct Command {
    pub action: u8,
    pub quiet: u8,
    pub format: u32,
    pub medium: u8,
    pub width: u32,
    pub height: u32,
    pub size: usize,
    pub offset: usize,
    pub id: u32,
    pub number: u32,
    pub placement: u32,
    pub compressed: bool,
    pub more: bool,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub x_offset: u32,
    pub y_offset: u32,
    pub cols: u32,
    pub rows: u32,
    pub cursor_movement: u32,
    pub z: i32,
    pub delete: u8,
    pub unicode: bool,
}

impl Default for Command {
    fn default() -> Self {
        Self {
            action: b't',
            quiet: 0,
            format: 32,
            medium: b'd',
            width: 0,
            height: 0,
            size: 0,
            offset: 0,
            id: 0,
            number: 0,
            placement: 0,
            compressed: false,
            more: false,
            x: 0,
            y: 0,
            w: 0,
            h: 0,
            x_offset: 0,
            y_offset: 0,
            cols: 0,
            rows: 0,
            cursor_movement: 0,
            z: 0,
            delete: b'a',
            unicode: false,
        }
    }
}

impl Command {
    /// Parses the `key=value,…` control data. Unknown keys are ignored.
    pub fn parse(control: &[u8]) -> Self {
        let mut cmd = Self::default();
        for pair in control.split(|&b| b == b',') {
            let Some((&key, rest)) = pair.split_first() else { continue };
            let Some(value) = rest.strip_prefix(b"=") else { continue };
            let text = std::str::from_utf8(value).unwrap_or_default();
            let num = || text.parse::<u32>().unwrap_or(0);
            let ch = value.first().copied().unwrap_or(0);
            match key {
                b'a' => cmd.action = ch,
                b'q' => cmd.quiet = num() as u8,
                b'f' => cmd.format = num(),
                b't' => cmd.medium = ch,
                b's' => cmd.width = num(),
                b'v' => cmd.height = num(),
                b'S' => cmd.size = num() as usize,
                b'O' => cmd.offset = num() as usize,
                b'i' => cmd.id = num(),
                b'I' => cmd.number = num(),
                b'p' => cmd.placement = num(),
                b'o' => cmd.compressed = ch == b'z',
                b'm' => cmd.more = ch == b'1',
                b'x' => cmd.x = num(),
                b'y' => cmd.y = num(),
                b'w' => cmd.w = num(),
                b'h' => cmd.h = num(),
                b'X' => cmd.x_offset = num(),
                b'Y' => cmd.y_offset = num(),
                b'c' => cmd.cols = num(),
                b'r' => cmd.rows = num(),
                b'C' => cmd.cursor_movement = num(),
                b'z' => cmd.z = text.parse().unwrap_or(0),
                b'd' => cmd.delete = ch,
                b'U' => cmd.unicode = ch == b'1',
                _ => {}
            }
        }
        cmd
    }
}

/// A chunked transmission still arriving.
struct Loading {
    command: Command,
    data: Vec<u8>,
}

/// Why a command failed, as the error code the protocol replies with.
#[derive(Debug, PartialEq, Eq)]
pub struct GraphicsError {
    pub code: &'static str,
    pub message: String,
}

impl GraphicsError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Default)]
pub struct Graphics {
    images: HashMap<u32, Image>,
    placements: HashMap<u64, Placement>,
    /// (image id, placement id) → placement key, for ids the client chose.
    by_ids: HashMap<(u32, u32), u64>,
    loading: Option<Loading>,
    next_image_id: u32,
    next_key: u64,
    next_generation: u64,
    stored_bytes: usize,
    /// The tallest placement in rows, so snapshots know how far above the
    /// viewport to look for images reaching into it.
    pub max_rows: usize,
}

impl Graphics {
    pub fn image(&self, id: u32) -> Option<&Image> {
        self.images.get(&id)
    }

    pub fn placement(&self, key: u64) -> Option<&Placement> {
        self.placements.get(&key)
    }

    pub fn is_empty(&self) -> bool {
        self.placements.is_empty()
    }

    /// Virtual placements (unicode placeholders), by image id.
    pub fn virtual_placements(&self) -> impl Iterator<Item = &Placement> {
        self.placements.values().filter(|p| p.virtual_placement)
    }

    /// Adds chunk data to a transmission in progress. Returns the finished
    /// command and its data when the last chunk arrives, or `None` when this
    /// command isn't a continuation (or more chunks follow).
    pub fn continue_loading(&mut self, cmd: &Command, payload: &[u8]) -> Option<Option<(Command, Vec<u8>)>> {
        let loading = self.loading.as_mut()?;
        let decoded = base64::decode(payload).unwrap_or_default();
        if loading.data.len() + decoded.len() <= MAX_PAYLOAD {
            loading.data.extend_from_slice(&decoded);
        }
        if cmd.more {
            return Some(None);
        }
        let loading = self.loading.take().expect("checked above");
        Some(Some((loading.command, loading.data)))
    }

    pub fn start_loading(&mut self, command: Command, payload: &[u8]) {
        let data = base64::decode(payload).unwrap_or_default();
        self.loading = Some(Loading { command, data });
    }

    /// Decodes and stores an image. Returns its id.
    pub fn transmit(&mut self, cmd: &Command, payload: Vec<u8>) -> Result<u32, GraphicsError> {
        let raw = read_medium(cmd, payload)?;
        let raw = if cmd.compressed {
            miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&raw, MAX_PAYLOAD)
                .map_err(|_| GraphicsError::new("EINVAL", "zlib decompression failed"))?
        } else {
            raw
        };
        let (width, height, rgba) = decode(cmd, raw)?;
        Ok(self.store(cmd.id, cmd.number, width, height, rgba))
    }

    /// Stores decoded RGBA under `id` (0 = pick a free id). Returns the id.
    pub fn store(&mut self, id: u32, number: u32, width: u32, height: u32, rgba: Vec<u8>) -> u32 {
        let id = if id != 0 {
            id
        } else {
            // Ids the terminal picks count down from the top so they never
            // collide with small ids clients choose themselves.
            self.next_image_id = self.next_image_id.wrapping_sub(1).max(1 << 24);
            while self.images.contains_key(&self.next_image_id) {
                self.next_image_id -= 1;
            }
            self.next_image_id
        };
        if let Some(old) = self.images.remove(&id) {
            self.stored_bytes -= old.rgba.len();
        }
        if number != 0 {
            // Numbers are not unique: the newest image with a number wins.
            for image in self.images.values_mut().filter(|i| i.number == number) {
                image.number = 0;
            }
        }
        self.next_generation += 1;
        self.stored_bytes += rgba.len();
        self.images.insert(
            id,
            Image {
                id,
                number,
                width,
                height,
                rgba: Arc::new(rgba),
                generation: self.next_generation,
            },
        );
        self.enforce_quota(id);
        id
    }

    /// Looks up the image a command refers to by id or number.
    pub fn resolve(&self, cmd: &Command) -> Option<u32> {
        if cmd.id != 0 {
            return self.images.contains_key(&cmd.id).then_some(cmd.id);
        }
        if cmd.number != 0 {
            return self
                .images
                .values()
                .filter(|i| i.number == cmd.number)
                .max_by_key(|i| i.generation)
                .map(|i| i.id);
        }
        None
    }

    /// Creates (or replaces) a placement. Returns its key.
    pub fn place(&mut self, image_id: u32, cmd: &Command, cell: (u32, u32)) -> u64 {
        let image = &self.images[&image_id];
        let sx = cmd.x.min(image.width);
        let sy = cmd.y.min(image.height);
        let sw = if cmd.w == 0 { image.width - sx } else { cmd.w.min(image.width - sx) };
        let sh = if cmd.h == 0 { image.height - sy } else { cmd.h.min(image.height - sy) };
        let (cw, ch) = (cell.0.max(1), cell.1.max(1));
        // With only one of columns/rows given, the other keeps the aspect ratio.
        let (cols, rows) = match (cmd.cols, cmd.rows) {
            (0, 0) => ((sw + cmd.x_offset).div_ceil(cw), (sh + cmd.y_offset).div_ceil(ch)),
            (c, 0) => (c, ((c * cw) as f32 * sh as f32 / sw.max(1) as f32 / ch as f32).ceil() as u32),
            (0, r) => (((r * ch) as f32 * sw as f32 / sh.max(1) as f32 / cw as f32).ceil() as u32, r),
            (c, r) => (c, r),
        };
        let placement = Placement {
            image_id,
            placement_id: cmd.placement,
            source: [sx, sy, sw, sh],
            cols: cols.max(1),
            rows: rows.max(1),
            x_offset: cmd.x_offset,
            y_offset: cmd.y_offset,
            z: cmd.z,
            virtual_placement: cmd.unicode,
            scaled: cmd.cols != 0 || cmd.rows != 0,
        };
        self.max_rows = self.max_rows.max(placement.rows as usize);
        if cmd.placement != 0
            && let Some(old) = self.by_ids.remove(&(image_id, cmd.placement))
        {
            self.placements.remove(&old);
        }
        self.next_key += 1;
        let key = self.next_key;
        if cmd.placement != 0 {
            self.by_ids.insert((image_id, cmd.placement), key);
        }
        self.placements.insert(key, placement);
        key
    }

    /// Removes placements matching a predicate; with `free`, images left
    /// without placements are dropped too.
    pub fn remove_placements(&mut self, free: bool, mut matches: impl FnMut(u64, &Placement) -> bool) {
        let removed: Vec<u64> = self
            .placements
            .iter()
            .filter(|(key, p)| matches(**key, p))
            .map(|(key, _)| *key)
            .collect();
        let mut touched = Vec::new();
        for key in removed {
            if let Some(p) = self.placements.remove(&key) {
                self.by_ids.remove(&(p.image_id, p.placement_id));
                touched.push(p.image_id);
            }
        }
        if free {
            for id in touched {
                self.free_if_unused(id);
            }
        }
    }

    /// Drops an image and every placement of it.
    pub fn remove_image(&mut self, id: u32) {
        self.remove_placements(false, |_, p| p.image_id == id);
        if let Some(image) = self.images.remove(&id) {
            self.stored_bytes -= image.rgba.len();
        }
    }

    fn free_if_unused(&mut self, id: u32) {
        if !self.placements.values().any(|p| p.image_id == id)
            && let Some(image) = self.images.remove(&id)
        {
            self.stored_bytes -= image.rgba.len();
        }
    }

    /// Evicts the oldest images (unplaced first) until under the quota,
    /// never the one just stored.
    fn enforce_quota(&mut self, keep: u32) {
        while self.stored_bytes > STORAGE_QUOTA {
            let placed: Vec<u32> = self.placements.values().map(|p| p.image_id).collect();
            let victim = self
                .images
                .values()
                .filter(|i| i.id != keep)
                .min_by_key(|i| (placed.contains(&i.id), i.generation))
                .map(|i| i.id);
            match victim {
                Some(id) => self.remove_image(id),
                None => break,
            }
        }
    }
}

/// Reads the image bytes from wherever the command says they are.
fn read_medium(cmd: &Command, payload: Vec<u8>) -> Result<Vec<u8>, GraphicsError> {
    match cmd.medium {
        b'd' => Ok(payload),
        b'f' | b't' | b's' => {
            let name = String::from_utf8(payload).map_err(|_| GraphicsError::new("EINVAL", "path is not UTF-8"))?;
            let path = match cmd.medium {
                b's' => shared_memory_path(&name)?,
                _ => std::path::PathBuf::from(&name),
            };
            if cmd.medium == b't' && !is_temp_file(&path) {
                return Err(GraphicsError::new("EINVAL", "temporary file is not in a temporary directory"));
            }
            let result = read_file(&path, cmd.offset, cmd.size);
            // Temporary files and shared memory are consumed by reading them.
            if cmd.medium != b'f' {
                let _ = std::fs::remove_file(&path);
            }
            result
        }
        other => Err(GraphicsError::new(
            "EINVAL",
            format!("unknown transmission medium {:?}", other as char),
        )),
    }
}

fn read_file(path: &std::path::Path, offset: usize, size: usize) -> Result<Vec<u8>, GraphicsError> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).map_err(|e| GraphicsError::new("EBADF", e.to_string()))?;
    if offset > 0 {
        file.seek(SeekFrom::Start(offset as u64))
            .map_err(|e| GraphicsError::new("EBADF", e.to_string()))?;
    }
    let limit = if size == 0 { MAX_PAYLOAD } else { size.min(MAX_PAYLOAD) };
    let mut data = Vec::new();
    file.take(limit as u64)
        .read_to_end(&mut data)
        .map_err(|e| GraphicsError::new("EBADF", e.to_string()))?;
    Ok(data)
}

/// Temporary files must live in a temp directory and say what they are for,
/// so a client can't trick ionterm into deleting arbitrary files.
fn is_temp_file(path: &std::path::Path) -> bool {
    let in_temp = [std::env::temp_dir(), "/tmp".into(), "/dev/shm".into()]
        .iter()
        .any(|dir| path.starts_with(dir));
    in_temp && path.to_string_lossy().contains("tty-graphics-protocol")
}

#[cfg(target_os = "linux")]
fn shared_memory_path(name: &str) -> Result<std::path::PathBuf, GraphicsError> {
    let name = name.trim_start_matches('/');
    if name.is_empty() || name.contains('/') {
        return Err(GraphicsError::new("EINVAL", "invalid shared memory name"));
    }
    Ok(std::path::Path::new("/dev/shm").join(name))
}

#[cfg(not(target_os = "linux"))]
fn shared_memory_path(_name: &str) -> Result<std::path::PathBuf, GraphicsError> {
    Err(GraphicsError::new("EINVAL", "shared memory transmission is not supported here"))
}

/// Turns raw bytes into RGBA according to the command's format.
fn decode(cmd: &Command, raw: Vec<u8>) -> Result<(u32, u32, Vec<u8>), GraphicsError> {
    let check = |w: u32, h: u32| {
        if w == 0 || h == 0 || w > MAX_DIMENSION || h > MAX_DIMENSION {
            Err(GraphicsError::new("EINVAL", format!("bad image size {w}x{h}")))
        } else {
            Ok(())
        }
    };
    match cmd.format {
        24 | 32 => {
            let (w, h) = (cmd.width, cmd.height);
            check(w, h)?;
            let bpp = (cmd.format / 8) as usize;
            let expected = w as usize * h as usize * bpp;
            if raw.len() < expected {
                return Err(GraphicsError::new(
                    "ENODATA",
                    format!("expected {expected} bytes of pixel data, got {}", raw.len()),
                ));
            }
            let rgba = if bpp == 4 {
                raw[..expected].to_vec()
            } else {
                raw[..expected].chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect()
            };
            Ok((w, h, rgba))
        }
        100 => decode_png(&raw).and_then(|(w, h, rgba)| check(w, h).map(|()| (w, h, rgba))),
        other => Err(GraphicsError::new("EINVAL", format!("unknown image format {other}"))),
    }
}

fn decode_png(data: &[u8]) -> Result<(u32, u32, Vec<u8>), GraphicsError> {
    let fail = |e: png::DecodingError| GraphicsError::new("EBADPNG", e.to_string());
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(fail)?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| GraphicsError::new("EBADPNG", "image too large"))?;
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf).map_err(fail)?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf.chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf.chunks(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err(GraphicsError::new("EBADPNG", "unexpanded palette image")),
    };
    Ok((info.width, info.height, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_control_data() {
        let cmd = Command::parse(b"a=T,f=24,s=2,v=1,i=7,m=1,z=-3,q=2");
        assert_eq!(cmd.action, b'T');
        assert_eq!((cmd.format, cmd.width, cmd.height, cmd.id), (24, 2, 1, 7));
        assert!(cmd.more);
        assert_eq!(cmd.z, -3);
        assert_eq!(cmd.quiet, 2);
    }

    #[test]
    fn stores_rgb_and_places_by_cells() {
        let mut g = Graphics::default();
        let cmd = Command::parse(b"f=24,s=20,v=40,i=1");
        let id = g.transmit(&cmd, vec![255; 20 * 40 * 3]).unwrap();
        assert_eq!(id, 1);
        assert_eq!(g.image(1).unwrap().rgba.len(), 20 * 40 * 4);
        let key = g.place(1, &Command::parse(b"a=p,i=1"), (10, 20));
        let p = g.placement(key).unwrap();
        assert_eq!((p.cols, p.rows), (2, 2));
        g.remove_placements(true, |_, p| p.image_id == 1);
        assert!(g.image(1).is_none());
    }

    #[test]
    fn rejects_short_data() {
        let mut g = Graphics::default();
        let err = g.transmit(&Command::parse(b"f=32,s=4,v=4"), vec![0; 10]).unwrap_err();
        assert_eq!(err.code, "ENODATA");
    }
}
