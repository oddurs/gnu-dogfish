//! A minimal unsigned big integer (little-endian 32-bit limbs) for the
//! conversions the fast paths cannot do exactly.

use std::cmp::Ordering;

#[derive(Clone)]
pub(super) struct Big(Vec<u32>);

impl Big {
    pub(super) fn from_u128(mut v: u128) -> Self {
        let mut limbs = Vec::with_capacity(4);
        while v != 0 {
            limbs.push(v as u32);
            v >>= 32;
        }
        Big(limbs)
    }

    pub(super) fn from_digits(digits: &[u8]) -> Self {
        let mut b = Big(vec![0]);
        for chunk in digits.chunks(9) {
            let v = chunk.iter().fold(0u32, |v, &d| v * 10 + u32::from(d));
            b.mul_add(10u32.pow(chunk.len() as u32), v);
        }
        b
    }

    pub(super) fn mul_add(&mut self, mul: u32, add: u32) {
        let mut carry = u64::from(add);
        for limb in &mut self.0 {
            let v = u64::from(*limb) * u64::from(mul) + carry;
            *limb = v as u32;
            carry = v >> 32;
        }
        if carry != 0 {
            self.0.push(carry as u32);
        }
    }

    pub(super) fn mul_pow10(&mut self, mut k: u64) {
        while k >= 9 {
            self.mul_add(1_000_000_000, 0);
            k -= 9;
        }
        self.mul_add(10u32.pow(k as u32), 0);
    }

    pub(super) fn bits(&self) -> u64 {
        let mut n = self.0.len();
        while n > 0 && self.0[n - 1] == 0 {
            n -= 1;
        }
        if n == 0 {
            return 0;
        }
        (n as u64 - 1) * 32 + u64::from(32 - self.0[n - 1].leading_zeros())
    }

    fn bit(&self, i: u64) -> bool {
        self.0
            .get((i / 32) as usize)
            .is_some_and(|l| (l >> (i % 32)) & 1 == 1)
    }

    pub(super) fn shl(&mut self, n: u64) {
        let limbs = (n / 32) as usize;
        let bits = (n % 32) as u32;
        if bits != 0 {
            let mut carry = 0u32;
            for limb in &mut self.0 {
                let v = *limb;
                *limb = (v << bits) | carry;
                carry = v >> (32 - bits);
            }
            if carry != 0 {
                self.0.push(carry);
            }
        }
        if limbs != 0 {
            self.0.splice(0..0, std::iter::repeat_n(0, limbs));
        }
    }

    fn shr1(&mut self) {
        let mut carry = 0u32;
        for limb in self.0.iter_mut().rev() {
            let v = *limb;
            *limb = (v >> 1) | (carry << 31);
            carry = v & 1;
        }
    }

    fn cmp(&self, other: &Big) -> Ordering {
        let n = self.0.len().max(other.0.len());
        for i in (0..n).rev() {
            let a = self.0.get(i).copied().unwrap_or(0);
            let b = other.0.get(i).copied().unwrap_or(0);
            if a != b {
                return a.cmp(&b);
            }
        }
        Ordering::Equal
    }

    fn sub_assign(&mut self, other: &Big) {
        let mut borrow = 0i64;
        for i in 0..self.0.len() {
            let v = i64::from(self.0[i]) - i64::from(other.0.get(i).copied().unwrap_or(0)) - borrow;
            borrow = i64::from(v < 0);
            self.0[i] = v.rem_euclid(1 << 32) as u32;
        }
    }

    pub(super) fn is_zero(&self) -> bool {
        self.0.iter().all(|&l| l == 0)
    }

    /// `self / d` for a quotient below 2^127, and whether a remainder is left.
    pub(super) fn div_small_quotient(mut self, d: &Big) -> (u128, bool) {
        let t = self.bits().saturating_sub(d.bits()).min(126);
        let mut dd = Big(d.0.clone());
        dd.shl(t);
        let mut q: u128 = 0;
        for i in (0..=t).rev() {
            if self.cmp(&dd) != Ordering::Less {
                self.sub_assign(&dd);
                q |= 1 << i;
            }
            dd.shr1();
        }
        (q, !self.is_zero())
    }

    /// The leading 128 bits as `m × 2^e`, and whether any bit below them is set.
    pub(super) fn top128(&self) -> (u128, i64, bool) {
        let bits = self.bits();
        let drop = bits.saturating_sub(128);
        let mut m: u128 = 0;
        for i in (drop..bits).rev() {
            m = (m << 1) | u128::from(self.bit(i));
        }
        let sticky = (0..drop).any(|i| self.bit(i));
        (m, drop as i64, sticky)
    }

    /// Removes and returns the bits from bit `s` up (at most 32 of them).
    pub(super) fn take_high(&mut self, s: u64) -> u32 {
        let limb = (s / 32) as usize;
        let shift = (s % 32) as u32;
        let get = |i: usize| u64::from(self.0.get(i).copied().unwrap_or(0));
        let v = (get(limb) | (get(limb + 1) << 32)) >> shift;
        if let Some(l) = self.0.get_mut(limb) {
            *l &= ((1u64 << shift) - 1) as u32;
        }
        self.0.truncate(limb + 1);
        v as u32
    }

    /// The decimal digits (values 0 to 9, most significant first; none for
    /// zero), consuming `self`.
    pub(super) fn into_decimal(mut self) -> Vec<u8> {
        let mut chunks = Vec::new();
        while !self.is_zero() {
            let mut rem = 0u64;
            for limb in self.0.iter_mut().rev() {
                let v = (rem << 32) | u64::from(*limb);
                *limb = (v / 1_000_000_000) as u32;
                rem = v % 1_000_000_000;
            }
            while self.0.last() == Some(&0) {
                self.0.pop();
            }
            chunks.push(rem as u32);
        }
        let mut out = Vec::with_capacity(chunks.len() * 9);
        for (i, &c) in chunks.iter().rev().enumerate() {
            let mut d = [0u8; 9];
            let mut v = c;
            for slot in d.iter_mut().rev() {
                *slot = (v % 10) as u8;
                v /= 10;
            }
            let skip = if i == 0 {
                d.iter().take_while(|&&x| x == 0).count()
            } else {
                0
            };
            out.extend_from_slice(&d[skip..]);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_round_trip() {
        let digits: Vec<u8> = b"340282366920938463463374607431768211457"
            .iter()
            .map(|c| c - b'0')
            .collect();
        assert_eq!(Big::from_digits(&digits).into_decimal(), digits);
        assert_eq!(Big::from_u128(0).into_decimal(), Vec::<u8>::new());
        assert_eq!(
            Big::from_u128(1_000_000_000).into_decimal(),
            vec![1, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn high_bits() {
        let mut b = Big::from_u128(0b1011 << 70 | 5);
        assert_eq!(b.take_high(70), 0b1011);
        assert_eq!(b.bits(), 3);
        let mut b = Big::from_u128(7 << 64);
        assert_eq!(b.take_high(64), 7);
        assert!(b.is_zero());
    }
}
