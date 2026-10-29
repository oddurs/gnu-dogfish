//! SHA-256 for `sha256sum`.
//!
//! On x86-64 CPUs with AVX2 and BMI2 but no SHA extensions, this computes the
//! message schedule of two blocks at once in AVX2 registers, interleaved with
//! the scalar rounds of the first block, which use BMI2's `rorx`/`andn`; the
//! second block's rounds then read their schedule from memory (the method of
//! OpenSSL's `sha256_block_data_order_avx2`, which GNU `sha256sum` reaches
//! through libcrypto). `ring` covers everything else: SHA-NI, AVX, SSSE3 and
//! the other architectures.

/// A SHA-256 computation.
pub(crate) enum Sha256 {
    Ring(ring::digest::Context),
    #[cfg(target_arch = "x86_64")]
    Avx2(avx2::Avx2Sha256),
}

impl Sha256 {
    pub(crate) fn new() -> Self {
        #[cfg(target_arch = "x86_64")]
        if avx2::is_fastest() {
            return Self::Avx2(avx2::Avx2Sha256::new());
        }
        Self::Ring(ring::digest::Context::new(&ring::digest::SHA256))
    }

    pub(crate) fn update(&mut self, data: &[u8]) {
        match self {
            Self::Ring(c) => c.update(data),
            #[cfg(target_arch = "x86_64")]
            Self::Avx2(c) => c.update(data),
        }
    }

    pub(crate) fn finish(self) -> [u8; 32] {
        match self {
            Self::Ring(c) => {
                let mut out = [0u8; 32];
                out.copy_from_slice(c.finish().as_ref());
                out
            }
            #[cfg(target_arch = "x86_64")]
            Self::Avx2(c) => c.finish(),
        }
    }
}

/// The AVX2 implementation.
#[cfg(target_arch = "x86_64")]
mod avx2 {
    use std::arch::x86_64::*;

    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    const H0: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    /// AVX2 and BMI2 without the SHA extensions (with them, `ring` uses those).
    pub(super) fn is_fastest() -> bool {
        is_x86_feature_detected!("avx2")
            && is_x86_feature_detected!("bmi1")
            && is_x86_feature_detected!("bmi2")
            && !is_x86_feature_detected!("sha")
    }

    /// The streaming state; only built once [`is_fastest`] said yes.
    pub(crate) struct Avx2Sha256 {
        state: [u32; 8],
        buf: [u8; 64],
        buf_len: usize,
        total: u64,
    }

    impl Avx2Sha256 {
        pub(super) fn new() -> Self {
            Self {
                state: H0,
                buf: [0; 64],
                buf_len: 0,
                total: 0,
            }
        }

        fn blocks(&mut self, blocks: &[[u8; 64]]) {
            // Safety: only constructed when the CPU has AVX2, BMI1 and BMI2.
            unsafe { compress(&mut self.state, blocks) }
        }

        pub(super) fn update(&mut self, mut data: &[u8]) {
            self.total = self.total.wrapping_add(data.len() as u64);
            if self.buf_len > 0 {
                let n = (64 - self.buf_len).min(data.len());
                self.buf[self.buf_len..self.buf_len + n].copy_from_slice(&data[..n]);
                self.buf_len += n;
                data = &data[n..];
                if self.buf_len < 64 {
                    return;
                }
                let block = self.buf;
                self.blocks(&[block]);
                self.buf_len = 0;
            }
            let (blocks, rest) = data.as_chunks::<64>();
            self.blocks(blocks);
            self.buf[..rest.len()].copy_from_slice(rest);
            self.buf_len = rest.len();
        }

        pub(super) fn finish(mut self) -> [u8; 32] {
            let bits = self.total.wrapping_mul(8);
            let mut tail = [0u8; 128];
            tail[..self.buf_len].copy_from_slice(&self.buf[..self.buf_len]);
            tail[self.buf_len] = 0x80;
            let len = if self.buf_len < 56 { 64 } else { 128 };
            tail[len - 8..len].copy_from_slice(&bits.to_be_bytes());
            let (blocks, _) = tail[..len].as_chunks::<64>();
            self.blocks(blocks);
            let mut out = [0u8; 32];
            for (o, s) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
                *o = s.to_be_bytes();
            }
            out
        }
    }

    /// One round; the caller rotates the roles of the eight working variables.
    /// `$bc` holds `b ^ c`: this round's `a ^ b` is the next round's `b ^ c`,
    /// which saves an operation in `Maj`.
    macro_rules! round {
        ($a:ident, $b:ident, $c:ident, $d:ident, $e:ident, $f:ident, $g:ident, $h:ident, $bc:ident, $wk:expr) => {
            let s1 = $e.rotate_right(6) ^ $e.rotate_right(11) ^ $e.rotate_right(25);
            let ch = ($e & $f) ^ (!$e & $g);
            let t1 = $h.wrapping_add(s1).wrapping_add(ch).wrapping_add($wk);
            let s0 = $a.rotate_right(2) ^ $a.rotate_right(13) ^ $a.rotate_right(22);
            let ab = $a ^ $b;
            let maj = (ab & $bc) ^ $b;
            $bc = ab;
            $d = $d.wrapping_add(t1);
            $h = t1.wrapping_add(s0).wrapping_add(maj);
        };
    }

    /// Where word `t` of the first block sits in the interleaved `W + K` array
    /// (the second block's is 4 further on).
    const fn wk_index(t: usize) -> usize {
        8 * (t / 4) + t % 4
    }

    /// `W + K` of word `$t` (at offset `$lane`), read from memory: a volatile
    /// read keeps the compiler from extracting it from the vector register that
    /// computed it (two ALU micro-ops a round) instead of loading it (a load port).
    macro_rules! wk {
        ($wk:ident, $t:expr, $lane:expr) => {
            // Safety: the index is below 128, the array's length.
            $wk.as_ptr().add(wk_index($t) + $lane).read_volatile()
        };
    }

    /// Eight rounds from word `$base`, reading `W + K` from `$wk` at offset `$lane`;
    /// `$bc` carries `b ^ c` from one round to the next.
    macro_rules! rounds8 {
        ($v:ident, $bc:ident, $wk:ident, $lane:expr, $base:expr) => {
            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = $v;
            round!(a, b, c, d, e, f, g, h, $bc, wk!($wk, $base, $lane));
            round!(h, a, b, c, d, e, f, g, $bc, wk!($wk, $base + 1, $lane));
            round!(g, h, a, b, c, d, e, f, $bc, wk!($wk, $base + 2, $lane));
            round!(f, g, h, a, b, c, d, e, $bc, wk!($wk, $base + 3, $lane));
            round!(e, f, g, h, a, b, c, d, $bc, wk!($wk, $base + 4, $lane));
            round!(d, e, f, g, h, a, b, c, $bc, wk!($wk, $base + 5, $lane));
            round!(c, d, e, f, g, h, a, b, $bc, wk!($wk, $base + 6, $lane));
            round!(b, c, d, e, f, g, h, a, $bc, wk!($wk, $base + 7, $lane));
            $v = [a, b, c, d, e, f, g, h];
        };
    }

    /// Rotates each 32-bit lane right by `N` (`M` = 32 - `N`).
    #[inline(always)]
    unsafe fn ror<const N: i32, const M: i32>(x: __m256i) -> __m256i {
        unsafe { _mm256_or_si256(_mm256_srli_epi32::<N>(x), _mm256_slli_epi32::<M>(x)) }
    }

    /// The next four schedule words of both blocks from the previous sixteen
    /// (`x0` oldest), as in Intel's SSE/AVX SHA-256 code.
    #[inline(always)]
    unsafe fn schedule(x0: __m256i, x1: __m256i, x2: __m256i, x3: __m256i) -> __m256i {
        unsafe {
            // W[t-15..t-12] and W[t-7..t-4].
            let w15 = _mm256_alignr_epi8::<4>(x1, x0);
            let w7 = _mm256_alignr_epi8::<4>(x3, x2);
            let s0 = _mm256_xor_si256(
                _mm256_xor_si256(ror::<7, 25>(w15), ror::<18, 14>(w15)),
                _mm256_srli_epi32::<3>(w15),
            );
            let mut t = _mm256_add_epi32(_mm256_add_epi32(x0, s0), w7);
            let sigma1 = |v: __m256i| {
                _mm256_xor_si256(
                    _mm256_xor_si256(ror::<17, 15>(v), ror::<19, 13>(v)),
                    _mm256_srli_epi32::<10>(v),
                )
            };
            // W[t], W[t+1] need sigma1 of W[t-2], W[t-1] (lanes 2, 3 of x3)...
            let lo = sigma1(_mm256_shuffle_epi32::<0b11_10_11_10>(x3));
            t = _mm256_add_epi32(
                t,
                _mm256_blend_epi32::<0b1100_1100>(lo, _mm256_setzero_si256()),
            );
            // ...and W[t+2], W[t+3] need sigma1 of the W[t], W[t+1] just made.
            let hi = sigma1(_mm256_shuffle_epi32::<0b01_00_01_00>(t));
            _mm256_add_epi32(
                t,
                _mm256_blend_epi32::<0b1100_1100>(_mm256_setzero_si256(), hi),
            )
        }
    }

    /// Compresses `blocks` into `state`, two at a time.
    ///
    /// # Safety
    /// The CPU must support AVX2, BMI1 and BMI2.
    #[target_feature(enable = "avx2,bmi1,bmi2")]
    // The last round of a block leaves `b ^ c` for a next round there is not.
    #[allow(unused_assignments)]
    unsafe fn compress(state: &mut [u32; 8], blocks: &[[u8; 64]]) {
        // Byte order: each 32-bit word big-endian.
        let bswap = _mm256_setr_epi8(
            3, 2, 1, 0, 7, 6, 5, 4, 11, 10, 9, 8, 15, 14, 13, 12, 3, 2, 1, 0, 7, 6, 5, 4, 11, 10,
            9, 8, 15, 14, 13, 12,
        );
        // The four round constants of word group j, for both blocks.
        let k = |j: usize| -> __m256i {
            // Safety: K has 64 words and j < 16.
            unsafe {
                let q = _mm_loadu_si128(K.as_ptr().add(4 * j).cast());
                _mm256_set_m128i(q, q)
            }
        };
        let mut i = 0;
        while i < blocks.len() {
            let a = &blocks[i];
            // A lone last block is paired with itself; its copy is ignored.
            let b = blocks.get(i + 1).unwrap_or(a);
            // W + K of words 4j..4j+3: block a at 8j..8j+3, block b at 8j+4..8j+7.
            let mut wk = [0u32; 128];
            // Safety: AVX2 is enabled here; the loads stay within the 64-byte
            // blocks, the stores within `wk` (j < 16).
            unsafe {
                let load = |j: usize| {
                    let v = _mm256_set_m128i(
                        _mm_loadu_si128(b.as_ptr().add(16 * j).cast()),
                        _mm_loadu_si128(a.as_ptr().add(16 * j).cast()),
                    );
                    _mm256_shuffle_epi8(v, bswap)
                };
                let (mut x0, mut x1, mut x2, mut x3) = (load(0), load(1), load(2), load(3));
                macro_rules! store {
                    ($j:expr, $x:expr) => {
                        _mm256_storeu_si256(
                            wk.as_mut_ptr().add(8 * $j).cast(),
                            _mm256_add_epi32($x, k($j)),
                        );
                    };
                }
                store!(0, x0);
                store!(1, x1);
                store!(2, x2);
                store!(3, x3);
                let mut v = *state;
                let mut bc = v[1] ^ v[2];
                // Block a's rounds, with the schedule of both blocks computed
                // alongside, a group of eight rounds ahead of where it is needed.
                macro_rules! step {
                    ($x0:ident, $x1:ident, $x2:ident, $x3:ident, $j:expr) => {
                        $x0 = schedule($x0, $x1, $x2, $x3);
                        store!($j, $x0);
                    };
                }
                macro_rules! group {
                    ($base:expr) => {
                        rounds8!(v, bc, wk, 0, $base);
                    };
                }
                group!(0);
                step!(x0, x1, x2, x3, 4);
                step!(x1, x2, x3, x0, 5);
                group!(8);
                step!(x2, x3, x0, x1, 6);
                step!(x3, x0, x1, x2, 7);
                group!(16);
                step!(x0, x1, x2, x3, 8);
                step!(x1, x2, x3, x0, 9);
                group!(24);
                step!(x2, x3, x0, x1, 10);
                step!(x3, x0, x1, x2, 11);
                group!(32);
                step!(x0, x1, x2, x3, 12);
                step!(x1, x2, x3, x0, 13);
                group!(40);
                step!(x2, x3, x0, x1, 14);
                step!(x3, x0, x1, x2, 15);
                group!(48);
                group!(56);
                for (s, w) in state.iter_mut().zip(v) {
                    *s = s.wrapping_add(w);
                }
            }
            if i + 1 < blocks.len() {
                // Block b: its schedule is all in `wk` already.
                let mut v = *state;
                let mut bc = v[1] ^ v[2];
                // Safety: the `wk` reads stay below its length (see `wk!`).
                unsafe {
                    rounds8!(v, bc, wk, 4, 0);
                    rounds8!(v, bc, wk, 4, 8);
                    rounds8!(v, bc, wk, 4, 16);
                    rounds8!(v, bc, wk, 4, 24);
                    rounds8!(v, bc, wk, 4, 32);
                    rounds8!(v, bc, wk, 4, 40);
                    rounds8!(v, bc, wk, 4, 48);
                    rounds8!(v, bc, wk, 4, 56);
                }
                for (s, w) in state.iter_mut().zip(v) {
                    *s = s.wrapping_add(w);
                }
            }
            i += 2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring_digest(data: &[u8]) -> [u8; 32] {
        let mut c = ring::digest::Context::new(&ring::digest::SHA256);
        c.update(data);
        let mut out = [0u8; 32];
        out.copy_from_slice(c.finish().as_ref());
        out
    }

    #[test]
    fn matches_ring_on_every_length_and_split() {
        let data: Vec<u8> = (0..3000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        for len in (0..300).chain([511, 512, 513, 1000, 2999, 3000]) {
            let want = ring_digest(&data[..len]);
            let mut whole = Sha256::new();
            whole.update(&data[..len]);
            assert_eq!(whole.finish(), want, "len {len}");
            for split in [1, 7, 63, 64, 65, 129] {
                let mut parts = Sha256::new();
                for chunk in data[..len].chunks(split) {
                    parts.update(chunk);
                }
                assert_eq!(parts.finish(), want, "len {len} split {split}");
            }
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx2_path_matches_ring() {
        if !(is_x86_feature_detected!("avx2")
            && is_x86_feature_detected!("bmi1")
            && is_x86_feature_detected!("bmi2"))
        {
            return;
        }
        let data: Vec<u8> = (0..5000u32).map(|i| (i * 7 + i / 3) as u8).collect();
        for len in (0..260).chain([4095, 4096, 4097, 5000]) {
            let mut h = avx2::Avx2Sha256::new();
            h.update(&data[..len]);
            assert_eq!(h.finish(), ring_digest(&data[..len]), "len {len}");
        }
    }
}
