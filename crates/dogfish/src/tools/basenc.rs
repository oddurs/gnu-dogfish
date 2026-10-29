//! base64 / basenc

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};

use anyhow::{Result, anyhow};
use data_encoding::{BASE32, BASE32HEX, HEXUPPER};
use dogfish_core::{DogfishContext, gnu_io_error, gnu_quote, gnu_quote_file, stdout};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy, lossy_all, print_help, print_version, usage_error,
};

/// The encodings `basenc` offers (`base64` always uses [`Encoding::Base64`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Base64,
    Base64Url,
    Base32,
    Base32Hex,
    Base16,
    Base2Msbf,
    Base2Lsbf,
    Z85,
}

const STD_B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL_B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
const B32_HEX: &[u8; 32] = b"0123456789ABCDEFGHIJKLMNOPQRSTUV";
const Z85_ALPHABET: &[u8; 85] =
    b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ.-:+=^!/*?&<>()[]{}@%$#";

/// Why decoding (or z85 encoding) stopped; GNU still writes what it produced.
enum CodecError {
    Invalid,
    Z85Length,
}

impl Encoding {
    /// Whether `c` belongs to the alphabet (padding included).
    fn in_alphabet(self, c: u8) -> bool {
        match self {
            Self::Base64 => STD_B64.contains(&c) || c == b'=',
            Self::Base64Url => URL_B64.contains(&c) || c == b'=',
            Self::Base32 => B32.contains(&c) || c == b'=',
            Self::Base32Hex => B32_HEX.contains(&c) || c == b'=',
            Self::Base16 => c.is_ascii_digit() || (b'A'..=b'F').contains(&c),
            Self::Base2Msbf | Self::Base2Lsbf => c == b'0' || c == b'1',
            Self::Z85 => Z85_ALPHABET.contains(&c),
        }
    }

    fn encode(self, data: &[u8]) -> std::result::Result<Vec<u8>, CodecError> {
        use base64::Engine as _;
        Ok(match self {
            Self::Base64 => base64::engine::general_purpose::STANDARD
                .encode(data)
                .into_bytes(),
            Self::Base64Url => base64::engine::general_purpose::URL_SAFE
                .encode(data)
                .into_bytes(),
            Self::Base32 => BASE32.encode(data).into_bytes(),
            Self::Base32Hex => BASE32HEX.encode(data).into_bytes(),
            Self::Base16 => HEXUPPER.encode(data).into_bytes(),
            Self::Base2Msbf => data
                .iter()
                .flat_map(|b| (0..8).rev().map(move |i| b'0' + ((b >> i) & 1)))
                .collect(),
            Self::Base2Lsbf => data
                .iter()
                .flat_map(|b| (0..8).map(move |i| b'0' + ((b >> i) & 1)))
                .collect(),
            Self::Z85 => {
                let (groups, rest) = data.as_chunks::<4>();
                if !rest.is_empty() {
                    return Err(CodecError::Z85Length);
                }
                let mut out = Vec::with_capacity(groups.len() * 5);
                for group in groups {
                    let mut v = u32::from_be_bytes(*group);
                    let mut chars = [0u8; 5];
                    for slot in chars.iter_mut().rev() {
                        *slot = Z85_ALPHABET[(v % 85) as usize];
                        v /= 85;
                    }
                    out.extend_from_slice(&chars);
                }
                out
            }
        })
    }

    /// Characters per encoded group: decoding works one whole group at a time.
    fn group_len(self) -> usize {
        match self {
            Self::Base64 | Self::Base64Url => 4,
            Self::Base32 | Self::Base32Hex | Self::Base2Msbf | Self::Base2Lsbf => 8,
            Self::Base16 => 2,
            Self::Z85 => 5,
        }
    }

    /// Decodes `clean` (newlines, or with `--ignore-garbage` everything
    /// outside the alphabet, already removed) group by group, as gnulib does,
    /// appending to `out`. A trailing short group is decoded as far as it
    /// goes and reported invalid; false means decoding stopped at an invalid
    /// group, after emitting the bytes known good before it.
    fn decode_groups(self, clean: &[u8], table: &DecodeTable, out: &mut Vec<u8>) -> bool {
        match self {
            Self::Base64 => decode_base64(clean, STD_B64, table, out),
            Self::Base64Url => decode_base64(clean, URL_B64, table, out),
            Self::Base32 => clean
                .chunks(8)
                .all(|group| decode_base32_group(group, B32, out)),
            Self::Base32Hex => clean
                .chunks(8)
                .all(|group| decode_base32_group(group, B32_HEX, out)),
            Self::Base16 => clean.chunks(2).all(|pair| {
                let nibble = |c: Option<&u8>| {
                    c.filter(|c| self.in_alphabet(**c))
                        .and_then(|&c| (c as char).to_digit(16))
                };
                match (nibble(pair.first()), nibble(pair.get(1))) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi * 16 + lo) as u8);
                        true
                    }
                    _ => false,
                }
            }),
            Self::Base2Msbf | Self::Base2Lsbf => clean.chunks(8).all(|group| {
                if group.len() < 8 || !group.iter().all(|&c| self.in_alphabet(c)) {
                    return false;
                }
                let bits = group.iter().map(|&c| c - b'0');
                out.push(if self == Self::Base2Msbf {
                    bits.fold(0, |acc, b| (acc << 1) | b)
                } else {
                    bits.rev().fold(0, |acc, b| (acc << 1) | b)
                });
                true
            }),
            Self::Z85 => clean.chunks(5).all(|group| {
                let value = group.iter().try_fold(0u64, |acc, c| {
                    let digit = Z85_ALPHABET.iter().position(|a| a == c)?;
                    Some(acc * 85 + digit as u64)
                });
                match value.filter(|_| group.len() == 5).map(u32::try_from) {
                    Some(Ok(v)) => {
                        out.extend_from_slice(&v.to_be_bytes());
                        true
                    }
                    _ => false,
                }
            }),
        }
    }
}

/// A 256-entry map from byte to 6-bit base64 value, [`INVALID`] for anything
/// else (padding included, which the per-group slow path handles).
type DecodeTable = [u8; 256];

const INVALID: u8 = 0x80;

fn decode_table(alphabet: &[u8]) -> DecodeTable {
    let mut table = [INVALID; 256];
    for (value, &c) in alphabet.iter().enumerate() {
        table[c as usize] = value as u8;
    }
    table
}

/// Decodes base64 groups: whole groups of four alphabet characters go
/// through a table, anything else (padding, invalid characters, a trailing
/// short group) through gnulib's per-group logic in [`decode_base64_group`].
fn decode_base64(
    clean: &[u8],
    alphabet: &[u8; 64],
    table: &DecodeTable,
    out: &mut Vec<u8>,
) -> bool {
    let start = out.len();
    // Room for every group decoded in full; truncated to what was written.
    out.resize(start + clean.len().div_ceil(4) * 3, 0);
    let mut w = start;
    let (groups, tail) = clean.as_chunks::<4>();
    let mut slow = Vec::with_capacity(3);
    let mut ok = true;
    for g in groups {
        let (a, b, c, d) = (
            table[g[0] as usize],
            table[g[1] as usize],
            table[g[2] as usize],
            table[g[3] as usize],
        );
        if (a | b | c | d) & INVALID == 0 {
            let n =
                (u32::from(a) << 18) | (u32::from(b) << 12) | (u32::from(c) << 6) | u32::from(d);
            out[w..w + 3].copy_from_slice(&n.to_be_bytes()[1..]);
            w += 3;
            continue;
        }
        slow.clear();
        ok = decode_base64_group(g, alphabet, &mut slow);
        out[w..w + slow.len()].copy_from_slice(&slow);
        w += slow.len();
        if !ok {
            break;
        }
    }
    if ok && !tail.is_empty() {
        slow.clear();
        ok = decode_base64_group(tail, alphabet, &mut slow);
        out[w..w + slow.len()].copy_from_slice(&slow);
        w += slow.len();
    }
    out.truncate(w);
    ok
}

/// gnulib's `decode_4`: one (possibly short) group of base64 characters,
/// emitting each byte as soon as the characters behind it are known valid.
fn decode_base64_group(g: &[u8], alphabet: &[u8; 64], out: &mut Vec<u8>) -> bool {
    let val = |c: u8| alphabet.iter().position(|&a| a == c).map(|p| p as u32);
    if g.len() < 2 {
        return false;
    }
    let (Some(v0), Some(v1)) = (val(g[0]), val(g[1])) else {
        return false;
    };
    out.push(((v0 << 2) | (v1 >> 4)) as u8);
    if g.len() == 2 {
        return false;
    }
    if g[2] == b'=' {
        return g.len() == 4 && g[3] == b'=';
    }
    let Some(v2) = val(g[2]) else { return false };
    out.push((((v1 << 4) & 0xf0) | (v2 >> 2)) as u8);
    if g.len() == 3 {
        return false;
    }
    if g[3] == b'=' {
        return true;
    }
    let Some(v3) = val(g[3]) else { return false };
    out.push((((v2 << 6) & 0xc0) | v3) as u8);
    true
}

/// gnulib's `decode_8`: a group of eight base32 characters yields up to five
/// bytes; a short group decodes nothing, a malformed one stops part way.
fn decode_base32_group(g: &[u8], alphabet: &[u8; 32], out: &mut Vec<u8>) -> bool {
    let val = |c: u8| {
        alphabet
            .iter()
            .position(|&a| a == c)
            .or_else(|| {
                // GNU decodes base32hex by translating its alphabet to the
                // standard one and passing anything else through, so the
                // standard-only letters W-Z keep their standard values.
                (alphabet == B32_HEX && (b'W'..=b'Z').contains(&c))
                    .then(|| usize::from(c - b'W') + 22)
            })
            .map(|p| p as u64)
    };
    if g.len() < 8 {
        return false;
    }
    let (Some(v0), Some(v1)) = (val(g[0]), val(g[1])) else {
        return false;
    };
    // 40 bits: character k contributes bits 35-5k..39-5k.
    let mut acc = (v0 << 35) | (v1 << 30);
    let byte = |acc: u64, n: u32| ((acc >> (32 - 8 * n)) & 0xff) as u8;
    out.push(byte(acc, 0));
    // After characters 0-1, gnulib checks 2-3, 4, 5-6 and 7, one byte each.
    let steps: [(&[usize], u32); 4] = [(&[2, 3], 1), (&[4], 2), (&[5, 6], 3), (&[7], 4)];
    for (chars, nbyte) in steps {
        if g[chars[0]] == b'=' {
            return g[chars[0]..].iter().all(|&c| c == b'=');
        }
        for &k in chars {
            let Some(v) = val(g[k]) else { return false };
            acc |= v << (35 - 5 * k as u32);
        }
        out.push(byte(acc, nbyte));
    }
    true
}

/// Bytes encoded per block. GNU's block (30720 bytes) is a multiple of 3, 4
/// and 5, so no padding appears before the last block, and `--z85` reports a
/// ragged length on the block that holds the tail, after writing the blocks
/// before it; a multiple of it keeps both true here.
const GNU_ENC_BLOCK: usize = 30 * 1024;
const ENC_BLOCK: usize = 4 * GNU_ENC_BLOCK;

/// Bytes read per decoding step.
const DEC_BLOCK: usize = 128 * 1024;

/// What `basenc --base64url -d` reads per step (`BASE_LENGTH (DEC_BLOCKSIZE)`):
/// GNU rejects a whole step that holds a `+` or `/`, keeping what it decoded
/// from the steps before.
const GNU_URL_DEC_STEP: usize = 5600;

/// Why streaming stopped early.
enum StreamError {
    Read(io::Error),
    Write(io::Error),
}

/// Reads until `buf` is full or the input ends, as `fread` does.
fn fill(input: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match input.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// One `read`, retried on `EINTR`.
fn read_some(input: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match input.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            r => return r,
        }
    }
}

/// Two output characters for every 12-bit input value: one lookup per half
/// of a 3-byte group.
fn pair_table(alphabet: &[u8; 64]) -> Vec<[u8; 2]> {
    (0..4096)
        .map(|v| [alphabet[v >> 6], alphabet[v & 63]])
        .collect()
}

/// Appends the base64 encoding of `data` (padded if its length is not a
/// multiple of 3) to `out`.
fn encode_base64(data: &[u8], alphabet: &[u8; 64], pairs: &[[u8; 2]], out: &mut Vec<u8>) {
    let start = out.len();
    out.resize(start + data.len().div_ceil(3) * 4, 0);
    let (groups, rest) = data.as_chunks::<3>();
    let (whole, tail) = out[start..].split_at_mut(groups.len() * 4);
    for (g, o) in groups.iter().zip(whole.as_chunks_mut::<4>().0) {
        let n = (usize::from(g[0]) << 16) | (usize::from(g[1]) << 8) | usize::from(g[2]);
        let (hi, lo) = (pairs[n >> 12], pairs[n & 0xfff]);
        *o = [hi[0], hi[1], lo[0], lo[1]];
    }
    if let [b0, more @ ..] = rest {
        let b1 = more.first().copied().unwrap_or(0);
        tail[0] = alphabet[usize::from(b0 >> 2)];
        tail[1] = alphabet[usize::from(((b0 & 3) << 4) | (b1 >> 4))];
        tail[2] = if more.is_empty() {
            b'='
        } else {
            alphabet[usize::from((b1 & 15) << 2)]
        };
        tail[3] = b'=';
    }
}

/// GNU's `wrap_write`: appends `data` to `out`, breaking lines at `wrap`
/// columns. A line's newline is written only once more data follows it (or
/// by the caller at the end), so `column` runs from 0 to `wrap`.
fn wrap_into(data: &[u8], wrap: usize, column: &mut usize, out: &mut Vec<u8>) {
    if wrap == 0 {
        out.extend_from_slice(data);
        return;
    }
    let mut rest = data;
    while !rest.is_empty() {
        if *column == wrap {
            out.push(b'\n');
            *column = 0;
        }
        let n = (wrap - *column).min(rest.len());
        out.extend_from_slice(&rest[..n]);
        *column += n;
        rest = &rest[n..];
    }
}

/// GNU's `do_encode`: encodes `input` block by block. Returns the z85
/// length error, if any, after writing what GNU writes before it.
fn encode_stream(
    encoding: Encoding,
    input: &mut dyn Read,
    out: &mut dyn Write,
    wrap: usize,
    ctx: &DogfishContext,
) -> std::result::Result<Option<CodecError>, StreamError> {
    let mut inbuf = vec![0u8; ENC_BLOCK];
    let mut outbuf = Vec::with_capacity(ENC_BLOCK * 2);
    let alphabet = match encoding {
        Encoding::Base64Url => URL_B64,
        _ => STD_B64,
    };
    // With whole groups per line, base64 is encoded straight into its lines.
    let direct =
        matches!(encoding, Encoding::Base64 | Encoding::Base64Url) && wrap.is_multiple_of(4);
    let pairs = if direct {
        pair_table(alphabet)
    } else {
        Vec::new()
    };
    let mut column = 0;
    let mut error = None;
    loop {
        let n = fill(input, &mut inbuf).map_err(StreamError::Read)?;
        ctx.telemetry.add_read(n as u64);
        let mut block = &inbuf[..n];
        if encoding == Encoding::Z85 && !n.is_multiple_of(4) {
            // GNU dies on its block holding the ragged tail, unwritten.
            block = &block[..n / GNU_ENC_BLOCK * GNU_ENC_BLOCK];
            error = Some(CodecError::Z85Length);
        }
        outbuf.clear();
        if direct {
            let mut rest = block;
            while !rest.is_empty() {
                let take = if wrap == 0 {
                    rest.len()
                } else {
                    if column == wrap {
                        outbuf.push(b'\n');
                        column = 0;
                    }
                    ((wrap - column) / 4 * 3).min(rest.len())
                };
                encode_base64(&rest[..take], alphabet, &pairs, &mut outbuf);
                column += take.div_ceil(3) * 4;
                rest = &rest[take..];
            }
        } else if !block.is_empty() {
            match encoding.encode(block) {
                Ok(encoded) => wrap_into(&encoded, wrap, &mut column, &mut outbuf),
                Err(e) => error = Some(e),
            }
        }
        out.write_all(&outbuf).map_err(StreamError::Write)?;
        ctx.telemetry.add_written(outbuf.len() as u64);
        if error.is_some() {
            return Ok(error);
        }
        if n < ENC_BLOCK {
            break;
        }
    }
    // When wrapping, terminate the last line.
    if wrap != 0 && column > 0 {
        out.write_all(b"\n").map_err(StreamError::Write)?;
    }
    Ok(None)
}

/// GNU's `do_decode`: filters and decodes `input` a block at a time, carrying
/// a partial group from one block to the next, so memory stays bounded.
fn decode_stream(
    encoding: Encoding,
    input: &mut dyn Read,
    out: &mut dyn Write,
    ignore_garbage: bool,
    ctx: &DogfishContext,
) -> std::result::Result<Option<CodecError>, StreamError> {
    // basenc --base64url rejects a whole GNU-sized step holding '+' or '/';
    // reading in exactly those steps reproduces what it writes first. With
    // --ignore-garbage those characters are filtered out instead.
    let url_steps = encoding == Encoding::Base64Url && !ignore_garbage;
    let mut raw = vec![
        0u8;
        if url_steps {
            GNU_URL_DEC_STEP
        } else {
            DEC_BLOCK
        }
    ];
    let mut clean: Vec<u8> = Vec::with_capacity(raw.len() + 8);
    let mut outbuf = Vec::with_capacity(DEC_BLOCK);
    // What --ignore-garbage keeps: GNU keeps '=' for every encoding, so it
    // is invalid input, not garbage, for those without padding.
    let mut keep = [false; 256];
    for c in 0..=255u8 {
        keep[usize::from(c)] = encoding.in_alphabet(c) || c == b'=';
    }
    let table = decode_table(match encoding {
        Encoding::Base64Url => URL_B64,
        _ => STD_B64,
    });
    let group = encoding.group_len();
    loop {
        let n = if url_steps {
            fill(input, &mut raw)
        } else {
            read_some(input, &mut raw)
        }
        .map_err(StreamError::Read)?;
        ctx.telemetry.add_read(n as u64);
        let chunk = &raw[..n];
        if url_steps && chunk.iter().any(|c| matches!(c, b'+' | b'/')) {
            return Ok(Some(CodecError::Invalid));
        }
        if ignore_garbage {
            clean.extend(chunk.iter().copied().filter(|&c| keep[usize::from(c)]));
        } else {
            let mut from = 0;
            for nl in memchr::memchr_iter(b'\n', chunk) {
                clean.extend_from_slice(&chunk[from..nl]);
                from = nl + 1;
            }
            clean.extend_from_slice(&chunk[from..]);
        }
        // Decode the whole groups; at the end of input, the short one too.
        let ready = if n == 0 {
            clean.len()
        } else {
            clean.len() / group * group
        };
        outbuf.clear();
        let ok = encoding.decode_groups(&clean[..ready], &table, &mut outbuf);
        out.write_all(&outbuf).map_err(StreamError::Write)?;
        ctx.telemetry.add_written(outbuf.len() as u64);
        if !ok {
            return Ok(Some(CodecError::Invalid));
        }
        clean.drain(..ready);
        if n == 0 {
            return Ok(None);
        }
    }
}

/// The options `base64` and `basenc` share.
struct CodecOptions<'a> {
    decode: bool,
    ignore_garbage: bool,
    wrap: &'a str,
    file: Option<&'a str>,
}

/// The shared body of `base64` and `basenc`.
fn run_codec(
    tool: &str,
    encoding: Encoding,
    opts: CodecOptions<'_>,
    ctx: &DogfishContext,
) -> Result<i32> {
    let Some(wrap) = opts.wrap.trim_start().parse::<u64>().ok() else {
        eprintln!("{tool}: invalid wrap size: {}", gnu_quote(opts.wrap));
        return Ok(1);
    };
    let wrap = usize::try_from(wrap).unwrap_or(usize::MAX);
    let mut input: Box<dyn Read> = match opts.file {
        Some(f) if f != "-" => Box::new(
            File::open(f).map_err(|e| anyhow!("{}: {}", gnu_quote_file(f), gnu_io_error(&e)))?,
        ),
        _ => Box::new(io::stdin().lock()),
    };
    // Each write is one of GNU's checked `fwrite` calls: the first that has
    // to write and fails ends the run with `write error`.
    let mut out = stdout::GnuFwrite::new(stdout::stdout());
    let result = if opts.decode {
        decode_stream(encoding, &mut *input, &mut out, opts.ignore_garbage, ctx)
    } else {
        encode_stream(encoding, &mut *input, &mut out, wrap, ctx)
    };
    match result {
        Ok(None) => Ok(0),
        Ok(Some(CodecError::Invalid)) => {
            eprintln!("{tool}: invalid input");
            Ok(1)
        }
        Ok(Some(CodecError::Z85Length)) => {
            eprintln!("{tool}: invalid input (length must be multiple of 4 characters)");
            Ok(1)
        }
        Err(StreamError::Read(e)) => {
            eprintln!("{tool}: read error: {}", gnu_io_error(&e));
            Ok(1)
        }
        // GNU's `write_error`, which still knows why.
        Err(StreamError::Write(e)) if stdout::is_write_error(&e) => {
            stdout::mark_reported();
            eprintln!("{tool}: write error: {}", gnu_io_error(&e));
            Ok(1)
        }
        Err(StreamError::Write(e)) => Err(e.into()),
    }
}

const BASE64_HELP: &str = "\
Base64 encode or decode FILE, or standard input, to standard output

Usage: base64 [OPTIONS] [FILE]

Arguments:
  [FILE]  Input file (default: stdin)

Options:
  -d, --decode          Decode data
  -i, --ignore-garbage  When decoding, ignore non-alphabet characters
  -w, --wrap <COLS>     Wrap encoded lines after COLS character (default 76). Use 0 to disable line
                        wrapping [default: 76]
      --help            Display this help and exit
";

const BASENC_HELP: &str = "\
basenc encode or decode FILE, or standard input, to standard output

Usage: basenc [OPTIONS] [FILE]

Arguments:
  [FILE]  Input file

Options:
      --base64...       Same as 'base64' program (RFC4648 section 4)
      --base64url...    File- and url-safe base64 (RFC4648 section 5)
      --base32...       Same as 'base32' program (RFC4648 section 6)
      --base32hex...    Extended hex alphabet base32 (RFC4648 section 7)
      --base16...       Hex encoding (RFC4648 section 8)
      --base2msbf...    Bit string with most significant bit (msb) first
      --base2lsbf...    Bit string with least significant bit (lsb) first
      --z85...          Ascii85-like encoding (ZeroMQ spec:32/Z85); when encoding, input length must
                        be a multiple of 4; when decoding, input length must be a multiple of 5
  -d, --decode          Decode data
  -i, --ignore-garbage  When decoding, ignore non-alphabet characters
  -w, --wrap <COLS>     Wrap encoded lines after COLS character (default 76). Use 0 to disable line
                        wrapping [default: 76]
      --help            Display this help and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum CodecOpt {
    Decode,
    Wrap,
    IgnoreGarbage,
    Encoding(Encoding),
    Help,
    Version,
}

/// GNU's `long_options` and `"diw:"`; the encodings are basenc's only.
const BASENC_SPECS: &[OptSpec<CodecOpt>] = &[
    OptSpec::both(b'd', "decode", HasArg::No, CodecOpt::Decode),
    OptSpec::both(b'w', "wrap", HasArg::Required, CodecOpt::Wrap),
    OptSpec::both(b'i', "ignore-garbage", HasArg::No, CodecOpt::IgnoreGarbage),
    OptSpec::long("help", HasArg::No, CodecOpt::Help),
    OptSpec::long("version", HasArg::No, CodecOpt::Version),
    OptSpec::long("base64", HasArg::No, CodecOpt::Encoding(Encoding::Base64)),
    OptSpec::long(
        "base64url",
        HasArg::No,
        CodecOpt::Encoding(Encoding::Base64Url),
    ),
    OptSpec::long("base32", HasArg::No, CodecOpt::Encoding(Encoding::Base32)),
    OptSpec::long(
        "base32hex",
        HasArg::No,
        CodecOpt::Encoding(Encoding::Base32Hex),
    ),
    OptSpec::long("base16", HasArg::No, CodecOpt::Encoding(Encoding::Base16)),
    OptSpec::long(
        "base2msbf",
        HasArg::No,
        CodecOpt::Encoding(Encoding::Base2Msbf),
    ),
    OptSpec::long(
        "base2lsbf",
        HasArg::No,
        CodecOpt::Encoding(Encoding::Base2Lsbf),
    ),
    OptSpec::long("z85", HasArg::No, CodecOpt::Encoding(Encoding::Z85)),
];

/// `tool`'s table: basenc's, or base64's, which is the same without the
/// encodings (they come last).
fn specs_for(tool: &str) -> &'static [OptSpec<CodecOpt>] {
    if tool == "basenc" {
        BASENC_SPECS
    } else {
        &BASENC_SPECS[..5]
    }
}

/// The command line of `base64` or `basenc`.
struct CodecArgs {
    decode: bool,
    ignore_garbage: bool,
    wrap: String,
    /// The last encoding option (basenc), as in GNU.
    encoding: Option<Encoding>,
    file: Option<String>,
}

/// Parses `tool`'s command line; `Err` is the exit status after `--help`,
/// `--version` or a usage diagnostic.
fn parse_codec_args(
    tool: &str,
    args: Vec<OsString>,
    help: &str,
) -> std::result::Result<CodecArgs, i32> {
    let mut p = CodecArgs {
        decode: false,
        ignore_garbage: false,
        wrap: "76".to_string(),
        encoding: None,
        file: None,
    };
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), specs_for(tool));
    while let Some(opt) = opts.next() {
        let (o, v) = opt.map_err(|e| getopt_error(tool, &e))?;
        match o {
            CodecOpt::Decode => p.decode = true,
            CodecOpt::Wrap => p.wrap = v.as_deref().map(lossy).unwrap_or_default(),
            CodecOpt::IgnoreGarbage => p.ignore_garbage = true,
            CodecOpt::Encoding(e) => p.encoding = Some(e),
            CodecOpt::Help => return Err(print_help(help)),
            CodecOpt::Version => return Err(print_version(tool)),
        }
    }
    let operands = lossy_all(opts.operands());
    if let Some(extra) = operands.get(1) {
        return Err(usage_error(
            tool,
            format!("extra operand {}", gnu_quote(extra)),
        ));
    }
    p.file = operands.into_iter().next();
    Ok(p)
}

pub fn run_base64(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_codec_args("base64", args, BASE64_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let opts = CodecOptions {
        decode: parsed.decode,
        ignore_garbage: parsed.ignore_garbage,
        wrap: &parsed.wrap,
        file: parsed.file.as_deref(),
    };
    run_codec("base64", Encoding::Base64, opts, ctx)
}

pub fn run_basenc(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_codec_args("basenc", args, BASENC_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let Some(encoding) = parsed.encoding else {
        return Ok(usage_error("basenc", "missing encoding type"));
    };
    let opts = CodecOptions {
        decode: parsed.decode,
        ignore_garbage: parsed.ignore_garbage,
        wrap: &parsed.wrap,
        file: parsed.file.as_deref(),
    };
    run_codec("basenc", encoding, opts, ctx)
}

/// The option table and help text of `tool` (base64 or basenc), for its
/// manual page and shell completions (`crate::manual`).
pub(crate) fn manual(tool: &'static str) -> crate::manual::Manual {
    let help = if tool == "basenc" {
        BASENC_HELP
    } else {
        BASE64_HELP
    };
    crate::manual::Manual::new(tool, help, specs_for(tool))
}

/// Entry points for the fuzz targets in `fuzz/` (only with the `fuzzing` feature).
#[cfg(feature = "fuzzing")]
#[allow(dead_code)]
pub mod fuzz {
    use super::*;

    const ALL: [Encoding; 8] = [
        Encoding::Base64,
        Encoding::Base64Url,
        Encoding::Base32,
        Encoding::Base32Hex,
        Encoding::Base16,
        Encoding::Base2Msbf,
        Encoding::Base2Lsbf,
        Encoding::Z85,
    ];

    /// Every encoding, through the streaming coder `base64`/`basenc` use:
    /// decoding what was encoded (wrapped or not) gives back the data, and
    /// decoding anything never panics.
    pub fn round_trip(data: &[u8]) {
        let ctx = DogfishContext::new("basenc");
        for e in ALL {
            for wrap in [0, 76] {
                let mut enc = Vec::new();
                let Ok(None) = encode_stream(e, &mut &data[..], &mut enc, wrap, &ctx) else {
                    continue;
                };
                let mut dec = Vec::new();
                let r = decode_stream(e, &mut &enc[..], &mut dec, false, &ctx);
                assert!(matches!(r, Ok(None)), "{data:?} wrap {wrap}");
                assert_eq!(dec, data, "wrap {wrap}");
            }
            for garbage in [false, true] {
                let _ = decode_stream(e, &mut &data[..], &mut Vec::new(), garbage, &ctx);
            }
        }
    }
}
