//! env — run a program in a modified environment (`-i`, `-u`, `-S`, `-C`)

use std::ffi::{CStr, CString, OsString};
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use anyhow::Result;
use dogfish_core::{
    ColumnAlign, DogfishContext, RecordWriter, Table, gnu_io_error, gnu_quote, stdout,
};

use crate::common::exit_status::{EXIT_CANCELED, EXIT_CANNOT_INVOKE, EXIT_ENOENT};
use crate::common::getopt::{
    Getopt, HasArg, OptSpec, Order, error_bytes, getopt_error, lossy, print_help, print_version,
    try_help, usage_error,
};
use crate::common::json_opt::{check_json_version, json_long, json_short};
use crate::common::quote::{qa, quote_locale};
use crate::common::signals::{parse_signal, rt_signal_range, sig_name_into};

/// GNU `devmsg` for `-v`: a label and a raw argument (bytes as given) on stderr.
fn devmsg(label: &[u8], arg: &[u8]) {
    let _ = io::stderr().write_all(&[label, arg, b"\n"].concat());
}

/// Largest signal number worth probing (GNU's SIGNUM_BOUND).
fn signum_bound() -> libc::c_int {
    rt_signal_range().map_or(31, |(_, max)| max)
}

fn sig_name(sig: libc::c_int) -> String {
    let mut buf = [0u8; 16];
    String::from_utf8_lossy(sig_name_into(sig, &mut buf)).into_owned()
}

const ENV_HELP: &str = "\
Usage: env [OPTION]... [-] [NAME=VALUE]... [COMMAND [ARG]...]
Set each NAME to VALUE in the environment and run COMMAND.

  -i, --ignore-environment  start with an empty environment
  -0, --null           end each output line with NUL, not newline
  -u, --unset=NAME     remove variable from the environment
  -C, --chdir=DIR      change working directory to DIR
  -S, --split-string=S  process and split S into separate arguments;
                        used to pass multiple arguments on shebang lines
      --block-signal[=SIG]    block delivery of SIG signal(s) to COMMAND
      --default-signal[=SIG]  reset handling of SIG signal(s) to the default
      --ignore-signal[=SIG]   set handling of SIG signal(s) to do nothing
      --list-signal-handling  list non default signal handling to stderr
  -v, --debug          print verbose information for each processing step
  -J, --json[=VERSION] print the environment as NDJSON records: name, value
  -t, --table          print the environment as a table
      --help        display this help and exit
      --version     output version information and exit

A mere - implies -i.  If no COMMAND, print the resulting environment.

SIG may be a signal name like 'PIPE', or a signal number like '13'.
Without SIG, all known signals are included.  Multiple signals can be
comma-separated.  An empty SIG argument is a no-op.

Exit status:
  125  if the env command itself fails
  126  if COMMAND is found but cannot be invoked
  127  if COMMAND cannot be found
  -    the exit status of COMMAND otherwise
";

/// A whitespace character in an option cluster: GNU's option string has
/// them to catch `#!/usr/bin/env -i command` shebang lines.
const fn shebang_space(c: u8) -> OptSpec<&'static str> {
    OptSpec::short(c, HasArg::No, "shebang-space")
}

/// GNU's `longopts` and `"+C:iS:u:v0"` plus whitespace, plus the Dogfish
/// `-J, --json` and `-t, --table`. Options end at the first operand.
const ENV_OPTS: &[OptSpec<&str>] = &[
    OptSpec::both(b'i', "ignore-environment", HasArg::No, "ignore-environment"),
    OptSpec::both(b'0', "null", HasArg::No, "null"),
    OptSpec::both(b'u', "unset", HasArg::Required, "unset"),
    OptSpec::both(b'C', "chdir", HasArg::Required, "chdir"),
    OptSpec::long("default-signal", HasArg::Optional, "default-signal"),
    OptSpec::long("ignore-signal", HasArg::Optional, "ignore-signal"),
    OptSpec::long("block-signal", HasArg::Optional, "block-signal"),
    OptSpec::long("list-signal-handling", HasArg::No, "list-signal-handling"),
    OptSpec::both(b'v', "debug", HasArg::No, "debug"),
    OptSpec::both(b'S', "split-string", HasArg::Required, "split-string"),
    OptSpec::long("help", HasArg::No, "help"),
    OptSpec::long("version", HasArg::No, "version"),
    json_short("json"),
    json_long("json"),
    OptSpec::both(b't', "table", HasArg::No, "table").exact(),
    shebang_space(b' '),
    shebang_space(b'\t'),
    shebang_space(b'\n'),
    shebang_space(0x0b),
    shebang_space(0x0c),
    shebang_space(b'\r'),
];

/// Splits an `env -S` string into arguments like GNU: whitespace separates,
/// single and double quotes group, backslash escapes, `${NAME}` expands,
/// `#` starts a comment and `\c` ends the string.
fn split_env_string(s: &str) -> std::result::Result<Vec<String>, String> {
    let b = s.as_bytes();
    let mut args: Vec<Vec<u8>> = Vec::new();
    let mut cur: Option<Vec<u8>> = None;
    let (mut sq, mut dq) = (false, false);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        i += 1;
        match c {
            b'\'' if !dq => {
                sq = !sq;
                cur.get_or_insert_with(Vec::new);
            }
            b'"' if !sq => {
                dq = !dq;
                cur.get_or_insert_with(Vec::new);
            }
            b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r' if !sq && !dq => {
                args.extend(cur.take());
            }
            b'#' if !sq && !dq && cur.is_none() => break,
            b'$' if !sq => {
                let tail = &s[i - 1..];
                let err = || format!("only ${{VARNAME}} expansion is supported, error at: {tail}");
                if b.get(i) != Some(&b'{') {
                    return Err(err());
                }
                let name_len = b[i + 1..]
                    .iter()
                    .enumerate()
                    .take_while(|&(k, &ch)| {
                        ch == b'_' || ch.is_ascii_alphabetic() || (k > 0 && ch.is_ascii_digit())
                    })
                    .count();
                if name_len == 0 || b.get(i + 1 + name_len) != Some(&b'}') {
                    return Err(err());
                }
                let name = &s[i + 1..i + 1 + name_len];
                // An empty expansion does not by itself make an argument.
                let value = std::env::var_os(name).unwrap_or_default();
                if !value.is_empty() {
                    cur.get_or_insert_with(Vec::new)
                        .extend_from_slice(value.as_bytes());
                }
                i += name_len + 2;
            }
            b'\\' if !sq || matches!(b.get(i), Some(b'\\' | b'\'')) => {
                let Some(&e) = b.get(i) else {
                    return Err("invalid backslash at end of string in -S".to_string());
                };
                i += 1;
                let literal = match e {
                    b'"' | b'#' | b'$' | b'\'' | b'\\' => e,
                    b'_' if dq => b' ',
                    b'_' => {
                        args.extend(cur.take());
                        continue;
                    }
                    b'c' if dq => {
                        return Err("'\\c' must not appear in double-quoted -S string".to_string());
                    }
                    b'c' => break,
                    b'f' => 0x0c,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'v' => 0x0b,
                    _ => {
                        let seq = s[i - 1..].chars().next().unwrap_or('?');
                        return Err(format!("invalid sequence '\\{seq}' in -S"));
                    }
                };
                cur.get_or_insert_with(Vec::new).push(literal);
            }
            _ => cur.get_or_insert_with(Vec::new).push(c),
        }
    }
    if sq || dq {
        return Err("no terminating quote in -S string".to_string());
    }
    args.extend(cur);
    Ok(args
        .into_iter()
        .map(|a| String::from_utf8_lossy(&a).into_owned())
        .collect())
}

/// What to do with the signals named by `--block/default/ignore-signal`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SignalAction {
    Block,
    Default,
    Ignore,
}

/// Parses a `--*-signal` list (`PIPE,INT`); `None` means every signal.
fn parse_signal_list(list: Option<&str>) -> std::result::Result<Option<Vec<libc::c_int>>, String> {
    let Some(list) = list else { return Ok(None) };
    let mut sigs = Vec::new();
    for item in list.split(',').filter(|s| !s.is_empty()) {
        match parse_signal(item) {
            Some(sig) if sig != 0 => sigs.push(sig),
            _ => return Err(format!("{}: invalid signal", gnu_quote(item))),
        }
    }
    Ok(Some(sigs))
}

/// Applies the signal options; a signal named explicitly that cannot be
/// changed (e.g. `STOP`) is an error, as in GNU.
fn apply_signal_actions(
    actions: &[(SignalAction, Option<Vec<libc::c_int>>)],
) -> std::result::Result<(), String> {
    let all: Vec<libc::c_int> = (1..=signum_bound())
        .filter(|&s| s != libc::SIGKILL && s != libc::SIGSTOP)
        .collect();
    // SAFETY: plain signal-disposition and mask updates for this process,
    // which is single-threaded here and about to exec or print and exit.
    unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        let mut block = false;
        for (action, sigs) in actions {
            for &sig in sigs.as_deref().unwrap_or(&all) {
                let handler = match action {
                    SignalAction::Block => {
                        libc::sigaddset(&mut mask, sig);
                        block = true;
                        continue;
                    }
                    SignalAction::Default => libc::SIG_DFL,
                    SignalAction::Ignore => libc::SIG_IGN,
                };
                if libc::signal(sig, handler) == libc::SIG_ERR && sigs.is_some() {
                    let err = io::Error::last_os_error();
                    return Err(format!(
                        "failed to set signal action for signal {sig}: {}",
                        gnu_io_error(&err)
                    ));
                }
            }
        }
        if block {
            libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
        }
    }
    Ok(())
}

fn list_signal_handling() {
    let stderr = io::stderr();
    let mut err = stderr.lock();
    // SAFETY: querying (not changing) the mask and dispositions.
    unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigprocmask(libc::SIG_BLOCK, std::ptr::null(), &mut mask);
        for sig in 1..=signum_bound() {
            let mut act: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(sig, std::ptr::null(), &mut act) != 0 {
                continue;
            }
            let ignored = act.sa_sigaction == libc::SIG_IGN;
            let blocked = libc::sigismember(&mask, sig) == 1;
            if !ignored && !blocked {
                continue;
            }
            let state = match (blocked, ignored) {
                (true, true) => "BLOCK,IGNORE",
                (true, false) => "BLOCK",
                _ => "IGNORE",
            };
            let _ = writeln!(err, "{:<10} ({:>2}): {}", sig_name(sig), sig, state);
        }
    }
}

/// The environment GNU `env` builds: `NAME=VALUE` entries in `environ` order,
/// with `putenv` semantics (replace in place, else append).
struct EnvList(Vec<Vec<u8>>);

impl EnvList {
    fn name_of(entry: &[u8]) -> &[u8] {
        let eq = entry.iter().position(|&c| c == b'=').unwrap_or(entry.len());
        &entry[..eq]
    }

    fn unset(&mut self, name: &[u8]) {
        self.0.retain(|e| Self::name_of(e) != name);
    }

    fn put(&mut self, assignment: &[u8]) {
        let name = Self::name_of(assignment);
        match self.0.iter_mut().find(|e| Self::name_of(e) == name) {
            Some(e) => *e = assignment.to_vec(),
            None => self.0.push(assignment.to_vec()),
        }
    }

    fn get(&self, name: &[u8]) -> Option<&[u8]> {
        self.0
            .iter()
            .find(|e| Self::name_of(e) == name)
            .map(|e| &e[name.len() + 1..])
    }
}

/// Replaces this process with `argv[0]`, searching `PATH` (taken from the new
/// environment, defaulting to `/bin:/usr/bin`) the way `execvp` does. Returns
/// only on failure, with the error to report.
fn exec_with_env(argv: &[OsString], env: &EnvList) -> io::Error {
    let to_c = |b: &[u8]| CString::new(b.to_vec()).ok();
    let Some(c_args) = argv
        .iter()
        .map(|a| to_c(a.as_bytes()))
        .collect::<Option<Vec<_>>>()
    else {
        return io::Error::from_raw_os_error(libc::EINVAL);
    };
    let c_env: Vec<CString> = env.0.iter().filter_map(|e| to_c(e)).collect();
    let mut arg_ptrs: Vec<*const libc::c_char> = c_args.iter().map(|a| a.as_ptr()).collect();
    arg_ptrs.push(std::ptr::null());
    let mut env_ptrs: Vec<*const libc::c_char> = c_env.iter().map(|e| e.as_ptr()).collect();
    env_ptrs.push(std::ptr::null());

    let exec = |path: &CStr| -> io::Error {
        // SAFETY: `path`, `arg_ptrs` and `env_ptrs` are valid NUL-terminated
        // strings and null-terminated pointer arrays that outlive the call.
        unsafe { libc::execve(path.as_ptr(), arg_ptrs.as_ptr(), env_ptrs.as_ptr()) };
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOEXEC) {
            // Like execvp: run a script without a #! line through the shell.
            let mut sh_args: Vec<*const libc::c_char> = vec![c"/bin/sh".as_ptr(), path.as_ptr()];
            sh_args.extend_from_slice(&arg_ptrs[1..]);
            // SAFETY: as above.
            unsafe { libc::execve(c"/bin/sh".as_ptr(), sh_args.as_ptr(), env_ptrs.as_ptr()) };
        }
        err
    };

    let cmd = argv[0].as_bytes();
    if cmd.contains(&b'/') {
        return exec(&c_args[0]);
    }
    if cmd.is_empty() {
        return io::Error::from_raw_os_error(libc::ENOENT);
    }
    let path = env.get(b"PATH").unwrap_or(b"/bin:/usr/bin");
    let mut denied = false;
    let mut last = io::Error::from_raw_os_error(libc::ENOENT);
    for dir in path.split(|&c| c == b':') {
        let mut candidate = if dir.is_empty() {
            Vec::new()
        } else {
            let mut d = dir.to_vec();
            d.push(b'/');
            d
        };
        candidate.extend_from_slice(cmd);
        let Some(c_path) = to_c(&candidate) else {
            continue;
        };
        let err = exec(&c_path);
        match err.raw_os_error() {
            Some(libc::EACCES) => denied = true,
            Some(libc::ENOENT | libc::ENOTDIR | libc::ESTALE | libc::ETIMEDOUT | libc::ENODEV) => {}
            _ => return err,
        }
        last = err;
    }
    if denied {
        io::Error::from_raw_os_error(libc::EACCES)
    } else {
        last
    }
}

/// One row of `env --table`.
struct EnvRecord {
    key: String,
    value: String,
}

/// GNU's hint after an error that suggests a shebang line without -S.
const SHEBANG_HINT: &str = "use -[v]S to pass options in shebang lines";

/// C's `isspace` in the C locale (which, unlike Rust's, includes `\v`).
fn is_c_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

pub fn run_env(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let usage = |msg: String| Ok(usage_error("env", msg));
    let mut opts = Getopt::with_order(
        args.into_iter().skip(1).collect(),
        ENV_OPTS,
        Order::StopAtOperand,
    );
    let mut ignore_env = false;
    let mut null = false;
    let mut debug = false;
    let mut json = false;
    let mut table = false;
    let mut list_signals = false;
    // Raw bytes: a name that is not UTF-8 is unset (or a directory entered) as given.
    let mut unset: Vec<OsString> = Vec::new();
    let mut chdir: Option<OsString> = None;
    let mut signal_actions: Vec<(SignalAction, Option<Vec<libc::c_int>>)> = Vec::new();
    while let Some(item) = opts.next() {
        let (key, raw) = match item {
            Ok(o) => o,
            Err(e) => {
                // A space in the first argument means a shebang line like
                // `#!/usr/bin/env -i command`, which needs -S (GNU's hint).
                if let Some(&[c, b'\'']) = e.as_bytes().strip_prefix(b"invalid option -- '")
                    && is_c_space(c)
                {
                    error_bytes("env", e.as_bytes());
                    eprintln!("env: {SHEBANG_HINT}");
                    return Ok(try_help("env"));
                }
                return Ok(getopt_error("env", &e));
            }
        };
        let value = raw.as_deref().map(lossy);
        match key {
            "ignore-environment" => ignore_env = true,
            "null" => null = true,
            "unset" => unset.push(raw.unwrap_or_default()),
            "chdir" => chdir = raw,
            "debug" => debug = true,
            "json" => {
                if let Err(code) = check_json_version("env", raw.as_deref()) {
                    return Ok(code);
                }
                json = true;
            }
            "table" => table = true,
            "list-signal-handling" => list_signals = true,
            "shebang-space" => {
                let spelled = opts.last_spelling();
                eprintln!("env: invalid option -- '{}'", &spelled[1..]);
                eprintln!("env: use -[v]S to pass options in shebang lines");
                return Ok(try_help("env"));
            }
            "help" => return Ok(print_help(ENV_HELP)),
            "version" => return Ok(print_version("env")),
            "split-string" => {
                let s = value.unwrap_or_default();
                match split_env_string(&s) {
                    Ok(items) => {
                        if debug {
                            eprintln!("split -S:  {}", gnu_quote(&s));
                            for (i, item) in items.iter().enumerate() {
                                let lead = if i == 0 { " into:    " } else { "     &    " };
                                eprintln!("{lead}{}", gnu_quote(item));
                            }
                        }
                        opts.splice_front(items.into_iter().map(OsString::from).collect());
                    }
                    Err(msg) => {
                        eprintln!("env: {msg}");
                        return Ok(EXIT_CANCELED);
                    }
                }
            }
            signal_opt => {
                let action = match signal_opt {
                    "block-signal" => SignalAction::Block,
                    "default-signal" => SignalAction::Default,
                    _ => SignalAction::Ignore,
                };
                match parse_signal_list(value.as_deref()) {
                    Ok(sigs) => signal_actions.push((action, sigs)),
                    Err(msg) => return usage(msg),
                }
            }
        }
    }
    let mut rest = opts.operands();
    if rest.first().is_some_and(|a| a == "-") {
        ignore_env = true;
        rest.remove(0);
    }

    let mut env = EnvList(if ignore_env {
        Vec::new()
    } else {
        std::env::vars_os()
            .map(|(k, v)| {
                let mut e = k.into_vec();
                e.push(b'=');
                e.extend_from_slice(v.as_bytes());
                e
            })
            .collect()
    });
    if ignore_env {
        if debug {
            eprintln!("cleaning environ");
        }
    } else {
        for name in &unset {
            if name.is_empty() || name.as_bytes().contains(&b'=') {
                eprintln!("env: cannot unset {}: Invalid argument", quote_locale(name));
                return Ok(EXIT_CANCELED);
            }
            if debug {
                devmsg(b"unset:    ", name.as_bytes());
            }
            env.unset(name.as_bytes());
        }
    }
    let assignments = rest
        .iter()
        .take_while(|a| a.as_bytes().contains(&b'='))
        .count();
    for a in &rest[..assignments] {
        if debug {
            devmsg(b"setenv:   ", a.as_bytes());
        }
        env.put(a.as_bytes());
    }
    let command = &rest[assignments..];

    if let Err(msg) = apply_signal_actions(&signal_actions) {
        eprintln!("env: {msg}");
        return Ok(EXIT_CANCELED);
    }
    if list_signals {
        list_signal_handling();
    }

    if command.is_empty() {
        if chdir.is_some() {
            return usage("must specify command with --chdir (-C)".to_string());
        }
        return print_env(&env, null, json, table, ctx);
    }
    if null {
        return usage("cannot specify --null (-0) with command".to_string());
    }
    if let Some(dir) = &chdir {
        if debug {
            eprintln!("chdir:    {}", qa(dir));
        }
        if let Err(e) = std::env::set_current_dir(dir) {
            eprintln!(
                "env: cannot change directory to {}: {}",
                qa(dir),
                gnu_io_error(&e)
            );
            return Ok(EXIT_CANCELED);
        }
    }
    if debug {
        devmsg(b"executing: ", command[0].as_bytes());
        for (i, a) in command.iter().enumerate() {
            eprintln!("   arg[{i}]= {}", quote_locale(a));
        }
    }
    let _ = stdout::flush();
    let err = exec_with_env(command, &env);
    let status = if err.raw_os_error() == Some(libc::ENOENT) {
        EXIT_ENOENT
    } else {
        EXIT_CANNOT_INVOKE
    };
    eprintln!("env: {}: {}", quote_locale(&command[0]), gnu_io_error(&err));
    if status == EXIT_ENOENT && command[0].as_bytes().iter().copied().any(is_c_space) {
        eprintln!("env: {SHEBANG_HINT}");
    }
    Ok(status)
}

fn print_env(
    env: &EnvList,
    null: bool,
    json: bool,
    table: bool,
    ctx: &DogfishContext,
) -> Result<i32> {
    if json {
        let mut w = RecordWriter::stdout();
        for e in &env.0 {
            let name = EnvList::name_of(e);
            w.record(|r| {
                r.bytes("name", name)
                    .bytes("value", e.get(name.len() + 1..).unwrap_or_default());
            })?;
        }
        w.flush()?;
        return Ok(0);
    }
    if table {
        let records: Vec<EnvRecord> = env
            .0
            .iter()
            .map(|e| {
                let name = EnvList::name_of(e);
                EnvRecord {
                    key: String::from_utf8_lossy(name).into_owned(),
                    value: String::from_utf8_lossy(e.get(name.len() + 1..).unwrap_or_default())
                        .into_owned(),
                }
            })
            .collect();
        {
            let mut tbl = Table::new(vec![
                ("VARIABLE", ColumnAlign::Left),
                ("VALUE", ColumnAlign::Left),
            ]);
            let st = ctx.style();
            for r in &records {
                tbl.add_row(vec![st.bold_cyan(&r.key), r.value.clone()]);
            }
            let mut out = stdout::stdout();
            tbl.render(&mut out, st)?;
        }
        return Ok(0);
    }
    let mut out = io::BufWriter::new(stdout::stdout());
    let term: &[u8] = if null { b"\0" } else { b"\n" };
    for e in &env.0 {
        out.write_all(e)?;
        out.write_all(term)?;
    }
    out.flush()?;
    Ok(0)
}

/// env's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("env", ENV_HELP, ENV_OPTS)
}
