//! Permission bits, the umask, mode strings and symbolic mode parsing
//! (chmod, cp, mkdir, mv).

// Permission bits as `u32` (`libc::mode_t` is `u16` on macOS).
pub(crate) const S_ISUID: u32 = 0o4000;
pub(crate) const S_ISGID: u32 = 0o2000;
pub(crate) const S_ISVTX: u32 = 0o1000;
const S_IRWXU: u32 = 0o700;
const S_IRWXG: u32 = 0o070;
const S_IRWXO: u32 = 0o007;
pub(crate) const S_IRWXUGO: u32 = 0o777;
pub(crate) const CHMOD_MODE_BITS: u32 = 0o7777;

/// The process umask (reading it requires setting it, so it is restored at once).
pub(crate) fn process_umask() -> u32 {
    let old = unsafe { libc::umask(0) };
    unsafe { libc::umask(old) };
    mode_t_to_u32(old)
}

// `libc::mode_t` is `u32` on Linux and `u16` on macOS: the casts below are
// needed on one platform and redundant on the other.
#[allow(clippy::unnecessary_cast)]
fn mode_t_to_u32(mode: libc::mode_t) -> u32 {
    mode as u32
}

#[allow(clippy::unnecessary_cast)]
pub(crate) fn u32_to_mode_t(mode: u32) -> libc::mode_t {
    (mode & CHMOD_MODE_BITS) as libc::mode_t
}

/// The nine `rwx` characters of `mode`, with `s`/`S`/`t`/`T` for the special bits
/// (`strmode` without the type letter).
pub(crate) fn strmode9(mode: u32) -> String {
    let mut s = String::with_capacity(9);
    for (shift, special, letter) in [(6, S_ISUID, 's'), (3, S_ISGID, 's'), (0, S_ISVTX, 't')] {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(match (bits & 1 != 0, mode & special != 0) {
            (true, true) => letter,
            (false, true) => letter.to_ascii_uppercase(),
            (true, false) => 'x',
            (false, false) => '-',
        });
    }
    s
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModeFlag {
    Ordinary,
    CopyExisting,
    XIfAnyX,
}

/// One clause of a compiled `chmod` mode (a port of gnulib's `modechange`).
#[derive(Clone, Copy)]
pub(crate) struct ModeChange {
    op: u8,
    flag: ModeFlag,
    affected: u32,
    pub(crate) value: u32,
    mentioned: u32,
}

/// Compiles an octal or symbolic mode (`755`, `u+x,g-w`, `=rwX`, `go=u`).
pub(crate) fn mode_compile(spec: &str) -> Option<Vec<ModeChange>> {
    let s = spec.as_bytes();
    let octal_digit = |b: Option<&u8>| b.copied().filter(|b| (b'0'..=b'7').contains(b));
    if octal_digit(s.first()).is_some() {
        let mut octal = 0u32;
        for &b in s {
            octal_digit(Some(&b))?;
            octal = octal * 8 + u32::from(b - b'0');
            if octal > CHMOD_MODE_BITS {
                return None;
            }
        }
        // Fewer than five digits leave a directory's set-id bits alone.
        let mentioned = if s.len() < 5 {
            (octal & (S_ISUID | S_ISGID)) | S_ISVTX | S_IRWXUGO
        } else {
            CHMOD_MODE_BITS
        };
        return Some(vec![ModeChange {
            op: b'=',
            flag: ModeFlag::Ordinary,
            affected: CHMOD_MODE_BITS,
            value: octal,
            mentioned,
        }]);
    }

    let mut changes = Vec::new();
    let mut i = 0;
    loop {
        let mut affected = 0;
        loop {
            match s.get(i) {
                Some(b'u') => affected |= S_ISUID | S_IRWXU,
                Some(b'g') => affected |= S_ISGID | S_IRWXG,
                Some(b'o') => affected |= S_ISVTX | S_IRWXO,
                Some(b'a') => affected |= CHMOD_MODE_BITS,
                Some(b'=' | b'+' | b'-') => break,
                _ => return None,
            }
            i += 1;
        }
        loop {
            let op = s[i];
            i += 1;
            let mut mentioned = 0;
            let mut flag = ModeFlag::CopyExisting;
            let value;
            match s.get(i) {
                Some(b'0'..=b'7') => {
                    let mut octal = 0u32;
                    while let Some(b) = octal_digit(s.get(i)) {
                        octal = octal * 8 + u32::from(b - b'0');
                        if octal > CHMOD_MODE_BITS {
                            return None;
                        }
                        i += 1;
                    }
                    if affected != 0 || s.get(i).is_some_and(|&b| b != b',') {
                        return None;
                    }
                    affected = CHMOD_MODE_BITS;
                    mentioned = CHMOD_MODE_BITS;
                    value = octal;
                    flag = ModeFlag::Ordinary;
                }
                Some(b'u') => {
                    value = S_IRWXU;
                    i += 1;
                }
                Some(b'g') => {
                    value = S_IRWXG;
                    i += 1;
                }
                Some(b'o') => {
                    value = S_IRWXO;
                    i += 1;
                }
                _ => {
                    let mut v = 0;
                    flag = ModeFlag::Ordinary;
                    loop {
                        match s.get(i) {
                            Some(b'r') => v |= 0o444,
                            Some(b'w') => v |= 0o222,
                            Some(b'x') => v |= 0o111,
                            Some(b'X') => flag = ModeFlag::XIfAnyX,
                            Some(b's') => v |= S_ISUID | S_ISGID,
                            Some(b't') => v |= S_ISVTX,
                            _ => break,
                        }
                        i += 1;
                    }
                    value = v;
                }
            }
            changes.push(ModeChange {
                op,
                flag,
                affected,
                value,
                mentioned: if mentioned != 0 {
                    mentioned
                } else if affected != 0 {
                    affected & value
                } else {
                    value
                },
            });
            if !matches!(s.get(i), Some(b'=' | b'+' | b'-')) {
                break;
            }
        }
        match s.get(i) {
            Some(b',') => i += 1,
            None => return Some(changes),
            _ => return None,
        }
    }
}

/// Applies compiled `changes` to `old`; returns the new mode and the bits the
/// changes cared about. Clauses without a `who` part honour `umask`.
pub(crate) fn mode_adjust(old: u32, dir: bool, umask: u32, changes: &[ModeChange]) -> (u32, u32) {
    let mut mode = old & CHMOD_MODE_BITS;
    let mut mode_bits = 0;
    for c in changes {
        let omit = (if dir { S_ISUID | S_ISGID } else { 0 }) & !c.mentioned;
        let mut value = c.value;
        match c.flag {
            ModeFlag::Ordinary => {}
            ModeFlag::CopyExisting => {
                value &= mode;
                let spread = |mask: u32, all: u32| if value & mask != 0 { all } else { 0 };
                value |= spread(0o444, 0o444) | spread(0o222, 0o222) | spread(0o111, 0o111);
            }
            ModeFlag::XIfAnyX => {
                if mode & 0o111 != 0 || dir {
                    value |= 0o111;
                }
            }
        }
        value &= (if c.affected != 0 { c.affected } else { !umask }) & !omit;
        match c.op {
            b'=' => {
                let preserved = (if c.affected != 0 { !c.affected } else { 0 }) | omit;
                mode_bits |= CHMOD_MODE_BITS & !preserved;
                mode = (mode & preserved) | value;
            }
            b'+' => {
                mode_bits |= value;
                mode |= value;
            }
            _ => {
                mode_bits |= value;
                mode &= !value;
            }
        }
    }
    (mode, mode_bits)
}
