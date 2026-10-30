//! Time formatting for ls, du and stat: file times in local time through
//! GNU `nstrftime`, which is [`date_format::format_time`], the formatter
//! `date` uses.

use std::time::SystemTime;

use crate::common::date_format::{self, local_time};

/// Seconds and nanoseconds since the epoch.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub(crate) struct Timespec {
    pub(crate) sec: i64,
    pub(crate) nsec: i64,
}

impl Timespec {
    pub(crate) fn now() -> Self {
        Self::from_system(SystemTime::now())
    }

    pub(crate) fn from_system(t: SystemTime) -> Self {
        match t.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(d) => Self {
                sec: d.as_secs() as i64,
                nsec: i64::from(d.subsec_nanos()),
            },
            Err(e) => {
                let d = e.duration();
                let (mut sec, mut nsec) = (-(d.as_secs() as i64), -i64::from(d.subsec_nanos()));
                if nsec < 0 {
                    sec -= 1;
                    nsec += 1_000_000_000;
                }
                Self { sec, nsec }
            }
        }
    }

    /// The nanoseconds as `nstrftime`'s `%N` takes them.
    fn ns(self) -> u32 {
        u32::try_from(self.nsec).unwrap_or(0)
    }
}

/// The longest time stamp `ls` formats (GNU's `TIME_STAMP_LEN_MAXIMUM`).
pub(crate) const TIME_STAMP_LEN_MAXIMUM: usize = 1000;

fn into_string(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Formats `t` in local time with a GNU `strftime` format into GNU's
/// fixed-size buffer (`ls`, `stat`); `None` when the time can't be converted
/// or the text would be longer than [`TIME_STAMP_LEN_MAXIMUM`] (GNU `ls` then
/// shows the seconds).
pub(crate) fn format_local_time(fmt: &str, t: Timespec) -> Option<String> {
    const MAX: usize = TIME_STAMP_LEN_MAXIMUM;
    let lt = local_time(t.sec)?;
    let text = date_format::format_time_within(fmt.as_bytes(), &lt, t.ns(), false, MAX as i64 + 1);
    (text.len() <= MAX).then(|| into_string(text))
}

/// [`format_local_time`] without a length limit, as GNU's `fprintftime`
/// writes it (`du --time`); `None` only when the time can't be converted.
pub(crate) fn print_local_time(fmt: &str, t: Timespec) -> Option<String> {
    let lt = local_time(t.sec)?;
    Some(into_string(date_format::format_time(
        fmt.as_bytes(),
        &lt,
        t.ns(),
        false,
    )))
}
