//! C `long double` in software: glibc's `strtold`, `printf` and the
//! arithmetic `seq` does, bit for bit.
//!
//! GNU `seq` and `printf` (and `sort -g`) compute with `long double`, whose
//! format depends on the platform. [`Float`] is a binary floating-point
//! number in one of three [`Format`]s:
//!
//! - [`X87`], the x86 80-bit extended format (a 64-bit significand with an
//!   explicit integer bit, a 15-bit exponent): `long double` on x86 and
//!   x86-64, except Android.
//! - [`Binary128`], IEEE quadruple precision (113-bit significand, 15-bit
//!   exponent): `long double` on 64-bit Arm (but Apple's), RISC-V,
//!   LoongArch and IBM Z under glibc, and on x86-64 Android.
//! - [`Binary64`], IEEE double: `long double` everywhere else Dogfish runs,
//!   such as Apple silicon and 32-bit Arm. (PowerPC's IBM double-double is
//!   not modelled; Dogfish uses double there.)
//!
//! [`LongDouble`] is the platform's own. Every operation rounds to nearest,
//! ties to even, with gradual underflow, as the hardware does in its default
//! mode; [`parse::strtold`] is glibc's `strtold` and [`format::format_into`]
//! glibc's `printf_fp` and `printf_fphex`.

use std::cmp::Ordering;
use std::fmt;
use std::marker::PhantomData;

mod big;
pub(crate) mod format;
pub(crate) mod parse;

pub(crate) use format::{FloatSpec, format_into};
pub(crate) use parse::strtold;

/// A binary floating-point format. Values are kept with an explicit leading
/// significand bit whatever the encoding does.
pub(crate) trait Format: Copy + fmt::Debug + 'static {
    /// Significand bits, the leading one included.
    const P: u32;
    /// Exponent bits.
    const EXP_BITS: u32;
    /// glibc prints `%a` with the significand's top four bits before the
    /// point (`0x8p-3` for 1), as for the 80-bit format, rather than `0x1p+0`.
    const EXPLICIT_HEX: bool;
    /// glibc's `TININESS_AFTER_ROUNDING` for the platform: whether `strtold`
    /// decides underflow (`ERANGE`) after rounding.
    const TININESS_AFTER_ROUNDING: bool;
    /// The sign of the NaN an invalid operation (`inf - inf`) makes:
    /// negative on x86, whose `printf` shows it as `-nan`.
    const DEFAULT_NAN_NEGATIVE: bool;
    /// Decimal magnitudes (the exponent of `0.ddd × 10^m`) beyond which
    /// `strtold` overflows, or underflows to zero, without computing.
    const MAX_MAG: i64;
    const MIN_MAG: i64;

    const BIAS: i64 = (1 << (Self::EXP_BITS - 1)) - 1;
    /// The biased exponent of infinity and NaN.
    const EXP_INF: i32 = (1 << Self::EXP_BITS) - 1;
    /// The exponent of the least significant bit of the smallest subnormal.
    const MIN_LSB: i64 = 1 - Self::BIAS - (Self::P as i64 - 1);
}

/// The x86 80-bit extended format.
#[derive(Clone, Copy, Debug)]
pub(crate) struct X87;

impl Format for X87 {
    const P: u32 = 64;
    const EXP_BITS: u32 = 15;
    const EXPLICIT_HEX: bool = true;
    const TININESS_AFTER_ROUNDING: bool = true;
    const DEFAULT_NAN_NEGATIVE: bool = true;
    const MAX_MAG: i64 = 4934;
    const MIN_MAG: i64 = -4952;
}

/// IEEE 754 binary128.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Binary128;

impl Format for Binary128 {
    const P: u32 = 113;
    const EXP_BITS: u32 = 15;
    const EXPLICIT_HEX: bool = false;
    const TININESS_AFTER_ROUNDING: bool = cfg!(target_arch = "riscv64");
    const DEFAULT_NAN_NEGATIVE: bool = cfg!(target_arch = "x86_64");
    const MAX_MAG: i64 = 4934;
    const MIN_MAG: i64 = -4967;
}

/// IEEE 754 binary64 (`double`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Binary64;

impl Format for Binary64 {
    const P: u32 = 53;
    const EXP_BITS: u32 = 11;
    const EXPLICIT_HEX: bool = false;
    const TININESS_AFTER_ROUNDING: bool = cfg!(any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "riscv64"
    ));
    const DEFAULT_NAN_NEGATIVE: bool = cfg!(any(target_arch = "x86", target_arch = "x86_64"));
    const MAX_MAG: i64 = 310;
    const MIN_MAG: i64 = -325;
}

/// The platform's `long double` format.
#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    not(target_os = "android")
))]
pub(crate) type Native = X87;
#[cfg(any(
    all(target_arch = "aarch64", not(target_vendor = "apple")),
    target_arch = "riscv64",
    target_arch = "loongarch64",
    target_arch = "s390x",
    all(target_arch = "x86_64", target_os = "android")
))]
pub(crate) type Native = Binary128;
#[cfg(not(any(
    target_arch = "x86",
    target_arch = "x86_64",
    all(target_arch = "aarch64", not(target_vendor = "apple")),
    target_arch = "riscv64",
    target_arch = "loongarch64",
    target_arch = "s390x"
)))]
pub(crate) type Native = Binary64;

/// The platform's `long double`.
pub(crate) type LongDouble = Float<Native>;

/// A floating-point value: zero, subnormal, normal, infinite or NaN.
///
/// `be` is the biased exponent as encoded (0 for zero and subnormals,
/// [`Format::EXP_INF`] for infinities and NaNs), `m` the significand with
/// its leading bit (below `2^(P-1)` for subnormals, 0 for infinity, nonzero
/// for NaN). A finite value is `m × 2^(max(be, 1) - BIAS - (P - 1))`.
#[derive(Clone, Copy)]
pub(crate) struct Float<F: Format> {
    neg: bool,
    be: i32,
    m: u128,
    format: PhantomData<F>,
}

impl<F: Format> fmt::Debug for Float<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = Vec::new();
        let spec = FloatSpec {
            conv: b'a',
            ..FloatSpec::default()
        };
        format_into(&mut s, *self, &spec);
        f.write_str(&String::from_utf8_lossy(&s))
    }
}

/// What rounding a result to the format lost, as IEEE flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Status {
    pub(crate) inexact: bool,
    pub(crate) overflow: bool,
    /// Below the smallest normal number (before or after rounding, as the
    /// format's [`Format::TININESS_AFTER_ROUNDING`] says).
    pub(crate) tiny: bool,
}

impl Status {
    /// Whether glibc's `strtold` sets `errno` to `ERANGE`: on overflow, and
    /// on an inexact tiny result.
    pub(crate) fn erange(self) -> bool {
        self.overflow || (self.tiny && self.inexact)
    }
}

/// Rounds `m` right by `shift` bits (≥ 1) to nearest, ties to even, with a
/// nonzero tail below `m` when `sticky`. Returns the result and whether it
/// is inexact.
fn round_shift(m: u128, shift: i64, sticky: bool) -> (u128, bool) {
    let kept = if shift >= 128 { 0 } else { m >> shift };
    let (half, low) = if shift > 128 {
        (false, m != 0)
    } else {
        let h = shift - 1;
        ((m >> h) & 1 == 1, m & ((1u128 << h) - 1) != 0)
    };
    let low = low || sticky;
    let up = half && (low || kept & 1 == 1);
    (kept + u128::from(up), half || low)
}

impl<F: Format> Float<F> {
    const fn raw(neg: bool, be: i32, m: u128) -> Self {
        Float {
            neg,
            be,
            m,
            format: PhantomData,
        }
    }

    pub(crate) const fn zero(neg: bool) -> Self {
        Self::raw(neg, 0, 0)
    }

    pub(crate) const fn infinity(neg: bool) -> Self {
        Self::raw(neg, F::EXP_INF, 0)
    }

    pub(crate) const fn nan(neg: bool) -> Self {
        Self::raw(neg, F::EXP_INF, 1)
    }

    /// The NaN an invalid operation makes.
    fn default_nan() -> Self {
        Self::nan(F::DEFAULT_NAN_NEGATIVE)
    }

    pub(crate) fn from_u64(n: u64) -> Self {
        Self::round(false, u128::from(n), 0, false).0
    }

    pub(crate) fn is_nan(self) -> bool {
        self.be == F::EXP_INF && self.m != 0
    }

    pub(crate) fn is_infinite(self) -> bool {
        self.be == F::EXP_INF && self.m == 0
    }

    pub(crate) fn is_finite(self) -> bool {
        self.be != F::EXP_INF
    }

    pub(crate) fn is_zero(self) -> bool {
        self.be == 0 && self.m == 0
    }

    /// The sign bit (`signbit`): also set for `-0` and negative NaNs.
    pub(crate) fn is_sign_negative(self) -> bool {
        self.neg
    }

    /// The biased exponent and significand as encoded (see [`Float`]).
    pub(crate) fn encoding(self) -> (i32, u128) {
        (self.be, self.m)
    }

    /// A finite value as `m × 2^e`.
    fn parts(self) -> (u128, i64) {
        (
            self.m,
            i64::from(self.be.max(1)) - F::BIAS - (i64::from(F::P) - 1),
        )
    }

    /// The value closest to `±m × 2^e` (plus a nonzero tail below `m` when
    /// `sticky`), and what rounding lost. With `sticky`, `m` must have at
    /// least two bits more than the format keeps.
    pub(crate) fn round(neg: bool, m: u128, e: i64, sticky: bool) -> (Self, Status) {
        if m == 0 {
            let status = Status {
                inexact: sticky,
                overflow: false,
                tiny: sticky,
            };
            return (Self::zero(neg), status);
        }
        let p = i64::from(F::P);
        let bits = i64::from(128 - m.leading_zeros());
        let top = e.saturating_add(bits - 1);
        let mut lsb = (top - (p - 1)).max(F::MIN_LSB);
        let shift = lsb.saturating_sub(e);
        let (mut mant, inexact) = if shift <= 0 {
            debug_assert!(!sticky, "too few bits to round a sticky tail");
            (m << shift.unsigned_abs(), sticky)
        } else {
            round_shift(m, shift, sticky)
        };
        // Below the smallest normal number, 2^(MIN_LSB + P - 1)?
        let min_normal = F::MIN_LSB + p - 1;
        let mut tiny = top < min_normal;
        if tiny && F::TININESS_AFTER_ROUNDING && top == min_normal - 1 && shift > 1 {
            // Not tiny if rounding to P bits, unbounded, reaches it.
            tiny = round_shift(m, shift - 1, sticky).0 != 1 << p;
        }
        if mant == 1 << p {
            mant = 1 << (p - 1);
            lsb += 1;
        }
        let mut status = Status {
            inexact,
            overflow: false,
            tiny,
        };
        if mant >> (p - 1) == 0 {
            return (Self::raw(neg, 0, mant), status);
        }
        let be = lsb + (p - 1) + F::BIAS;
        if be >= i64::from(F::EXP_INF) {
            status.overflow = true;
            status.inexact = true;
            return (Self::infinity(neg), status);
        }
        (Self::raw(neg, be as i32, mant), status)
    }

    /// `self + other`, rounded.
    pub(crate) fn add(self, other: Self) -> Self {
        if self.is_nan() {
            return self;
        }
        if other.is_nan() {
            return other;
        }
        match (self.is_infinite(), other.is_infinite()) {
            (true, true) if self.neg != other.neg => return Self::default_nan(),
            (true, _) => return self,
            (_, true) => return other,
            _ => {}
        }
        if other.is_zero() {
            return if self.is_zero() {
                Self::zero(self.neg && other.neg)
            } else {
                self
            };
        }
        if self.is_zero() {
            return other;
        }
        let (mut a, mut b) = (self, other);
        let (mut ma, mut ea) = a.parts();
        let (mut mb, mut eb) = b.parts();
        if ea < eb {
            std::mem::swap(&mut a, &mut b);
            std::mem::swap(&mut ma, &mut mb);
            std::mem::swap(&mut ea, &mut eb);
        }
        // Three guard bits keep the sum exact or correctly rounded.
        const GUARD: i64 = 3;
        let d = ea - eb;
        let (x, y, e) = if d <= GUARD {
            (ma << d, mb, eb)
        } else {
            let s = d - GUARD;
            let y = if s >= 128 {
                u128::from(mb != 0)
            } else {
                (mb >> s) | u128::from(mb & ((1u128 << s) - 1) != 0)
            };
            (ma << GUARD, y, ea - GUARD)
        };
        let (neg, m) = if a.neg == b.neg {
            (a.neg, x + y)
        } else if x >= y {
            (a.neg, x - y)
        } else {
            (b.neg, y - x)
        };
        if m == 0 {
            return Self::zero(false);
        }
        Self::round(neg, m, e, false).0
    }

    /// `self × other`, rounded.
    pub(crate) fn mul(self, other: Self) -> Self {
        if self.is_nan() {
            return self;
        }
        if other.is_nan() {
            return other;
        }
        let neg = self.neg != other.neg;
        if self.is_infinite() || other.is_infinite() {
            if self.is_zero() || other.is_zero() {
                return Self::default_nan();
            }
            return Self::infinity(neg);
        }
        if self.is_zero() || other.is_zero() {
            return Self::zero(neg);
        }
        let (ma, ea) = self.parts();
        let (mb, eb) = other.parts();
        let (hi, lo) = widening_mul(ma, mb);
        if hi == 0 {
            return Self::round(neg, lo, ea + eb, false).0;
        }
        // Keep the top 128 bits of the product, the rest as a sticky bit.
        let k = 128 - hi.leading_zeros();
        let m = (hi << (128 - k)) | (lo >> k);
        let sticky = lo & ((1u128 << k) - 1) != 0;
        Self::round(neg, m, ea + eb + i64::from(k), sticky).0
    }

    /// The nearest `double`.
    pub(crate) fn to_f64(self) -> f64 {
        let sign = if self.neg { -1.0 } else { 1.0 };
        if self.is_nan() {
            return f64::NAN.copysign(sign);
        }
        if self.is_infinite() {
            return f64::INFINITY * sign;
        }
        let (m, e) = self.parts();
        let (d, _) = Float::<Binary64>::round(self.neg, m, e, false);
        let (be, dm) = d.encoding();
        let bits = (u64::from(d.neg) << 63) | ((be as u64) << 52) | (dm as u64 & ((1 << 52) - 1));
        f64::from_bits(bits)
    }

    /// The value as an `i64` when it is an integer of at most `P` bits (so
    /// that every integer up to it is exact as well) that fits; `-0` is not.
    pub(crate) fn exact_i64(self) -> Option<i64> {
        if !self.is_finite() || (self.neg && self.is_zero()) {
            return None;
        }
        let (m, e) = self.parts();
        let n = if m == 0 {
            0
        } else if e >= 0 {
            if e >= 64 || i64::from(128 - m.leading_zeros()) + e > 64 {
                return None;
            }
            m << e
        } else if e > -128 && m & ((1u128 << -e) - 1) == 0 {
            m >> -e
        } else {
            return None;
        };
        if n >= 1 << 63 || n > 1 << F::P {
            return None;
        }
        let n = n as i64;
        Some(if self.neg { -n } else { n })
    }
}

/// The 256-bit product of `a` and `b`, as (high, low) halves.
fn widening_mul(a: u128, b: u128) -> (u128, u128) {
    let (a1, a0) = (a >> 64, a & u128::from(u64::MAX));
    let (b1, b0) = (b >> 64, b & u128::from(u64::MAX));
    let ll = a0 * b0;
    let lh = a0 * b1;
    let hl = a1 * b0;
    let hh = a1 * b1;
    let (mid, carry1) = lh.overflowing_add(hl);
    let (lo, carry2) = ll.overflowing_add(mid << 64);
    let hi = hh + (mid >> 64) + (u128::from(carry1) << 64) + u128::from(carry2);
    (hi, lo)
}

impl<F: Format> PartialEq for Float<F> {
    fn eq(&self, other: &Self) -> bool {
        self.partial_cmp(other) == Some(Ordering::Equal)
    }
}

impl<F: Format> PartialOrd for Float<F> {
    /// IEEE comparison: NaNs are unordered, `-0` equals `0`.
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        if self.is_nan() || other.is_nan() {
            return None;
        }
        if self.is_zero() && other.is_zero() {
            return Some(Ordering::Equal);
        }
        if self.neg != other.neg {
            return Some(if self.neg {
                Ordering::Less
            } else {
                Ordering::Greater
            });
        }
        let magnitude = (self.be, self.m).cmp(&(other.be, other.m));
        Some(if self.neg {
            magnitude.reverse()
        } else {
            magnitude
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type X = Float<X87>;

    fn x(s: &str) -> X {
        let r = strtold::<X87>(s.as_bytes());
        assert_eq!(r.len, s.len(), "{s}");
        r.value
    }

    fn q(s: &str) -> Float<Binary128> {
        strtold::<Binary128>(s.as_bytes()).value
    }

    fn d(s: &str) -> Float<Binary64> {
        strtold::<Binary64>(s.as_bytes()).value
    }

    #[test]
    fn addition_rounds_to_nearest_even() {
        // 1e20 + 1 is 1e20: its ulp is 8, and a tie goes to the even one.
        assert_eq!(x("1e20").add(x("1")), x("1e20"));
        assert_eq!(x("1e20").add(x("4")), x("1e20"));
        assert_eq!(x("1e20").add(x("5")), x("100000000000000000008"));
        assert_eq!(x("1e20").add(x("12")), x("100000000000000000016"));
        // 2^64 + 1 ties to even, 2^64 + 3 up.
        assert_eq!(
            x("18446744073709551616").add(x("1")),
            x("18446744073709551616")
        );
        assert_eq!(
            x("18446744073709551616").add(x("3")),
            x("18446744073709551620")
        );
        assert_eq!(
            x("0.1").add(x("0.2")),
            x("0.3000000000000000000162630325872825")
        );
        assert!(x("1").add(x("-1")).encoding() == (0, 0));
        assert!(!x("1").add(x("-1")).is_sign_negative());
        assert!(x("-0").add(x("-0")).is_sign_negative());
        assert_eq!(x("1e-4950").add(x("-1e-4950")), x("0"));
        // Subnormals add exactly.
        assert_eq!(x("0x1p-16445").add(x("0x1p-16445")), x("0x1p-16444"));
        // Overflow and invalid operations.
        assert!(x("1e4932").add(x("1e4932")).is_infinite());
        let nan = x("inf").add(x("-inf"));
        assert!(nan.is_nan() && nan.is_sign_negative());
        assert_eq!(x("-inf").add(x("1")), x("-inf"));
    }

    #[test]
    fn multiplication_rounds_to_nearest_even() {
        assert_eq!(x("3").mul(x("0.5")), x("1.5"));
        assert_eq!(
            x("0.1").mul(x("3")),
            x("0.3000000000000000000108420217248550443")
        );
        assert_eq!(x("1e-4940").mul(x("1e-20")), x("0"));
        assert!(x("1e4000").mul(x("1e4000")).is_infinite());
        assert!(x("0").mul(x("inf")).is_nan());
        assert!(x("-2").mul(x("0")).is_sign_negative());
        // binary128 products have more than 128 bits.
        assert_eq!(
            q("0.1").mul(q("10")),
            q("1.000000000000000000000000000000000055")
        );
        assert_eq!(d("0.1").mul(d("3")), d("0.30000000000000004"));
    }

    #[test]
    fn comparisons() {
        assert!(x("1") < x("1.0000000000000000001"));
        assert!(x("-1") < x("-0.5"));
        assert!(x("-0") == x("0"));
        assert!(x("-inf") < x("-1e4932"));
        assert!(x("nan").partial_cmp(&x("1")).is_none());
        assert!(x("nan") != x("nan"));
    }

    #[test]
    fn conversions() {
        assert_eq!(x("0.1").to_f64(), 0.1);
        assert_eq!(x("1e4000").to_f64(), f64::INFINITY);
        assert_eq!(x("-1e-4000").to_f64().to_bits(), (-0.0f64).to_bits());
        assert!(x("-nan").to_f64().is_nan());
        assert_eq!(x("9223372036854775807").exact_i64(), Some(i64::MAX));
        assert_eq!(x("-9223372036854775807").exact_i64(), Some(-i64::MAX));
        assert_eq!(x("9223372036854775808").exact_i64(), None);
        assert_eq!(x("1e2").exact_i64(), Some(100));
        assert_eq!(x("1.5").exact_i64(), None);
        assert_eq!(x("-0").exact_i64(), None);
        assert_eq!(d("9007199254740992").exact_i64(), Some(1 << 53));
        assert_eq!(d("9007199254740994").exact_i64(), None);
        assert_eq!(X::from_u64(u64::MAX), x("18446744073709551615"));
        assert_eq!(
            Float::<Binary64>::from_u64(u64::MAX),
            d("18446744073709551616")
        );
    }

    #[test]
    fn widening_products() {
        assert_eq!(widening_mul(u128::MAX, u128::MAX), (u128::MAX - 1, 1));
        assert_eq!(widening_mul(1 << 127, 2), (1, 0));
    }
}
