//! base64 / basenc

use std::ffi::OsString;
use std::io::{self, Write};

use anyhow::{Result, anyhow};
use data_encoding::{BASE32, BASE32HEX, HEXUPPER};
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, gnu_io_error, gnu_quote,
    gnu_quote_file, io::ignore_broken_pipe, stdout,
};

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
    // base64 has the same table without the encodings (they come last).
    let specs = if tool == "basenc" {
        BASENC_SPECS
    } else {
        &BASENC_SPECS[..5]
    };
    let mut p = CodecArgs {
        decode: false,
        ignore_garbage: false,
        wrap: "76".to_string(),
        encoding: None,
        file: None,
    };
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), specs);
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
