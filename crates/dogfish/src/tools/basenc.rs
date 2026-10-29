//! base64 / basenc

use std::io::{self, Write};

use anyhow::{Result, anyhow};
use clap::{ArgAction, CommandFactory, FromArgMatches, Parser};
use data_encoding::{BASE32, BASE32HEX, HEXUPPER};
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, gnu_io_error, gnu_quote,
    gnu_quote_file, io::ignore_broken_pipe, stdout,
};

use crate::common::arg_order::last_index;

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
    /// Whether `c` belongs to the alphabet (padding included), for `--ignore-garbage`.
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

    /// Decodes like GNU: newlines are skipped (with `ignore_garbage`, everything
    /// outside the alphabet), and on invalid input the bytes decoded so far are
    /// returned along with the error.
    fn decode(self, input: &[u8], ignore_garbage: bool) -> (Vec<u8>, Option<CodecError>) {
        let clean: Vec<u8> = input
            .iter()
            .copied()
            .filter(|&c| {
                if ignore_garbage {
                    self.in_alphabet(c)
                } else {
                    c != b'\n'
                }
            })
            .collect();
        let mut out = Vec::with_capacity(clean.len());
        let ok = match self {
            Self::Base64 => clean
                .chunks(4)
                .all(|group| decode_base64_group(group, STD_B64, &mut out)),
            // GNU rejects the standard alphabet's '+' and '/' up front.
            Self::Base64Url => {
                !clean.iter().any(|c| matches!(c, b'+' | b'/'))
                    && clean
                        .chunks(4)
                        .all(|group| decode_base64_group(group, URL_B64, &mut out))
            }
            Self::Base32 => clean
                .chunks(8)
                .all(|group| decode_base32_group(group, B32, &mut out)),
            Self::Base32Hex => clean
                .chunks(8)
                .all(|group| decode_base32_group(group, B32_HEX, &mut out)),
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
        };
        (out, (!ok).then_some(CodecError::Invalid))
    }
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
    let val = |c: u8| alphabet.iter().position(|&a| a == c).map(|p| p as u64);
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

fn write_wrapped<W: Write>(out: &mut W, data: &[u8], wrap: usize) -> io::Result<()> {
    if data.is_empty() {
        // GNU base64 emits nothing at all for empty input.
        return Ok(());
    }
    if wrap == 0 {
        // GNU: `-w 0` disables wrapping and the trailing newline.
        return ignore_broken_pipe(out.write_all(data));
    }
    for chunk in data.chunks(wrap) {
        ignore_broken_pipe(out.write_all(chunk))?;
        ignore_broken_pipe(out.write_all(b"\n"))?;
    }
    Ok(())
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
    let src = match opts.file {
        Some(f) if f != "-" => InputSource::File(f.to_string()),
        _ => InputSource::Stdin,
    };
    let reader = FastReader::open(&src).map_err(|e| {
        anyhow!(
            "{}: {}",
            gnu_quote_file(src.display_name()),
            gnu_io_error(&e)
        )
    })?;
    let input = match reader.into_bytes() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{tool}: read error: {}", gnu_io_error(&e));
            return Ok(1);
        }
    };
    ctx.telemetry.add_read(input.len() as u64);
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout::stdout());
    let error = if opts.decode {
        let (bytes, err) = encoding.decode(&input, opts.ignore_garbage);
        ignore_broken_pipe(out.write_all(&bytes))?;
        ctx.telemetry.add_written(bytes.len() as u64);
        err
    } else {
        match encoding.encode(&input) {
            Ok(encoded) => {
                let wrap = usize::try_from(wrap).unwrap_or(usize::MAX);
                write_wrapped(&mut out, &encoded, wrap)?;
                ctx.telemetry.add_written(encoded.len() as u64);
                None
            }
            Err(e) => Some(e),
        }
    };
    ignore_broken_pipe(out.flush())?;
    match error {
        None => Ok(0),
        Some(CodecError::Invalid) => {
            eprintln!("{tool}: invalid input");
            Ok(1)
        }
        Some(CodecError::Z85Length) => {
            eprintln!("{tool}: invalid input (length must be multiple of 4 characters)");
            Ok(1)
        }
    }
}

#[derive(Parser)]
#[command(
    name = "base64",
    about = "Base64 encode or decode FILE, or standard input, to standard output",
    disable_help_flag = true,
    infer_long_args = true
)]
struct Base64Args {
    /// Decode data
    #[arg(short = 'd', long = "decode")]
    decode: bool,

    /// When decoding, ignore non-alphabet characters
    #[arg(short = 'i', long = "ignore-garbage")]
    ignore_garbage: bool,

    /// Wrap encoded lines after COLS character (default 76). Use 0 to disable line wrapping
    #[arg(
        short = 'w',
        long = "wrap",
        value_name = "COLS",
        default_value = "76",
        allow_hyphen_values = true
    )]
    wrap: String,

    /// Display this help and exit
    #[arg(long, action = ArgAction::Help)]
    help: Option<bool>,

    /// Input file (default: stdin)
    file: Option<String>,
}

pub fn run_base64(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = Base64Args::try_parse_from(args)?;
    let opts = CodecOptions {
        decode: parsed.decode,
        ignore_garbage: parsed.ignore_garbage,
        wrap: &parsed.wrap,
        file: parsed.file.as_deref(),
    };
    run_codec("base64", Encoding::Base64, opts, ctx)
}

#[derive(Parser)]
#[command(
    name = "basenc",
    about = "basenc encode or decode FILE, or standard input, to standard output",
    disable_help_flag = true,
    infer_long_args = true
)]
struct BasencArgs {
    /// Same as 'base64' program (RFC4648 section 4)
    #[arg(long = "base64", action = ArgAction::Count)]
    base64: u8,
    /// File- and url-safe base64 (RFC4648 section 5)
    #[arg(long = "base64url", action = ArgAction::Count)]
    base64url: u8,
    /// Same as 'base32' program (RFC4648 section 6)
    #[arg(long = "base32", action = ArgAction::Count)]
    base32: u8,
    /// Extended hex alphabet base32 (RFC4648 section 7)
    #[arg(long = "base32hex", action = ArgAction::Count)]
    base32hex: u8,
    /// Hex encoding (RFC4648 section 8)
    #[arg(long = "base16", action = ArgAction::Count)]
    base16: u8,
    /// Bit string with most significant bit (msb) first
    #[arg(long = "base2msbf", action = ArgAction::Count)]
    base2msbf: u8,
    /// Bit string with least significant bit (lsb) first
    #[arg(long = "base2lsbf", action = ArgAction::Count)]
    base2lsbf: u8,
    /// Ascii85-like encoding (ZeroMQ spec:32/Z85); when encoding, input length
    /// must be a multiple of 4; when decoding, input length must be a multiple of 5
    #[arg(long = "z85", action = ArgAction::Count)]
    z85: u8,

    /// Decode data
    #[arg(short = 'd', long = "decode")]
    decode: bool,

    /// When decoding, ignore non-alphabet characters
    #[arg(short = 'i', long = "ignore-garbage")]
    ignore_garbage: bool,

    /// Wrap encoded lines after COLS character (default 76). Use 0 to disable line wrapping
    #[arg(
        short = 'w',
        long = "wrap",
        value_name = "COLS",
        default_value = "76",
        allow_hyphen_values = true
    )]
    wrap: String,

    /// Display this help and exit
    #[arg(long, action = ArgAction::Help)]
    help: Option<bool>,

    /// Input file
    file: Option<String>,
}

pub fn run_basenc(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let matches = BasencArgs::command().try_get_matches_from(args)?;
    let parsed = BasencArgs::from_arg_matches(&matches)?;
    // As in GNU, the last encoding option given wins.
    let encodings = [
        ("base64", Encoding::Base64),
        ("base64url", Encoding::Base64Url),
        ("base32", Encoding::Base32),
        ("base32hex", Encoding::Base32Hex),
        ("base16", Encoding::Base16),
        ("base2msbf", Encoding::Base2Msbf),
        ("base2lsbf", Encoding::Base2Lsbf),
        ("z85", Encoding::Z85),
    ];
    let latest = encodings
        .iter()
        .filter_map(|(id, enc)| last_index(&matches, id).map(|i| (i, *enc)))
        .max_by_key(|(i, _)| *i);
    let Some((_, encoding)) = latest else {
        eprintln!("basenc: missing encoding type\nTry 'basenc --help' for more information.");
        return Ok(1);
    };
    let opts = CodecOptions {
        decode: parsed.decode,
        ignore_garbage: parsed.ignore_garbage,
        wrap: &parsed.wrap,
        file: parsed.file.as_deref(),
    };
    run_codec("basenc", encoding, opts, ctx)
}
