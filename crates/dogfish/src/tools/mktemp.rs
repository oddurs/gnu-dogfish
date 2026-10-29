//! `mktemp`

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;

use anyhow::Result;
use dogfish_core::{DogfishContext, gnu_io_error, stdout};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy, print_help, print_version, usage_error,
    utf8_operands,
};
use crate::common::path::{concat_path, last_component};
use crate::common::quote::quote_locale;

const MKTEMP_HELP: &str = "\
Usage: mktemp [OPTION]... [TEMPLATE]
Create a temporary file or directory, safely, and print its name.
TEMPLATE must contain at least 3 consecutive 'X's in last component.
If TEMPLATE is not specified, use tmp.XXXXXXXXXX, and --tmpdir is implied.
Files are created u+rw, and directories u+rwx, minus umask restrictions.

  -d, --directory     create a directory, not a file
  -u, --dry-run       do not create anything; merely print a name (unsafe)
  -q, --quiet         suppress diagnostics about file/dir-creation failure
      --suffix=SUFF   append SUFF to TEMPLATE; SUFF must not contain a slash.
                        This option is implied if TEMPLATE does not end in X
  -p DIR, --tmpdir[=DIR]  interpret TEMPLATE relative to DIR; if DIR is not
                        specified, use $TMPDIR if set, else /tmp.  With
                        this option, TEMPLATE must not be an absolute name;
                        unlike with -t, TEMPLATE may contain slashes, but
                        mktemp creates only the final component
  -t                  interpret TEMPLATE as a single file name component,
                        relative to a directory: $TMPDIR, if set; else the
                        directory specified via -p; else /tmp [deprecated]
      --help        display this help and exit
      --version     output version information and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum MktempOpt {
    Directory,
    DryRun,
    Quiet,
    TmpDir,
    Deprecated,
    Suffix,
    Help,
    Version,
}

/// GNU's `longopts` and `"dp:qtuV"` (`-p` requires its argument, `--tmpdir`
/// takes an optional one; `-V` is an undocumented `--version`).
const SPECS: &[OptSpec<MktempOpt>] = &[
    OptSpec::both(b'd', "directory", HasArg::No, MktempOpt::Directory),
    OptSpec::both(b'q', "quiet", HasArg::No, MktempOpt::Quiet),
    OptSpec::both(b'u', "dry-run", HasArg::No, MktempOpt::DryRun),
    OptSpec::long("suffix", HasArg::Required, MktempOpt::Suffix),
    OptSpec::long("tmpdir", HasArg::Optional, MktempOpt::TmpDir),
    OptSpec::long("help", HasArg::No, MktempOpt::Help),
    OptSpec::long("version", HasArg::No, MktempOpt::Version),
    OptSpec::short(b'p', HasArg::Required, MktempOpt::TmpDir),
    OptSpec::short(b't', HasArg::No, MktempOpt::Deprecated),
    OptSpec::short(b'V', HasArg::No, MktempOpt::Version),
];

pub fn run_mktemp(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    use MktempOpt::*;
    use std::io::Read;
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    let (mut directory, mut dry_run, mut quiet, mut use_dest_dir, mut deprecated_t) =
        (false, false, false, false, false);
    let (mut dest_dir_arg, mut suffix_opt) = (None, None);
    while let Some(opt) = opts.next() {
        let (o, v) = match opt {
            Ok(o) => o,
            Err(e) => return Ok(getopt_error("mktemp", &e)),
        };
        let v = v.as_deref().map(lossy);
        match o {
            Help => return Ok(print_help(MKTEMP_HELP)),
            Version => return Ok(print_version("mktemp")),
            Directory => directory = true,
            DryRun => dry_run = true,
            Quiet => quiet = true,
            TmpDir => {
                use_dest_dir = true;
                dest_dir_arg = v;
            }
            Deprecated => {
                use_dest_dir = true;
                deprecated_t = true;
            }
            Suffix => suffix_opt = v,
        }
    }
    let fail = |msg: String| {
        if !quiet {
            eprintln!("mktemp: {msg}");
        }
        Ok(1)
    };
    let operands = match utf8_operands("mktemp", opts.operands()) {
        Ok(o) => o,
        Err(code) => return Ok(code),
    };
    let template = match operands.len() {
        0 => {
            use_dest_dir = true;
            "tmp.XXXXXXXXXX".to_string()
        }
        1 => operands[0].clone(),
        _ => return Ok(usage_error("mktemp", "too many templates")),
    };

    // The suffix is --suffix, or whatever follows the last X of the template.
    let (template, suffix) = match &suffix_opt {
        Some(s) => {
            if !template.ends_with('X') {
                let mut shown = template.clone();
                shown.push_str(s);
                return fail(format!(
                    "with --suffix, template {} must end in X",
                    quote_locale(&shown)
                ));
            }
            (template.clone() + s, s.clone())
        }
        None => {
            let cut = template.rfind('X').map_or(template.len(), |i| i + 1);
            (template.clone(), template[cut..].to_string())
        }
    };
    if suffix.contains('/') {
        return fail(format!(
            "invalid suffix {}, contains directory separator",
            quote_locale(&suffix)
        ));
    }
    let stem = &template[..template.len() - suffix.len()];
    let x_count = stem.len() - stem.trim_end_matches('X').len();
    if x_count < 3 {
        return fail(format!(
            "too few X's in template {}",
            quote_locale(&template)
        ));
    }

    let full_template = if use_dest_dir {
        let env_tmpdir = std::env::var("TMPDIR").ok().filter(|t| !t.is_empty());
        let given = dest_dir_arg.filter(|d| !d.is_empty());
        let dir = if deprecated_t {
            if last_component(template.as_bytes()).len() != template.len() {
                return fail(format!(
                    "invalid template, {}, contains directory separator",
                    quote_locale(&template)
                ));
            }
            env_tmpdir.or(given)
        } else {
            if template.starts_with('/') {
                return fail(format!(
                    "invalid template, {}; with --tmpdir, it may not be absolute",
                    quote_locale(&template)
                ));
            }
            given.or(env_tmpdir)
        }
        .unwrap_or_else(|| "/tmp".to_string());
        concat_path(&dir, OsStr::new(&template))
    } else {
        PathBuf::from(&template)
    };
    let full = full_template.as_os_str().as_bytes().to_vec();
    let x_start = full.len() - suffix.len() - x_count;

    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut urandom = match fs::File::open("/dev/urandom") {
        Ok(f) => f,
        Err(e) => return fail(format!("cannot open /dev/urandom: {}", gnu_io_error(&e))),
    };
    let mut random = vec![0u8; x_count];
    let kind = if directory { "directory" } else { "file" };
    // Names are unpredictable and creation exclusive (O_EXCL, never following an
    // existing symlink), with restrictive permissions applied atomically.
    for _ in 0..10_000 {
        urandom.read_exact(&mut random)?;
        let mut name = full.clone();
        for (slot, b) in name[x_start..x_start + x_count].iter_mut().zip(&random) {
            *slot = ALPHABET[usize::from(*b) % ALPHABET.len()];
        }
        let path = PathBuf::from(OsString::from_vec(name));
        let res = if dry_run {
            match fs::symlink_metadata(&path) {
                Ok(_) => Err(io::Error::from_raw_os_error(libc::EEXIST)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        } else if directory {
            fs::DirBuilder::new().mode(0o700).create(&path)
        } else {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map(|_| ())
        };
        match res {
            Ok(()) => {
                ctx.telemetry.add_items(1);
                let mut out = stdout::stdout();
                let mut line = path.as_os_str().as_bytes().to_vec();
                line.push(b'\n');
                let written = out.write_all(&line).and_then(|()| out.flush());
                // As in GNU: if the name cannot be written out, nobody will
                // ever know of the file, so remove it again and fail.
                if let Err(e) = written
                    && !dry_run
                {
                    stdout::mark_reported();
                    if directory {
                        let _ = fs::remove_dir(&path);
                    } else {
                        let _ = fs::remove_file(&path);
                    }
                    if !quiet {
                        eprintln!("mktemp: write error: {}", gnu_io_error(&e));
                    }
                    return Ok(1);
                }
                return Ok(0);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return fail(format!(
                    "failed to create {kind} via template {}: {}",
                    quote_locale(&full_template),
                    gnu_io_error(&e)
                ));
            }
        }
    }
    fail(format!(
        "failed to create {kind} via template {}: File exists",
        quote_locale(&full_template)
    ))
}
