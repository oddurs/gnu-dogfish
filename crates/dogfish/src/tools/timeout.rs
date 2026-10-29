//! timeout — run a command with a time limit (`timeout -k 5 -s INT 1m cmd`)

use std::ffi::OsString;
use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

use anyhow::Result;
use dogfish_core::{DogfishContext, gnu_io_error, gnu_quote};

use crate::common::exit_status::{EXIT_CANCELED, EXIT_CANNOT_INVOKE, EXIT_ENOENT};
use crate::common::getopt::{
    Getopt, HasArg, OptSpec, Order, getopt_error, lossy, print_help, print_version, try_help,
    usage_error,
};
use crate::common::interval::{duration_from_secs, parse_interval};
use crate::common::quote::quote_locale;
use crate::common::signals::parse_signal;
use crate::common::signals::sig_name_into;

const TIMEOUT_HELP: &str = "\
Usage: timeout [OPTION] DURATION COMMAND [ARG]...
  or:  timeout [OPTION]
Start COMMAND, and kill it if still running after DURATION.

Mandatory arguments to long options are mandatory for short options too.
      --preserve-status
                 exit with the same status as COMMAND, even when the
                   command times out
      --foreground
                 when not running timeout directly from a shell prompt,
                   allow COMMAND to read from the TTY and get TTY signals;
                   in this mode, children of COMMAND will not be timed out
  -k, --kill-after=DURATION
                 also send a KILL signal if COMMAND is still running
                   this long after the initial signal was sent
  -s, --signal=SIGNAL
                 specify the signal to be sent on timeout;
                   SIGNAL may be a name like 'HUP' or a number;
                   see 'kill -l' for a list of signals
  -v, --verbose  diagnose to stderr any signal sent upon timeout
      --help        display this help and exit
      --version     output version information and exit

DURATION is a floating point number with an optional suffix:
's' for seconds (the default), 'm' for minutes, 'h' for hours or 'd' for days.
A duration of 0 disables the associated timeout.  Compound durations such
as 500ms or 1m30s are accepted too.

Upon timeout, send the TERM signal to COMMAND, if no other SIGNAL specified.
The TERM signal kills any process that does not block or catch that signal.
It may be necessary to use the KILL signal, since this signal can't be caught.

Exit status:
  124  if COMMAND times out, and --preserve-status is not specified
  125  if the timeout command itself fails
  126  if COMMAND is found but cannot be invoked
  127  if COMMAND cannot be found
  137  if COMMAND (or timeout itself) is sent the KILL (9) signal (128+9)
  -    the exit status of COMMAND otherwise
";

/// GNU's `long_options` and `"+k:s:v"`: options end at DURATION.
const TIMEOUT_OPTS: &[OptSpec<&str>] = &[
    OptSpec::both(b'k', "kill-after", HasArg::Required, "kill-after"),
    OptSpec::both(b's', "signal", HasArg::Required, "signal"),
    OptSpec::both(b'v', "verbose", HasArg::No, "verbose"),
    OptSpec::long("foreground", HasArg::No, "foreground"),
    OptSpec::long("preserve-status", HasArg::No, "preserve-status"),
    OptSpec::long("help", HasArg::No, "help"),
    OptSpec::long("version", HasArg::No, "version"),
];

/// State shared with the async-signal handler, which may only touch atomics
/// and async-signal-safe calls.
static TIMEOUT_PID: AtomicI32 = AtomicI32::new(0);
static TIMEOUT_SIGNAL: AtomicI32 = AtomicI32::new(libc::SIGTERM);
static TIMEOUT_KILL_AFTER_NS: AtomicU64 = AtomicU64::new(0);
static TIMEOUT_TIMED_OUT: AtomicBool = AtomicBool::new(false);
static TIMEOUT_FOREGROUND: AtomicBool = AtomicBool::new(false);
/// Preformatted `timeout: sending signal ` and ` to command 'CMD'\n` for -v.
static TIMEOUT_VERBOSE: std::sync::OnceLock<(Vec<u8>, Vec<u8>)> = std::sync::OnceLock::new();

/// Arms the real-time interval timer to deliver SIGALRM after `ns` nanoseconds.
fn arm_timer(ns: u64) {
    let secs = ns / 1_000_000_000;
    let usecs = (ns % 1_000_000_000).div_ceil(1000);
    let value = libc::itimerval {
        it_interval: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: libc::timeval {
            tv_sec: secs.min(i32::MAX as u64) as libc::time_t,
            tv_usec: usecs as libc::suseconds_t,
        },
    };
    // SAFETY: setitimer only reads `value`; it is a plain system call.
    unsafe {
        libc::setitimer(libc::ITIMER_REAL, &value, std::ptr::null_mut());
    }
}

fn duration_to_ns(secs: f64) -> u64 {
    if secs <= 0.0 {
        0
    } else {
        // Round up so tiny positive durations still time out.
        (secs * 1e9).ceil().min(u64::MAX as f64).max(1.0) as u64
    }
}

/// Signal handler: on SIGALRM send the configured signal (then arm the
/// `--kill-after` timer); forward any other caught signal to the command.
extern "C" fn timeout_on_signal(sig: libc::c_int) {
    let mut sig = sig;
    if sig == libc::SIGALRM {
        TIMEOUT_TIMED_OUT.store(true, Ordering::SeqCst);
        sig = TIMEOUT_SIGNAL.load(Ordering::SeqCst);
    }
    let pid = TIMEOUT_PID.load(Ordering::SeqCst);
    // SAFETY: only async-signal-safe calls (setitimer, write, signal, kill, _exit).
    unsafe {
        if pid <= 0 {
            // Not exec'd yet (we are the forked child): just go.
            libc::_exit(128 + sig);
        }
        let kill_after = TIMEOUT_KILL_AFTER_NS.swap(0, Ordering::SeqCst);
        if kill_after != 0 {
            TIMEOUT_SIGNAL.store(libc::SIGKILL, Ordering::SeqCst);
            arm_timer(kill_after);
        }
        if let Some((head, tail)) = TIMEOUT_VERBOSE.get() {
            let mut buf = [0u8; 16];
            let name = sig_name_into(sig, &mut buf);
            for part in [head.as_slice(), name, tail.as_slice()] {
                libc::write(libc::STDERR_FILENO, part.as_ptr().cast(), part.len());
            }
        }
        libc::kill(pid, sig);
        if !TIMEOUT_FOREGROUND.load(Ordering::SeqCst) {
            // Also signal the whole process group we lead, ignoring it ourselves.
            libc::signal(sig, libc::SIG_IGN);
            libc::kill(0, sig);
            if sig != libc::SIGKILL && sig != libc::SIGCONT {
                libc::kill(pid, libc::SIGCONT);
                libc::signal(libc::SIGCONT, libc::SIG_IGN);
                libc::kill(0, libc::SIGCONT);
            }
        }
    }
}

pub fn run_timeout(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let usage = |msg: String| Ok(usage_error("timeout", msg));
    let mut opts = Getopt::with_order(
        args.into_iter().skip(1).collect(),
        TIMEOUT_OPTS,
        Order::StopAtOperand,
    );
    let mut kill_after = 0f64;
    let mut term_signal = libc::SIGTERM;
    let mut verbose = false;
    let mut foreground = false;
    let mut preserve_status = false;
    while let Some(item) = opts.next() {
        let (key, value) = match item {
            Ok(kv) => kv,
            Err(e) => return Ok(getopt_error("timeout", &e)),
        };
        let value = value.as_deref().map(lossy).unwrap_or_default();
        match key {
            "kill-after" => match parse_interval(&value) {
                Some(secs) => kill_after = secs,
                None => return usage(format!("invalid time interval {}", gnu_quote(&value))),
            },
            "signal" => match parse_signal(&value) {
                Some(sig) => term_signal = sig,
                None => return usage(format!("{}: invalid signal", gnu_quote(&value))),
            },
            "verbose" => verbose = true,
            "foreground" => foreground = true,
            "preserve-status" => preserve_status = true,
            "help" => return Ok(print_help(TIMEOUT_HELP)),
            _ => return Ok(print_version("timeout")),
        }
    }
    let operands = opts.operands();
    if operands.len() < 2 {
        return Ok(try_help("timeout"));
    }
    let Some(limit) = parse_interval(&lossy(&operands[0])) else {
        return usage(format!(
            "invalid time interval {}",
            quote_locale(&operands[0])
        ));
    };
    let command = &operands[1..];
    ctx.telemetry
        .set_strategy(format!("timeout {:?}", duration_from_secs(limit)));

    TIMEOUT_SIGNAL.store(term_signal, Ordering::SeqCst);
    TIMEOUT_KILL_AFTER_NS.store(duration_to_ns(kill_after), Ordering::SeqCst);
    TIMEOUT_FOREGROUND.store(foreground, Ordering::SeqCst);
    if verbose {
        let _ = TIMEOUT_VERBOSE.set((
            b"timeout: sending signal ".to_vec(),
            format!(" to command {}\n", quote_locale(&command[0])).into_bytes(),
        ));
    }
    // SAFETY: process-wide signal setup before any child exists; the handler
    // only uses async-signal-safe operations.
    unsafe {
        if !foreground {
            // Lead a new process group so the signal reaches all descendants.
            libc::setpgid(0, 0);
        }
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = timeout_on_signal as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        for sig in [
            libc::SIGALRM,
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGHUP,
            libc::SIGTERM,
            term_signal,
        ] {
            if sig != 0 && sig != libc::SIGKILL && sig != libc::SIGSTOP {
                libc::sigaction(sig, &action, std::ptr::null_mut());
            }
        }
        // Allow running in the background while the command reads the TTY.
        libc::signal(libc::SIGTTIN, libc::SIG_IGN);
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }

    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..]);
    // SAFETY: the closure runs in the forked child and only calls signal(2).
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGTTIN, libc::SIG_DFL);
            libc::signal(libc::SIGTTOU, libc::SIG_DFL);
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let status = if e.kind() == io::ErrorKind::NotFound {
                EXIT_ENOENT
            } else {
                EXIT_CANNOT_INVOKE
            };
            eprintln!(
                "timeout: failed to run command {}: {}",
                quote_locale(&command[0]),
                gnu_io_error(&e)
            );
            return Ok(status);
        }
    };
    TIMEOUT_PID.store(child.id() as i32, Ordering::SeqCst);
    let limit_ns = duration_to_ns(limit);
    if limit_ns > 0 {
        arm_timer(limit_ns);
    }

    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("timeout: error waiting for command: {}", gnu_io_error(&e));
            return Ok(EXIT_CANCELED);
        }
    };
    let timed_out = TIMEOUT_TIMED_OUT.load(Ordering::SeqCst);
    let mut code = match (status.code(), status.signal()) {
        (Some(code), _) => code,
        (None, Some(sig)) => {
            if status.core_dumped() {
                eprintln!("timeout: the monitored command dumped core");
            }
            // SAFETY: getpid/getpgrp have no preconditions.
            let leader = unsafe { libc::getpid() == libc::getpgrp() };
            if !timed_out && leader {
                // Exit with the signal the command got, without dumping core.
                // SAFETY: resetting our own disposition and re-raising the signal.
                unsafe {
                    let no_core = libc::rlimit {
                        rlim_cur: 0,
                        rlim_max: 0,
                    };
                    libc::setrlimit(libc::RLIMIT_CORE, &no_core);
                    libc::signal(sig, libc::SIG_DFL);
                    let mut set: libc::sigset_t = std::mem::zeroed();
                    libc::sigemptyset(&mut set);
                    libc::sigaddset(&mut set, sig);
                    libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
                    libc::raise(sig);
                }
            }
            if timed_out && sig == libc::SIGKILL {
                preserve_status = true;
            }
            128 + sig
        }
        (None, None) => EXIT_CANCELED,
    };
    if timed_out && !preserve_status {
        code = 124;
    }
    Ok(code)
}
