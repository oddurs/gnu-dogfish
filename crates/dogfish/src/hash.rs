use std::io::{self, BufRead, Write};

use anyhow::{Result, anyhow};
use base64::Engine as _;
use blake2::Blake2b512;
use clap::Parser;
use data_encoding::{BASE32, BASE32HEX, BASE64URL, HEXLOWER};
use digest::Digest;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, emit_objects_or_table, gnu_io_error,
    io::ignore_broken_pipe,
};
use md5::Md5;
use rayon::prelude::*;
use serde::Serialize;
use sha2::Sha256;

#[derive(Parser)]
#[command(about = "Compute and check cryptographic message digests")]
struct HashArgs {
    /// Read checksums from the FILEs and check them
    #[arg(short = 'c', long = "check")]
    check: bool,

    /// Create a BSD-style checksum line
    #[arg(long = "tag")]
    tag: bool,

    /// Don't print OK for each successfully verified file
    #[arg(long = "quiet")]
    quiet: bool,

    /// Emit structured JSON / NDJSON
    #[arg(short = 'J', long = "json")]
    json: bool,

    /// Files to hash (defaults to stdin `-`)
    files: Vec<String>,
}

#[derive(Serialize, Clone)]
struct HashRecord {
    algorithm: String,
    file: String,
    digest: String,
    bytes: u64,
}

pub fn run_b2sum(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    run_generic_hash::<Blake2b512>("BLAKE2b", args, ctx)
}

pub fn run_sha256sum(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    run_generic_hash::<Sha256>("SHA256", args, ctx)
}

pub fn run_md5sum(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    run_generic_hash::<Md5>("MD5", args, ctx)
}

fn run_generic_hash<D>(algo_name: &str, args: Vec<String>, ctx: &DogfishContext) -> Result<i32>
where
    D: Digest + Default + Send,
{
    let parsed = HashArgs::try_parse_from(args)?;
    let sources = InputSource::from_args(&parsed.files);

    if parsed.check {
        return verify_checksums::<D>(algo_name, &sources, parsed.quiet, parsed.json, ctx);
    }

    ctx.telemetry.set_strategy(if sources.len() > 1 {
        format!("rayon parallel {} + mmap", algo_name)
    } else {
        format!("{} + mmap", algo_name)
    });

    // If multiple file sources and none is stdin, hash in parallel using Rayon!
    let can_parallel =
        sources.len() > 1 && sources.iter().all(|s| matches!(s, InputSource::File(_)));

    let results: Vec<Result<HashRecord>> = if can_parallel {
        sources
            .par_iter()
            .map(|src| compute_source_hash::<D>(algo_name, src, ctx))
            .collect()
    } else {
        sources
            .iter()
            .map(|src| compute_source_hash::<D>(algo_name, src, ctx))
            .collect()
    };

    let mut exit_code = 0;
    let mut records = Vec::with_capacity(results.len());
    for res in results {
        match res {
            Ok(rec) => records.push(rec),
            Err(e) => {
                eprintln!("{}: {}", ctx.tool_name, e);
                exit_code = 1;
            }
        }
    }

    if parsed.json {
        emit_objects_or_table(&records, ctx, true, false)?;
        return Ok(exit_code);
    }

    let st = ctx.style();
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    for rec in &records {
        if parsed.tag {
            writeln!(
                out,
                "{} ({}) = {}",
                st.bold_cyan(&rec.algorithm),
                rec.file,
                st.green(&rec.digest)
            )?;
        } else {
            writeln!(out, "{}  {}", st.green(&rec.digest), st.bold(&rec.file))?;
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(exit_code)
}

fn compute_source_hash<D>(
    algo_name: &str,
    src: &InputSource,
    ctx: &DogfishContext,
) -> Result<HashRecord>
where
    D: Digest + Default,
{
    let gnu_err = |e: std::io::Error| anyhow!("{}: {}", src.display_name(), gnu_io_error(&e));
    let mut reader = FastReader::open(src).map_err(gnu_err)?;
    let mut hasher = D::default();
    let mut bytes_len = 0_u64;

    reader
        .with_bytes(|slice| {
            bytes_len = slice.len() as u64;
            hasher.update(slice);
        })
        .map_err(gnu_err)?;

    ctx.telemetry.add_read(bytes_len);
    ctx.telemetry.add_items(1);

    let digest_bytes = hasher.finalize();
    let hex_str = HEXLOWER.encode(&digest_bytes);

    Ok(HashRecord {
        algorithm: algo_name.to_string(),
        file: src.display_name().to_string(),
        digest: hex_str,
        bytes: bytes_len,
    })
}

fn verify_checksums<D>(
    algo_name: &str,
    sources: &[InputSource],
    quiet: bool,
    json: bool,
    ctx: &DogfishContext,
) -> Result<i32>
where
    D: Digest + Default,
{
    let mut failed = 0_usize;
    let mut passed = 0_usize;
    let st = ctx.style();
    let mut json_results = Vec::new();

    for src in sources {
        let reader = FastReader::open(src)?.into_bufread();
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let Some((expected_hex, filename)) = parse_checksum_line(trimmed) else {
                continue;
            };
            let target = InputSource::File(filename.to_string());
            match compute_source_hash::<D>(algo_name, &target, ctx) {
                Ok(actual) if actual.digest.eq_ignore_ascii_case(expected_hex) => {
                    passed += 1;
                    if json {
                        json_results.push(serde_json::json!({
                            "file": filename,
                            "status": "OK",
                            "expected": expected_hex,
                            "actual": actual.digest,
                        }));
                    } else if !quiet {
                        println!("{}: {}", st.bold(filename), st.bold_green("OK"));
                    }
                }
                Ok(actual) => {
                    failed += 1;
                    if json {
                        json_results.push(serde_json::json!({
                            "file": filename,
                            "status": "FAILED",
                            "expected": expected_hex,
                            "actual": actual.digest,
                        }));
                    } else {
                        println!("{}: {}", st.bold(filename), st.bold_red("FAILED"));
                    }
                }
                Err(e) => {
                    failed += 1;
                    eprintln!("{}: {}", ctx.tool_name, e);
                }
            }
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&json_results)?);
    } else if failed > 0 {
        eprintln!(
            "{}: WARNING: {} of {} computed checksum(s) did NOT match",
            ctx.tool_name,
            failed,
            failed + passed
        );
    }
    Ok(if failed == 0 { 0 } else { 1 })
}

fn parse_checksum_line(line: &str) -> Option<(&str, &str)> {
    // BSD tag style: SHA256 (filename) = digest
    if let Some((left, right)) = line.split_once(") = ") {
        if let Some((_, fname)) = left.split_once(" (") {
            return Some((right.trim(), fname.trim()));
        }
    }
    // Standard GNU style: digest  filename or digest *filename
    let mut parts = line.splitn(2, char::is_whitespace);
    let digest = parts.next()?.trim();
    let rest = parts.next()?.trim_start();
    let filename = rest.strip_prefix('*').unwrap_or(rest);
    Some((digest, filename))
}

// ---------------------------------------------------------------------------
// base64
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "base64",
    about = "Base64 encode or decode data to standard output"
)]
struct Base64Args {
    /// Decode data
    #[arg(short = 'd', long = "decode")]
    decode: bool,

    /// Wrap encoded lines after COLS character (default 76; Use 0 to disable line wrapping)
    #[arg(short = 'w', long = "wrap", default_value_t = 76)]
    wrap: usize,

    /// Ignore non-alphabet characters when decoding
    #[arg(short = 'i', long = "ignore-garbage")]
    ignore_garbage: bool,

    /// Input file (default: stdin)
    file: Option<String>,
}

pub fn run_base64(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = Base64Args::try_parse_from(args)?;
    let src = match parsed.file {
        Some(ref f) if f != "-" => InputSource::File(f.clone()),
        _ => InputSource::Stdin,
    };

    let mut reader = FastReader::open(&src)
        .map_err(|e| anyhow!("{}: {}", src.display_name(), gnu_io_error(&e)))?;
    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());

    reader.with_bytes(|bytes| -> Result<()> {
        ctx.telemetry.add_read(bytes.len() as u64);
        if parsed.decode {
            let cleaned: Vec<u8> = if parsed.ignore_garbage {
                bytes
                    .iter()
                    .copied()
                    .filter(|b| b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/' || *b == b'=')
                    .collect()
            } else {
                bytes
                    .iter()
                    .copied()
                    .filter(|b| !b.is_ascii_whitespace())
                    .collect()
            };
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(&cleaned)
                .map_err(|e| anyhow!("base64: invalid input: {}", e))?;
            ignore_broken_pipe(out.write_all(&decoded))?;
            ctx.telemetry.add_written(decoded.len() as u64);
        } else {
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
            write_wrapped(&mut out, encoded.as_bytes(), parsed.wrap)?;
            ctx.telemetry.add_written(encoded.len() as u64);
        }
        Ok(())
    })??;

    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// basenc — Multi-radix encoder/decoder (base64, base64url, base32, base16/hex, base2)
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "basenc",
    about = "Encode or decode data in various bases (base64url, base32, base16/hex, base2)"
)]
struct BasencArgs {
    #[arg(long = "base64")]
    base64: bool,
    #[arg(long = "base64url")]
    base64url: bool,
    #[arg(long = "base32")]
    base32: bool,
    #[arg(long = "base32hex")]
    base32hex: bool,
    #[arg(long = "base16", visible_alias = "hex")]
    base16: bool,
    #[arg(long = "base2msbf", visible_alias = "bin")]
    base2: bool,

    /// Decode data
    #[arg(short = 'd', long = "decode")]
    decode: bool,

    /// Wrap encoded lines after COLS character (default 76; 0 to disable)
    #[arg(short = 'w', long = "wrap", default_value_t = 76)]
    wrap: usize,

    /// Input file
    file: Option<String>,
}

pub fn run_basenc(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = BasencArgs::try_parse_from(args)?;
    let src = match parsed.file {
        Some(ref f) if f != "-" => InputSource::File(f.clone()),
        _ => InputSource::Stdin,
    };

    let mut reader = FastReader::open(&src)
        .map_err(|e| anyhow!("{}: {}", src.display_name(), gnu_io_error(&e)))?;
    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());

    reader.with_bytes(|bytes| -> Result<()> {
        ctx.telemetry.add_read(bytes.len() as u64);
        if parsed.decode {
            let cleaned: Vec<u8> = bytes
                .iter()
                .copied()
                .filter(|b| !b.is_ascii_whitespace())
                .collect();
            let decoded = if parsed.base64url {
                BASE64URL
                    .decode(&cleaned)
                    .map_err(|e| anyhow!("basenc: invalid base64url: {:?}", e))?
            } else if parsed.base32 {
                BASE32
                    .decode(&cleaned)
                    .map_err(|e| anyhow!("basenc: invalid base32: {:?}", e))?
            } else if parsed.base32hex {
                BASE32HEX
                    .decode(&cleaned)
                    .map_err(|e| anyhow!("basenc: invalid base32hex: {:?}", e))?
            } else if parsed.base16 {
                let s = String::from_utf8_lossy(&cleaned).to_ascii_lowercase();
                HEXLOWER
                    .decode(s.as_bytes())
                    .map_err(|e| anyhow!("basenc: invalid hex: {:?}", e))?
            } else if parsed.base2 {
                decode_base2(&cleaned)?
            } else {
                base64::engine::general_purpose::STANDARD
                    .decode(&cleaned)
                    .map_err(|e| anyhow!("basenc: invalid base64: {}", e))?
            };
            ignore_broken_pipe(out.write_all(&decoded))?;
            ctx.telemetry.add_written(decoded.len() as u64);
        } else {
            let encoded = if parsed.base64url {
                BASE64URL.encode(bytes)
            } else if parsed.base32 {
                BASE32.encode(bytes)
            } else if parsed.base32hex {
                BASE32HEX.encode(bytes)
            } else if parsed.base16 {
                HEXLOWER.encode(bytes)
            } else if parsed.base2 {
                bytes
                    .iter()
                    .map(|b| format!("{:08b}", b))
                    .collect::<String>()
            } else {
                base64::engine::general_purpose::STANDARD.encode(bytes)
            };
            write_wrapped(&mut out, encoded.as_bytes(), parsed.wrap)?;
            ctx.telemetry.add_written(encoded.len() as u64);
        }
        Ok(())
    })??;

    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

fn decode_base2(bits: &[u8]) -> Result<Vec<u8>> {
    if bits.len() % 8 != 0 {
        return Err(anyhow!("base2 input length must be a multiple of 8"));
    }
    let mut out = Vec::with_capacity(bits.len() / 8);
    for chunk in bits.chunks_exact(8) {
        let s = std::str::from_utf8(chunk)?;
        let b = u8::from_str_radix(s, 2)?;
        out.push(b);
    }
    Ok(out)
}

fn write_wrapped<W: Write>(out: &mut W, data: &[u8], wrap: usize) -> io::Result<()> {
    if data.is_empty() {
        // GNU base64 emits nothing at all for empty input.
        return Ok(());
    }
    if wrap == 0 {
        ignore_broken_pipe(out.write_all(data))?;
        // GNU: `-w 0` disables wrapping and the trailing newline.
        return Ok(());
    }
    for chunk in data.chunks(wrap) {
        ignore_broken_pipe(out.write_all(chunk))?;
        ignore_broken_pipe(out.write_all(b"\n"))?;
    }
    Ok(())
}
