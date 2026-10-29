//! Signal names, as gnulib's sig2str/str2sig spell them (env, timeout).

#[cfg(target_os = "linux")]
pub(crate) const SIGNAL_NAMES: &[(&str, libc::c_int)] = &[
    ("HUP", libc::SIGHUP),
    ("INT", libc::SIGINT),
    ("QUIT", libc::SIGQUIT),
    ("ILL", libc::SIGILL),
    ("TRAP", libc::SIGTRAP),
    ("ABRT", libc::SIGABRT),
    ("BUS", libc::SIGBUS),
    ("FPE", libc::SIGFPE),
    ("KILL", libc::SIGKILL),
    ("USR1", libc::SIGUSR1),
    ("SEGV", libc::SIGSEGV),
    ("USR2", libc::SIGUSR2),
    ("PIPE", libc::SIGPIPE),
    ("ALRM", libc::SIGALRM),
    ("TERM", libc::SIGTERM),
    ("STKFLT", libc::SIGSTKFLT),
    ("CHLD", libc::SIGCHLD),
    ("CONT", libc::SIGCONT),
    ("STOP", libc::SIGSTOP),
    ("TSTP", libc::SIGTSTP),
    ("TTIN", libc::SIGTTIN),
    ("TTOU", libc::SIGTTOU),
    ("URG", libc::SIGURG),
    ("XCPU", libc::SIGXCPU),
    ("XFSZ", libc::SIGXFSZ),
    ("VTALRM", libc::SIGVTALRM),
    ("PROF", libc::SIGPROF),
    ("WINCH", libc::SIGWINCH),
    ("POLL", libc::SIGPOLL),
    ("PWR", libc::SIGPWR),
    ("SYS", libc::SIGSYS),
    // Aliases: accepted as input, never printed.
    ("IOT", libc::SIGABRT),
    ("CLD", libc::SIGCHLD),
    ("IO", libc::SIGIO),
];

#[cfg(not(target_os = "linux"))]
pub(crate) const SIGNAL_NAMES: &[(&str, libc::c_int)] = &[
    ("HUP", libc::SIGHUP),
    ("INT", libc::SIGINT),
    ("QUIT", libc::SIGQUIT),
    ("ILL", libc::SIGILL),
    ("TRAP", libc::SIGTRAP),
    ("ABRT", libc::SIGABRT),
    ("EMT", libc::SIGEMT),
    ("FPE", libc::SIGFPE),
    ("KILL", libc::SIGKILL),
    ("BUS", libc::SIGBUS),
    ("SEGV", libc::SIGSEGV),
    ("SYS", libc::SIGSYS),
    ("PIPE", libc::SIGPIPE),
    ("ALRM", libc::SIGALRM),
    ("TERM", libc::SIGTERM),
    ("URG", libc::SIGURG),
    ("STOP", libc::SIGSTOP),
    ("TSTP", libc::SIGTSTP),
    ("CONT", libc::SIGCONT),
    ("CHLD", libc::SIGCHLD),
    ("TTIN", libc::SIGTTIN),
    ("TTOU", libc::SIGTTOU),
    ("IO", libc::SIGIO),
    ("XCPU", libc::SIGXCPU),
    ("XFSZ", libc::SIGXFSZ),
    ("VTALRM", libc::SIGVTALRM),
    ("PROF", libc::SIGPROF),
    ("WINCH", libc::SIGWINCH),
    ("INFO", libc::SIGINFO),
    ("USR1", libc::SIGUSR1),
    ("USR2", libc::SIGUSR2),
    ("IOT", libc::SIGABRT),
    ("CLD", libc::SIGCHLD),
];

/// Real-time signal range, if the platform has one.
pub(crate) fn rt_signal_range() -> Option<(libc::c_int, libc::c_int)> {
    #[cfg(target_os = "linux")]
    {
        Some((libc::SIGRTMIN(), libc::SIGRTMAX()))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Whether `sig` is a signal number gnulib's `sig2str` can name (0 included).
fn sig_known(sig: libc::c_int) -> bool {
    sig == 0
        || SIGNAL_NAMES.iter().any(|(_, n)| *n == sig)
        || rt_signal_range().is_some_and(|(min, max)| (min..=max).contains(&sig))
}

/// Parses a signal like GNU `operand2sig`: a number (or an exit status that
/// encodes one), or a name in any case with an optional `SIG` prefix.
pub(crate) fn parse_signal(spec: &str) -> Option<libc::c_int> {
    let sig = if spec.starts_with(|c: char| c.is_ascii_digit()) {
        let n: libc::c_int = spec.parse().ok()?;
        // An exit status like 137 that encodes a signal (WIFSIGNALED/WTERMSIG).
        if (n & 0x7f) != 0 && (n & 0x7f) != 0x7f {
            n & 0x7f
        } else {
            n
        }
    } else {
        let upper = spec.to_ascii_uppercase();
        let lookup = |name: &str| -> Option<libc::c_int> {
            if let Some((_, n)) = SIGNAL_NAMES.iter().find(|(s, _)| *s == name) {
                return Some(*n);
            }
            let (min, max) = rt_signal_range()?;
            let (base, rest) = if let Some(r) = name.strip_prefix("RTMIN") {
                (min, r)
            } else {
                (max, name.strip_prefix("RTMAX")?)
            };
            let offset: libc::c_int = if rest.is_empty() {
                0
            } else {
                rest.parse().ok()?
            };
            let sig = base + offset;
            (min..=max).contains(&sig).then_some(sig)
        };
        lookup(&upper).or_else(|| lookup(upper.strip_prefix("SIG")?))?
    };
    sig_known(sig).then_some(sig)
}
