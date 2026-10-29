//! tr — Byte-oriented GNU translate / squeeze / delete with classes & escapes

use std::io::{self, Read, Write};

use anyhow::Result;
use clap::Parser;
use dogfish_core::{DEFAULT_BUF_SIZE, DogfishContext, io::ignore_broken_pipe};
use memchr::memmem;

#[derive(Parser)]
#[command(
    name = "tr",
    about = "Translate, squeeze, and/or delete characters from standard input"
)]
pub(crate) struct TrArgs {
    /// Use the complement of SET1
    #[arg(short = 'c', visible_short_alias = 'C', long = "complement")]
    pub(crate) complement: bool,

    /// Delete characters in SET1, do not translate
    #[arg(short = 'd', long = "delete")]
    delete: bool,

    /// Replace each sequence of a repeated character with a single occurrence
    #[arg(short = 's', long = "squeeze-repeats")]
    pub(crate) squeeze: bool,

    /// First truncate SET1 to length of SET2
    #[arg(short = 't', long = "truncate-set1")]
    pub(crate) truncate: bool,

    /// Like GNU (`getopt "+..."`), options end at the first set: `tr a -b` maps `a`
    /// to `-b`, while `tr --bogus a b` is an unrecognized option.
    #[arg(trailing_var_arg = true)]
    sets: Vec<String>,
}

/// An expanded tr SET: the bytes, plus where a `[c*]` fill repeat goes (SET2 only).
struct TrSet {
    pub(crate) bytes: Vec<u8>,
    fill: Option<(usize, u8)>,
}

/// Parses one (possibly escaped) character at `s[*i]`.
fn tr_char(s: &[u8], i: &mut usize) -> u8 {
    let c = s[*i];
    *i += 1;
    if c != b'\\' || *i >= s.len() {
        return c;
    }
    let e = s[*i];
    *i += 1;
    match e {
        b'a' => 0x07,
        b'b' => 0x08,
        b'f' => 0x0c,
        b'n' => b'\n',
        b'r' => b'\r',
        b't' => b'\t',
        b'v' => 0x0b,
        b'0'..=b'7' => {
            let mut v = (e - b'0') as u32;
            for _ in 0..2 {
                match s.get(*i) {
                    Some(&d @ b'0'..=b'7') if v * 8 + ((d - b'0') as u32) <= 255 => {
                        v = v * 8 + (d - b'0') as u32;
                        *i += 1;
                    }
                    _ => break,
                }
            }
            v as u8
        }
        other => other,
    }
}

fn tr_class(name: &[u8]) -> Option<Vec<u8>> {
    let pred: fn(u8) -> bool = match name {
        b"alnum" => |c| c.is_ascii_alphanumeric(),
        b"alpha" => |c| c.is_ascii_alphabetic(),
        b"blank" => |c| c == b' ' || c == b'\t',
        b"cntrl" => |c| c.is_ascii_control(),
        b"digit" => |c| c.is_ascii_digit(),
        b"graph" => |c| c.is_ascii_graphic(),
        b"lower" => |c| c.is_ascii_lowercase(),
        b"print" => |c| (0x20..0x7f).contains(&c),
        b"punct" => |c| c.is_ascii_punctuation(),
        b"space" => |c| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c),
        b"upper" => |c| c.is_ascii_uppercase(),
        b"xdigit" => |c| c.is_ascii_hexdigit(),
        _ => return None,
    };
    Some((0..=255u8).filter(|&c| pred(c)).collect())
}

fn parse_tr_set(raw: &str) -> std::result::Result<TrSet, String> {
    let s = raw.as_bytes();
    let mut out = TrSet {
        bytes: Vec::new(),
        fill: None,
    };
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'[' && i + 1 < s.len() {
            // [:class:]
            if s[i + 1] == b':'
                && let Some(end) = memmem::find(&s[i + 2..], b":]")
            {
                let name = &s[i + 2..i + 2 + end];
                let bytes = tr_class(name).ok_or_else(|| {
                    format!(
                        "invalid character class '{}'",
                        String::from_utf8_lossy(name)
                    )
                })?;
                out.bytes.extend(bytes);
                i += end + 4;
                continue;
            }
            // [=c=]
            if s[i + 1] == b'=' {
                let mut j = i + 2;
                if j < s.len() {
                    let c = tr_char(s, &mut j);
                    if s[j..].starts_with(b"=]") {
                        out.bytes.push(c);
                        i = j + 2;
                        continue;
                    }
                }
            }
            // [c*n] / [c*]
            let mut j = i + 1;
            let c = tr_char(s, &mut j);
            if j < s.len() && s[j] == b'*' {
                let ns = j + 1;
                let mut k = ns;
                while k < s.len() && s[k].is_ascii_digit() {
                    k += 1;
                }
                if k < s.len() && s[k] == b']' {
                    let digits = &raw[ns..k];
                    let n = if digits.starts_with('0') {
                        usize::from_str_radix(digits, 8).unwrap_or(0)
                    } else {
                        digits.parse().unwrap_or(0)
                    };
                    if n == 0 {
                        out.fill = Some((out.bytes.len(), c));
                    } else {
                        out.bytes.extend(std::iter::repeat_n(c, n));
                    }
                    i = k + 1;
                    continue;
                }
            }
        }
        let c = tr_char(s, &mut i);
        if i + 1 < s.len() && s[i] == b'-' {
            let mut j = i + 1;
            let end = tr_char(s, &mut j);
            if end < c {
                return Err(format!(
                    "range-endpoints of '{}' are in reverse collating sequence order",
                    String::from_utf8_lossy(&s[i - 1..j])
                ));
            }
            out.bytes.extend(c..=end);
            i = j;
            continue;
        }
        out.bytes.push(c);
    }
    Ok(out)
}

pub fn run_tr(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = TrArgs::try_parse_from(args)?;
    let usage_err = |msg: String| {
        eprintln!("tr: {}\nTry 'tr --help' for more information.", msg);
        Ok(1)
    };
    let nsets = parsed.sets.len();
    let translating = !parsed.delete && nsets == 2;
    if nsets == 0 {
        return usage_err("missing operand".to_string());
    }
    if nsets == 1 && !parsed.delete && !parsed.squeeze {
        return usage_err(format!(
            "missing operand after '{}'\nTwo strings must be given when translating.",
            parsed.sets[0]
        ));
    }
    if parsed.delete && parsed.squeeze && nsets == 1 {
        return usage_err(format!(
            "missing operand after '{}'\nTwo strings must be given when both deleting and squeezing repeats.",
            parsed.sets[0]
        ));
    }
    if parsed.delete && !parsed.squeeze && nsets == 2 {
        return usage_err(format!(
            "extra operand '{}'\nOnly one string may be given when deleting without squeezing repeats.",
            parsed.sets[1]
        ));
    }
    if nsets > 2 {
        return usage_err(format!("extra operand '{}'", parsed.sets[2]));
    }

    let set1 = match parse_tr_set(&parsed.sets[0]) {
        Ok(s) => s,
        Err(m) => {
            eprintln!("tr: {}", m);
            return Ok(1);
        }
    };
    let mut s1: Vec<u8> = set1.bytes;
    if parsed.complement {
        let mut member = [false; 256];
        for &b in &s1 {
            member[b as usize] = true;
        }
        s1 = (0..=255u8).filter(|&b| !member[b as usize]).collect();
    }
    let s2: Vec<u8> = match parsed.sets.get(1) {
        None => Vec::new(),
        Some(raw) => match parse_tr_set(raw) {
            Ok(mut set2) => {
                if let Some((pos, c)) = set2.fill {
                    let n = s1.len().saturating_sub(set2.bytes.len());
                    set2.bytes.splice(pos..pos, std::iter::repeat_n(c, n));
                }
                set2.bytes
            }
            Err(m) => {
                eprintln!("tr: {}", m);
                return Ok(1);
            }
        },
    };

    let mut map: [u8; 256] = std::array::from_fn(|i| i as u8);
    let mut delete = [false; 256];
    let mut squeeze = [false; 256];
    if translating {
        let mut s1 = s1.clone();
        let mut s2 = s2.clone();
        if parsed.truncate {
            s1.truncate(s2.len());
        } else if s2.len() < s1.len() {
            let Some(&last) = s2.last() else {
                eprintln!("tr: when not truncating set1, string2 must be non-empty");
                return Ok(1);
            };
            s2.resize(s1.len(), last);
        }
        for (&a, &b) in s1.iter().zip(s2.iter()) {
            map[a as usize] = b;
        }
    }
    if parsed.delete {
        for &b in &s1 {
            delete[b as usize] = true;
        }
    }
    if parsed.squeeze {
        let sq = if nsets == 2 { &s2 } else { &s1 };
        for &b in sq {
            squeeze[b as usize] = true;
        }
    }

    ctx.telemetry.set_strategy("256-byte O(1) LUT stream");
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut buf = vec![0u8; DEFAULT_BUF_SIZE];
    let mut obuf = Vec::with_capacity(DEFAULT_BUF_SIZE);
    let mut last: Option<u8> = None;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        ctx.telemetry.add_read(n as u64);
        obuf.clear();
        for &b in &buf[..n] {
            if delete[b as usize] {
                continue;
            }
            let c = map[b as usize];
            if squeeze[c as usize] && last == Some(c) {
                continue;
            }
            obuf.push(c);
            last = Some(c);
        }
        ignore_broken_pipe(out.write_all(&obuf))?;
        ctx.telemetry.add_written(obuf.len() as u64);
    }
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}
