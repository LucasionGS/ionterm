//! ZMODEM's wire format: headers, data subpackets, escaping and CRCs.
//!
//! A header is `**` ZDLE then `B` (hex), `A` (binary, CRC-16) or `C`
//! (binary, CRC-32), followed by a frame type and four bytes (a file
//! position, little-endian, or flags). After some headers come data
//! subpackets: escaped bytes ended by ZDLE plus one of four letters that say
//! what comes next, then the CRC of the data and that letter.

/// ZDLE, also CAN: escapes the next byte, and five in a row abort.
pub const ZDLE: u8 = 0x18;
const ZPAD: u8 = b'*';
const ZBIN: u8 = b'A';
const ZHEX: u8 = b'B';
const ZBIN32: u8 = b'C';
const XON: u8 = 0x11;

/// How a data subpacket ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// End of the frame: a header comes next.
    E,
    /// More data follows, no reply wanted.
    G,
    /// More data follows, reply with ZACK.
    Q,
    /// End of the frame, reply with ZACK.
    W,
}

impl End {
    fn byte(self) -> u8 {
        match self {
            Self::E => b'h',
            Self::G => b'i',
            Self::Q => b'j',
            Self::W => b'k',
        }
    }

    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            b'h' => Some(Self::E),
            b'i' => Some(Self::G),
            b'j' => Some(Self::Q),
            b'k' => Some(Self::W),
            _ => None,
        }
    }

    /// Whether more data subpackets follow this one.
    pub fn more(self) -> bool {
        matches!(self, Self::G | Self::Q)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    RqInit,
    RInit,
    SInit,
    Ack,
    File,
    Skip,
    Nak,
    Abort,
    Fin,
    RPos,
    Data,
    Eof,
    FErr,
    Crc,
    Challenge,
    Compl,
    Can,
    FreeCnt,
    Command,
    Stderr,
}

impl Kind {
    const ALL: [Kind; 20] = [
        Kind::RqInit,
        Kind::RInit,
        Kind::SInit,
        Kind::Ack,
        Kind::File,
        Kind::Skip,
        Kind::Nak,
        Kind::Abort,
        Kind::Fin,
        Kind::RPos,
        Kind::Data,
        Kind::Eof,
        Kind::FErr,
        Kind::Crc,
        Kind::Challenge,
        Kind::Compl,
        Kind::Can,
        Kind::FreeCnt,
        Kind::Command,
        Kind::Stderr,
    ];

    fn from_byte(byte: u8) -> Option<Self> {
        Self::ALL.get(usize::from(byte)).copied()
    }

    fn byte(self) -> u8 {
        Self::ALL.iter().position(|&k| k == self).unwrap_or(0) as u8
    }

    /// Whether data subpackets follow a header of this kind.
    fn has_data(self) -> bool {
        matches!(self, Kind::SInit | Kind::File | Kind::Data | Kind::Command)
    }
}

// ZRINIT flags (ZF0).
pub const CANFDX: u8 = 0x01;
pub const CANOVIO: u8 = 0x02;
pub const CANFC32: u8 = 0x20;
pub const ESCCTL: u8 = 0x40;
// ZFILE conversion option (ZF0): binary.
pub const ZCBIN: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    /// ZP0..ZP3: a position (little-endian), or ZF3..ZF0 flags.
    pub data: [u8; 4],
}

impl Header {
    pub fn new(kind: Kind) -> Self {
        Self { kind, data: [0; 4] }
    }

    pub fn with_position(kind: Kind, position: u32) -> Self {
        Self {
            kind,
            data: position.to_le_bytes(),
        }
    }

    pub fn with_flags(kind: Kind, f0: u8, f1: u8) -> Self {
        Self {
            kind,
            data: [0, 0, f1, f0],
        }
    }

    pub fn position(&self) -> u32 {
        u32::from_le_bytes(self.data)
    }

    pub fn f0(&self) -> u8 {
        self.data[3]
    }

    fn bytes(&self) -> [u8; 5] {
        let [a, b, c, d] = self.data;
        [self.kind.byte(), a, b, c, d]
    }
}

//#region CRCs

pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// CRC-32 as ZMODEM and zip use it (reflected, inverted).
pub fn crc32(data: &[u8]) -> u32 {
    crc32_update(0xFFFF_FFFF, data) ^ 0xFFFF_FFFF
}

/// Feeds bytes into a running CRC-32 (start at `0xFFFF_FFFF`, finish by
/// inverting), for checksumming a whole file in pieces.
pub fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    crc
}

//#endregion

//#region Encoding

/// Escapes bytes that the line might eat: ZDLE itself, XON/XOFF and DLE
/// (with or without the high bit), `@` followed by CR (a telnet escape),
/// and every control character when the receiver asked for that.
#[derive(Default)]
pub struct Escaper {
    pub escape_controls: bool,
    last: u8,
}

impl Escaper {
    pub fn push(&mut self, out: &mut Vec<u8>, byte: u8) {
        let escape = match byte {
            ZDLE | 0x10 | 0x11 | 0x13 | 0x90 | 0x91 | 0x93 => true,
            0x0d | 0x8d => self.last & 0x7f == b'@' || self.escape_controls,
            _ => self.escape_controls && byte & 0x60 == 0,
        };
        if escape {
            out.push(ZDLE);
            out.push(byte ^ 0x40);
        } else {
            out.push(byte);
        }
        self.last = byte;
    }

    pub fn extend(&mut self, out: &mut Vec<u8>, bytes: &[u8]) {
        for &byte in bytes {
            self.push(out, byte);
        }
    }
}

fn hex(out: &mut Vec<u8>, byte: u8) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    out.push(DIGITS[usize::from(byte >> 4)]);
    out.push(DIGITS[usize::from(byte & 15)]);
}

/// A hex header: plain ASCII, what receivers send.
pub fn hex_header(header: &Header) -> Vec<u8> {
    let mut out = vec![ZPAD, ZPAD, ZDLE, ZHEX];
    let bytes = header.bytes();
    for byte in bytes {
        hex(&mut out, byte);
    }
    for byte in crc16(&bytes).to_be_bytes() {
        hex(&mut out, byte);
    }
    // CR, LF with the high bit set, then XON to undo a stray XOFF.
    out.extend_from_slice(&[b'\r', 0x8a]);
    if !matches!(header.kind, Kind::Fin | Kind::Ack) {
        out.push(XON);
    }
    out
}

/// A binary header, what senders send.
pub fn binary_header(header: &Header, use_crc32: bool, escaper: &mut Escaper) -> Vec<u8> {
    let mut out = vec![ZPAD, ZDLE, if use_crc32 { ZBIN32 } else { ZBIN }];
    let bytes = header.bytes();
    escaper.extend(&mut out, &bytes);
    if use_crc32 {
        escaper.extend(&mut out, &crc32(&bytes).to_le_bytes());
    } else {
        escaper.extend(&mut out, &crc16(&bytes).to_be_bytes());
    }
    out
}

/// A data subpacket.
pub fn subpacket(data: &[u8], end: End, use_crc32: bool, escaper: &mut Escaper) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 8 + 8);
    escaper.extend(&mut out, data);
    out.push(ZDLE);
    out.push(end.byte());
    if use_crc32 {
        let crc = crc32_update(crc32_update(0xFFFF_FFFF, data), &[end.byte()]) ^ 0xFFFF_FFFF;
        escaper.extend(&mut out, &crc.to_le_bytes());
    } else {
        let mut crc_data = data.to_vec();
        crc_data.push(end.byte());
        escaper.extend(&mut out, &crc16(&crc_data).to_be_bytes());
    }
    out
}

//#endregion

//#region Decoding

/// What the decoder found in the incoming bytes.
#[derive(Debug, PartialEq, Eq)]
pub enum Token {
    Header(Header),
    /// A data subpacket; `ok` is false when its CRC didn't match.
    Data { data: Vec<u8>, end: End, ok: bool },
    /// The other side sent a run of CANs: it gave up.
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Header,
    Data { crc32: bool },
}

/// Turns bytes into tokens. Bytes can arrive in any pieces; whatever isn't
/// a whole token yet waits for the next `push`.
pub struct Decoder {
    buffer: Vec<u8>,
    /// Where decoding continues in `buffer`.
    pos: usize,
    mode: Mode,
    /// Bytes in a row that were CAN, across pushes.
    cans: usize,
    /// Five CANs arrived (the other side gave up): where their run ends in
    /// the buffer. What follows is the terminal's again.
    cancelled_at: Option<usize>,
}

/// Escaped bytes, decoded, or why decoding stopped.
enum Escaped {
    Byte(u8),
    End(End),
    /// Not enough bytes yet.
    Short,
    Cancelled,
    Bad,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            pos: 0,
            mode: Mode::Header,
            cans: 0,
            cancelled_at: None,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        // Drop what was decoded, once per push rather than once per token.
        if self.pos > 0 {
            self.buffer.drain(..self.pos);
            self.cancelled_at = self.cancelled_at.map(|at| at.saturating_sub(self.pos));
            self.pos = 0;
        }
        // Two CANs in a row never occur otherwise: CAN is ZDLE, and a ZDLE
        // in the data is sent escaped.
        for &byte in bytes {
            self.buffer.push(byte);
            self.cans = if byte == ZDLE { self.cans + 1 } else { 0 };
            if self.cans >= 5 {
                self.cancelled_at = Some(self.buffer.len());
            }
        }
    }

    /// Stop expecting data subpackets: look for the next header.
    pub fn expect_header(&mut self) {
        self.mode = Mode::Header;
    }

    /// Bytes received but not yet decoded (after the session ends, they
    /// belong to the terminal again).
    pub fn take_rest(&mut self) -> Vec<u8> {
        let rest = self.buffer.split_off(self.pos.min(self.buffer.len()));
        self.buffer.clear();
        self.pos = 0;
        rest
    }

    fn rest(&self) -> &[u8] {
        &self.buffer[self.pos..]
    }

    /// The next token, if the buffer holds a whole one.
    pub fn next(&mut self) -> Option<Token> {
        if let Some(at) = self.cancelled_at.take() {
            let at = at.max(self.pos);
            // The backspaces that follow erase the CANs on a terminal.
            let backspaces = self.buffer[at..].iter().take_while(|&&b| b == 0x08).count();
            self.pos = at + backspaces;
            self.mode = Mode::Header;
            return Some(Token::Cancelled);
        }
        match self.mode {
            Mode::Header => self.next_header(),
            Mode::Data { crc32 } => self.next_data(crc32),
        }
    }

    fn next_header(&mut self) -> Option<Token> {
        loop {
            let buffer = self.rest();
            // Find `*` ZDLE <format>, possibly with more `*`s before.
            let Some(start) = buffer.iter().position(|&b| b == ZPAD) else {
                // Line noise between frames (the CR LF XON after a hex header).
                self.pos = self.buffer.len();
                return None;
            };
            let mut i = start;
            while buffer.get(i) == Some(&ZPAD) {
                i += 1;
            }
            let (Some(&zdle), Some(&format)) = (buffer.get(i), buffer.get(i + 1)) else {
                self.pos += start;
                return None;
            };
            if zdle != ZDLE || !matches!(format, ZHEX | ZBIN | ZBIN32) {
                self.pos += i.max(start + 1);
                continue;
            }
            let body = i + 2;
            let parsed = match format {
                ZHEX => parse_hex_header(&buffer[body..]),
                _ => parse_binary_header(&buffer[body..], format == ZBIN32),
            };
            match parsed {
                Parsed::Short => {
                    self.pos += start;
                    return None;
                }
                Parsed::Bad(used) => {
                    let len = buffer.len();
                    self.pos += (body + used.max(1)).min(len);
                }
                Parsed::Header(header, used) => {
                    self.pos += body + used;
                    if header.kind.has_data() {
                        self.mode = Mode::Data { crc32: format == ZBIN32 };
                    }
                    return Some(Token::Header(header));
                }
            }
        }
    }

    fn next_data(&mut self, crc32: bool) -> Option<Token> {
        let buffer = &self.buffer[self.pos..];
        let mut data = Vec::new();
        let mut i = 0;
        let end = loop {
            match unescape(buffer, &mut i) {
                Escaped::Byte(byte) => data.push(byte),
                Escaped::End(end) => break end,
                Escaped::Short => return None,
                Escaped::Cancelled => {
                    // The run of CANs `push` saw; it ends the session.
                    self.pos += i;
                    self.mode = Mode::Header;
                    return Some(Token::Cancelled);
                }
                Escaped::Bad => {
                    // Lost sync: report a bad packet and resync on a header.
                    self.pos += i;
                    self.mode = Mode::Header;
                    return Some(Token::Data {
                        data: Vec::new(),
                        end: End::E,
                        ok: false,
                    });
                }
            }
        };
        let mut crc = [0u8; 4];
        let crc_len = if crc32 { 4 } else { 2 };
        for slot in crc.iter_mut().take(crc_len) {
            match unescape(buffer, &mut i) {
                Escaped::Byte(byte) => *slot = byte,
                Escaped::Short => return None,
                _ => {
                    self.pos += i;
                    self.mode = Mode::Header;
                    return Some(Token::Data { data, end, ok: false });
                }
            }
        }
        self.pos += i;
        let ok = if crc32 {
            let expected = crc32_update(crc32_update(0xFFFF_FFFF, &data), &[end.byte()]) ^ 0xFFFF_FFFF;
            expected.to_le_bytes() == crc[..4]
        } else {
            let mut crc_data = data.clone();
            crc_data.push(end.byte());
            crc16(&crc_data).to_be_bytes() == crc[..2]
        };
        if !ok || !end.more() {
            self.mode = Mode::Header;
        }
        Some(Token::Data { data, end, ok })
    }
}

/// Reads one escaped byte at `*i`, advancing past it.
fn unescape(buffer: &[u8], i: &mut usize) -> Escaped {
    // Flow control characters on the line are noise, even after a ZDLE.
    let flow = |b: u8| matches!(b, 0x11 | 0x13 | 0x91 | 0x93);
    let mut j = *i;
    while buffer.get(j).is_some_and(|&b| flow(b)) {
        j += 1;
    }
    let Some(&byte) = buffer.get(j) else { return Escaped::Short };
    if byte != ZDLE {
        *i = j + 1;
        return Escaped::Byte(byte);
    }
    j += 1;
    while buffer.get(j).is_some_and(|&b| flow(b)) {
        j += 1;
    }
    let Some(&next) = buffer.get(j) else { return Escaped::Short };
    *i = j + 1;
    if let Some(end) = End::from_byte(next) {
        return Escaped::End(end);
    }
    match next {
        ZDLE => Escaped::Cancelled,
        b'l' => Escaped::Byte(0x7f),
        b'm' => Escaped::Byte(0xff),
        _ if next & 0x60 == 0x40 => Escaped::Byte(next ^ 0x40),
        _ => Escaped::Bad,
    }
}

enum Parsed {
    Header(Header, usize),
    /// Not a header; skip this many bytes.
    Bad(usize),
    Short,
}

fn parse_hex_header(bytes: &[u8]) -> Parsed {
    // Type, four data bytes and a CRC-16: 7 bytes as 14 hex digits.
    if bytes.len() < 14 {
        if bytes.iter().all(u8::is_ascii_hexdigit) {
            return Parsed::Short;
        }
        return Parsed::Bad(1);
    }
    let mut raw = [0u8; 7];
    for (k, slot) in raw.iter_mut().enumerate() {
        let pair = &bytes[2 * k..2 * k + 2];
        let Some(byte) = std::str::from_utf8(pair).ok().and_then(|s| u8::from_str_radix(s, 16).ok()) else {
            return Parsed::Bad(1);
        };
        *slot = byte;
    }
    if crc16(&raw[..5]).to_be_bytes() != raw[5..7] {
        return Parsed::Bad(1);
    }
    let Some(kind) = Kind::from_byte(raw[0]) else { return Parsed::Bad(14) };
    Parsed::Header(
        Header {
            kind,
            data: [raw[1], raw[2], raw[3], raw[4]],
        },
        14,
    )
}

fn parse_binary_header(bytes: &[u8], wide: bool) -> Parsed {
    let len = if wide { 9 } else { 7 };
    let mut raw = [0u8; 9];
    let mut i = 0;
    for slot in raw.iter_mut().take(len) {
        match unescape(bytes, &mut i) {
            Escaped::Byte(byte) => *slot = byte,
            Escaped::Short => return Parsed::Short,
            _ => return Parsed::Bad(i),
        }
    }
    let ok = if wide {
        crc32(&raw[..5]).to_le_bytes() == raw[5..9]
    } else {
        crc16(&raw[..5]).to_be_bytes() == raw[5..7]
    };
    let kind = Kind::from_byte(raw[0]);
    match (ok, kind) {
        (true, Some(kind)) => Parsed::Header(
            Header {
                kind,
                data: [raw[1], raw[2], raw[3], raw[4]],
            },
            i,
        ),
        _ => Parsed::Bad(i),
    }
}

/// Whether `bytes` starts with a complete, valid hex header, and if so,
/// which one and how long it is (including the line end that follows).
/// `None` when more bytes are needed; `Some(None)` when it isn't one.
pub fn probe_hex_header(bytes: &[u8]) -> Option<Option<(Header, usize)>> {
    let prefix = [ZPAD, ZPAD, ZDLE, ZHEX];
    let have = bytes.len().min(prefix.len());
    if bytes[..have] != prefix[..have] {
        return Some(None);
    }
    if bytes.len() < prefix.len() {
        return None;
    }
    match parse_hex_header(&bytes[4..]) {
        Parsed::Short => None,
        Parsed::Bad(_) => Some(None),
        Parsed::Header(header, used) => Some(Some((header, 4 + used))),
    }
}

//#endregion

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crcs_match_the_references() {
        // CRC-16/XMODEM and CRC-32 of "123456789".
        assert_eq!(crc16(b"123456789"), 0x31C3);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn hex_headers_match_lrzsz() {
        // What `sz` sends first, and `rz`'s ZRINIT with CANFDX|CANOVIO|CANFC32.
        assert_eq!(&hex_header(&Header::new(Kind::RqInit))[..18], b"**\x18B00000000000000");
        let rinit = hex_header(&Header::with_flags(Kind::RInit, CANFDX | CANOVIO | CANFC32, 0));
        let probed = probe_hex_header(&rinit).unwrap().unwrap();
        assert_eq!(probed.0.kind, Kind::RInit);
        assert_eq!(probed.0.f0(), 0x23);
    }

    #[test]
    fn decodes_what_it_encodes_in_any_pieces() {
        let mut escaper = Escaper::default();
        let mut wire = binary_header(&Header::with_position(Kind::Data, 70_000), true, &mut escaper);
        let payload: Vec<u8> = (0..=255u8).cycle().take(3000).collect();
        wire.extend(subpacket(&payload[..1000], End::G, true, &mut escaper));
        wire.extend(subpacket(&payload[1000..], End::E, true, &mut escaper));
        wire.extend(hex_header(&Header::with_position(Kind::Eof, 73_000)));
        for piece in [1, 7, 64, wire.len()] {
            let mut decoder = Decoder::new();
            let mut tokens = Vec::new();
            for chunk in wire.chunks(piece) {
                decoder.push(chunk);
                while let Some(token) = decoder.next() {
                    tokens.push(token);
                }
            }
            assert_eq!(tokens.len(), 4, "pieces of {piece}");
            assert_eq!(tokens[0], Token::Header(Header::with_position(Kind::Data, 70_000)));
            let Token::Data { data, end: End::G, ok: true } = &tokens[1] else { panic!("{:?}", tokens[1]) };
            assert_eq!(data, &payload[..1000]);
            let Token::Data { data, end: End::E, ok: true } = &tokens[2] else { panic!("{:?}", tokens[2]) };
            assert_eq!(data, &payload[1000..]);
            assert_eq!(tokens[3], Token::Header(Header::with_position(Kind::Eof, 73_000)));
        }
    }

    #[test]
    fn notices_damage_and_cancels() {
        let mut escaper = Escaper::default();
        let mut wire = binary_header(&Header::with_position(Kind::Data, 0), false, &mut escaper);
        let mut packet = subpacket(b"hello", End::E, false, &mut escaper);
        packet[1] ^= 1;
        wire.extend(packet);
        let mut decoder = Decoder::new();
        decoder.push(&wire);
        assert!(matches!(decoder.next(), Some(Token::Header(_))));
        assert!(matches!(decoder.next(), Some(Token::Data { ok: false, .. })));
        decoder.push(&[ZDLE; 3]);
        assert_eq!(decoder.next(), None);
        decoder.push(&[ZDLE; 2]);
        assert_eq!(decoder.next(), Some(Token::Cancelled));
    }

    #[test]
    fn probes_only_real_headers() {
        assert_eq!(probe_hex_header(b"**\x18B0"), None);
        assert_eq!(probe_hex_header(b"**\x18Bzz"), Some(None));
        assert_eq!(probe_hex_header(b"**\x18B00000000000001"), Some(None));
        assert_eq!(probe_hex_header(b"*x"), Some(None));
    }
}
