//! tac — Concatenate and print files in reverse (SIMD memrchr_iter)

use std::io::{self, Write};

use anyhow::Result;
use clap::Parser;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, gnu_io_error, io::ignore_broken_pipe,
};
use memchr::memrchr_iter;

#[derive(Parser)]
#[command(
    name = "tac",
    about = "Concatenate and print files in reverse (last line first)"
)]
pub(crate) struct TacArgs {
    pub(crate) files: Vec<String>,
}

pub fn run_tac(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = TacArgs::try_parse_from(args)?;
    let sources = InputSource::from_args(&parsed.files);
    ctx.telemetry
        .set_strategy("mmap + memrchr reverse SIMD line scan");

    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());
    let mut exit_code = 0;

    for src in &sources {
        let res = FastReader::open(src).and_then(|mut reader| {
            reader
                .with_bytes(|bytes| -> io::Result<()> {
                    ctx.telemetry.add_read(bytes.len() as u64);
                    // Records end with '\n'; an unterminated final record is
                    // emitted as-is (GNU tac does not add a separator).
                    let mut end = bytes.len();
                    let search_end = if bytes.last() == Some(&b'\n') {
                        end - 1
                    } else {
                        end
                    };
                    for pos in memrchr_iter(b'\n', &bytes[..search_end]) {
                        out.write_all(&bytes[pos + 1..end])?;
                        end = pos + 1;
                    }
                    out.write_all(&bytes[..end])?;
                    ctx.telemetry.add_written(bytes.len() as u64);
                    Ok(())
                })
                .and_then(|r| r)
        });
        match res {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(exit_code),
            Err(e) => {
                let _ = out.flush();
                eprintln!(
                    "tac: failed to open '{}' for reading: {}",
                    src.display_name(),
                    gnu_io_error(&e)
                );
                exit_code = 1;
            }
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(exit_code)
}
