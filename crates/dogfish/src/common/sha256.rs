//! SHA-256 for `sha256sum`.
//!
//! On x86-64 CPUs with AVX2 and BMI2 but no SHA extensions, this runs an
//! assembly loop that computes the message schedule of two blocks at once in
//! AVX2 registers, interleaved with the scalar rounds of the first block,
//! which use BMI2's `rorx`/`andn`; the second block's rounds then read their
//! schedule from memory. That is the method of OpenSSL's
//! `sha256_block_data_order_avx2`, which GNU `sha256sum` reaches through
//! libcrypto. With AVX-512VL the schedule uses its lane rotation and
//! three-way XOR, on the same 256-bit registers. `ring` covers everything
//! else: SHA-NI, AVX, SSSE3 and the other architectures.

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

    /// AVX-512F and VL, for the shorter message schedule (still on 256-bit
    /// registers, which do not lower the clock).
    fn has_avx512vl() -> bool {
        is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512vl")
    }

    /// The streaming state; only built once [`is_fastest`] said yes.
    pub(crate) struct Avx2Sha256 {
        /// [`has_avx512vl`], asked once.
        vl: bool,
        state: [u32; 8],
        buf: [u8; 64],
        buf_len: usize,
        total: u64,
    }

    impl Avx2Sha256 {
        pub(super) fn new() -> Self {
            Self::with_vl(true)
        }

        /// With `vl` false, the AVX2 schedule even where AVX-512VL is there.
        pub(super) fn with_vl(vl: bool) -> Self {
            Self {
                vl: vl && has_avx512vl(),
                state: H0,
                buf: [0; 64],
                buf_len: 0,
                total: 0,
            }
        }

        fn blocks(&mut self, blocks: &[[u8; 64]]) {
            if blocks.is_empty() {
                return;
            }
            let (state, data, n) = (&mut self.state, blocks.as_ptr().cast(), blocks.len());
            // SAFETY: an `Avx2Sha256` is only made once `is_fastest` found
            // AVX2, BMI1 and BMI2, all that `compress_avx2` uses, and `vl` is
            // only set once `has_avx512vl` found AVX-512F and VL as well.
            // `data` points to `n` (at least one) whole blocks, which the
            // code only reads; `state` is the 8 words it updates.
            unsafe {
                if self.vl {
                    compress_avx512vl(state, data, n)
                } else {
                    compress_avx2(state, data, n)
                }
            }
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

    /// The constants the assembly reads, at fixed byte offsets.
    #[repr(C, align(64))]
    struct Consts {
        /// Offset 0: the round constants of each group of four words, twice
        /// (once for each block), as the schedule adds them.
        kk: [[u32; 8]; 16],
        /// Offset 512: a `vpshufb` mask that makes big-endian words.
        bswap: [u32; 8],
        /// Offset 544: a `vpshufb` mask that moves words 0 and 2 of each
        /// 128-bit lane to 0 and 1 and clears 2 and 3...
        low: [u32; 8],
        /// Offset 576: ...and one that moves them to 2 and 3 and clears 0
        /// and 1.
        high: [u32; 8],
    }

    // Read only by the assembly, which Rust cannot see.
    #[allow(dead_code)]
    static CONSTS: Consts = Consts {
        kk: {
            let mut kk = [[0; 8]; 16];
            let mut i = 0;
            while i < 64 {
                kk[i / 4][i % 4] = K[i];
                kk[i / 4][i % 4 + 4] = K[i];
                i += 1;
            }
            kk
        },
        bswap: [
            0x0001_0203,
            0x0405_0607,
            0x0809_0a0b,
            0x0c0d_0e0f,
            0x0001_0203,
            0x0405_0607,
            0x0809_0a0b,
            0x0c0d_0e0f,
        ],
        low: [
            0x0302_0100,
            0x0b0a_0908,
            u32::MAX,
            u32::MAX,
            0x0302_0100,
            0x0b0a_0908,
            u32::MAX,
            u32::MAX,
        ],
        high: [
            u32::MAX,
            u32::MAX,
            0x0302_0100,
            0x0b0a_0908,
            u32::MAX,
            u32::MAX,
            0x0302_0100,
            0x0b0a_0908,
        ],
    };

    // The assembly below, in Intel syntax, keeps the working variables `a`
    // to `h` in eax, ebx, ecx, edx, r8d, r9d, r10d and r11d, renaming them
    // from round to round instead of moving them, and uses five temporaries:
    //
    //   r12d, r13d  Sigma0/Sigma1 parts; r13d carries Sigma0(a) of a round
    //               into the next, which adds it to its `a` (OpenSSL's
    //               deferral, which shortens the round's critical path)
    //   r14d        `f` at the start of a round (`e` copied at the end of
    //               the one before), then Ch's parts
    //   r15d, esi   `b ^ c` coming in and `a ^ b` going out, swapping roles
    //               each round (this round's `a ^ b` is the next's `b ^ c`)
    //
    // rdi is the byte offset of the current rounds in the `W + K` area on the
    // stack (and of their group of constants in `CONSTS.kk`), rbp points to
    // `CONSTS`, and ymm0-ymm3 hold the last 16 schedule words of both blocks
    // (the first block in the low 128-bit lane), ymm4-ymm7 are temporaries
    // and ymm8 is the byte-swap mask.
    //
    // The `W + K` area is 8 chunks of 64 bytes, one for each 8 rounds:
    // words 8c..8c+3 of the first block, the same of the second, then
    // words 8c+4..8c+7 of each. So group j (words 4j..4j+3 of both blocks,
    // one ymm register) is at byte 32j, and the second block's words are 16
    // bytes after the first's.

    /// One instruction of assembly text, from its tokens.
    macro_rules! ins {
        ($($t:tt)*) => {
            concat!(stringify!($($t)*), "\n")
        };
    }

    /// One round. The registers are `[32-bit 64-bit]` pairs in the roles
    /// `a` to `h`, then the temporaries that hold `a ^ b` (going out) and
    /// `b ^ c` (coming in); `$off` is the offset of this round's `W + K`
    /// from rsp + rdi.
    macro_rules! round {
        ([$a:ident $aq:ident] [$b:ident $bq:ident] [$c:ident $cq:ident] [$d:ident $dq:ident]
         [$e:ident $eq:ident] [$f:ident $fq:ident] [$g:ident $gq:ident] [$h:ident $hq:ident],
         [$ab:ident $abq:ident] [$bc:ident $bcq:ident], $off:literal) => {
            concat!(
                // h += W + K; r14d = f & e.
                ins!(add $h, dword ptr [rsp + rdi + $off]),
                ins!(and r14d, $e),
                ins!(rorx r12d, $e, 25),
                ins!(rorx $ab, $e, 11),
                // The previous round's Sigma0.
                ins!(lea $a, [$aq + r13]),
                ins!(lea $h, [$hq + r14]),
                ins!(andn r14d, $e, $g),
                ins!(xor r12d, $ab),
                ins!(rorx r13d, $e, 6),
                // h += Ch(e, f, g) = (e & f) + (!e & g).
                ins!(lea $h, [$hq + r14]),
                // h += Sigma1(e).
                ins!(xor r12d, r13d),
                ins!(mov $ab, $a),
                ins!(rorx r14d, $a, 22),
                ins!(lea $h, [$hq + r12]),
                ins!(xor $ab, $b),
                ins!(rorx r13d, $a, 13),
                ins!(rorx r12d, $a, 2),
                // d += T1.
                ins!(lea $d, [$dq + $hq]),
                // Maj(a, b, c) = ((a ^ b) & (b ^ c)) ^ b.
                ins!(and $bc, $ab),
                ins!(xor r13d, r14d),
                ins!(xor $bc, $b),
                // r13d = Sigma0(a), for the next round.
                ins!(xor r13d, r12d),
                // h = T1 + Maj(a, b, c).
                ins!(lea $h, [$hq + $bcq]),
                ins!(mov r14d, $e),
            )
        };
    }

    /// Four rounds from the roles `$a`..`$h`, reading `W + K` at the four
    /// offsets.
    macro_rules! rounds4 {
        ($a:tt $b:tt $c:tt $d:tt $e:tt $f:tt $g:tt $h:tt,
         $o0:literal, $o1:literal, $o2:literal, $o3:literal) => {
            concat!(
                round!($a $b $c $d $e $f $g $h, [r15d r15] [esi rsi], $o0),
                round!($h $a $b $c $d $e $f $g, [esi rsi] [r15d r15], $o1),
                round!($g $h $a $b $c $d $e $f, [r15d r15] [esi rsi], $o2),
                round!($f $g $h $a $b $c $d $e, [esi rsi] [r15d r15], $o3),
            )
        };
    }

    /// Four rounds with `a` in eax (rounds 8k to 8k + 3)...
    macro_rules! rounds4_a {
        ($o0:literal, $o1:literal, $o2:literal, $o3:literal) => {
            rounds4!(
                [eax rax] [ebx rbx] [ecx rcx] [edx rdx] [r8d r8] [r9d r9] [r10d r10] [r11d r11],
                $o0, $o1, $o2, $o3
            )
        };
    }

    /// ...and with `a` in r8d (rounds 8k + 4 to 8k + 7).
    macro_rules! rounds4_e {
        ($o0:literal, $o1:literal, $o2:literal, $o3:literal) => {
            rounds4!(
                [r8d r8] [r9d r9] [r10d r10] [r11d r11] [eax rax] [ebx rbx] [ecx rcx] [edx rdx],
                $o0, $o1, $o2, $o3
            )
        };
    }

    /// The next group of schedule words of both blocks with AVX2, from the
    /// previous four (`$x0` oldest, which it replaces), as in Intel's and
    /// OpenSSL's AVX code; then the group plus its round constants is stored
    /// at `$off` from rsp + rdi.
    macro_rules! schedule_avx2 {
        ($x0:ident, $x1:ident, $x2:ident, $x3:ident, $off:literal) => {
            concat!(
                // W[t-15..t-12] and W[t-7..t-4].
                ins!(vpalignr ymm4, $x1, $x0, 4),
                ins!(vpalignr ymm5, $x3, $x2, 4),
                ins!(vpaddd $x0, $x0, ymm5),
                // sigma0(W[t-15..t-12]).
                ins!(vpsrld ymm5, ymm4, 7),
                ins!(vpsrld ymm6, ymm4, 3),
                ins!(vpslld ymm7, ymm4, 14),
                ins!(vpxor ymm6, ymm6, ymm5),
                ins!(vpsrld ymm5, ymm5, 11),
                ins!(vpxor ymm6, ymm6, ymm7),
                ins!(vpslld ymm7, ymm7, 11),
                ins!(vpxor ymm6, ymm6, ymm5),
                ins!(vpxor ymm6, ymm6, ymm7),
                ins!(vpaddd $x0, $x0, ymm6),
                // sigma1(W[t-2], W[t-1]) into words 0 and 1: each word
                // doubled into a quadword, whose right shifts are rotations.
                ins!(vpshufd ymm4, $x3, 0xfa),
                ins!(vpsrld ymm5, ymm4, 10),
                ins!(vpsrlq ymm4, ymm4, 17),
                ins!(vpxor ymm5, ymm5, ymm4),
                ins!(vpsrlq ymm4, ymm4, 2),
                ins!(vpxor ymm5, ymm5, ymm4),
                ins!(vpshufb ymm5, ymm5, ymmword ptr [rbp + 544]),
                ins!(vpaddd $x0, $x0, ymm5),
                // sigma1(W[t], W[t+1]), just made, into words 2 and 3.
                ins!(vpshufd ymm4, $x0, 0x50),
                ins!(vpsrld ymm5, ymm4, 10),
                ins!(vpsrlq ymm4, ymm4, 17),
                ins!(vpxor ymm5, ymm5, ymm4),
                ins!(vpsrlq ymm4, ymm4, 2),
                ins!(vpxor ymm5, ymm5, ymm4),
                ins!(vpshufb ymm5, ymm5, ymmword ptr [rbp + 576]),
                ins!(vpaddd $x0, $x0, ymm5),
                ins!(vpaddd ymm4, $x0, ymmword ptr [rbp + rdi + $off]),
                ins!(vmovdqa ymmword ptr [rsp + rdi + $off], ymm4),
            )
        };
    }

    /// [`schedule_avx2`] with AVX-512VL's `vprord` (rotate) and `vpternlogd`
    /// (here a three-way XOR): 22 instructions instead of 31.
    macro_rules! schedule_avx512vl {
        ($x0:ident, $x1:ident, $x2:ident, $x3:ident, $off:literal) => {
            concat!(
                ins!(vpalignr ymm4, $x1, $x0, 4),
                ins!(vpalignr ymm5, $x3, $x2, 4),
                ins!(vpaddd $x0, $x0, ymm5),
                ins!(vprord ymm5, ymm4, 7),
                ins!(vprord ymm6, ymm4, 18),
                ins!(vpsrld ymm4, ymm4, 3),
                ins!(vpternlogd ymm4, ymm5, ymm6, 0x96),
                ins!(vpaddd $x0, $x0, ymm4),
                // sigma1 of words 2 and 3 of x3, shifted down to 0 and 1...
                ins!(vprord ymm4, $x3, 17),
                ins!(vprord ymm5, $x3, 19),
                ins!(vpsrld ymm6, $x3, 10),
                ins!(vpternlogd ymm6, ymm4, ymm5, 0x96),
                ins!(vpsrldq ymm6, ymm6, 8),
                ins!(vpaddd $x0, $x0, ymm6),
                // ...and of the new words 0 and 1, shifted up to 2 and 3.
                ins!(vprord ymm4, $x0, 17),
                ins!(vprord ymm5, $x0, 19),
                ins!(vpsrld ymm6, $x0, 10),
                ins!(vpternlogd ymm6, ymm4, ymm5, 0x96),
                ins!(vpslldq ymm6, ymm6, 8),
                ins!(vpaddd $x0, $x0, ymm6),
                ins!(vpaddd ymm4, $x0, ymmword ptr [rbp + rdi + $off]),
                ins!(vmovdqa ymmword ptr [rsp + rdi + $off], ymm4),
            )
        };
    }

    /// Defines `$name(state, data, n)`, which compresses the `n` (at least
    /// one) 64-byte blocks at `data` into `state`, two at a time, with
    /// `$schedule` for the message schedule.
    ///
    /// Frame (rsp aligned to 64): the `W + K` area at 0..512, then the
    /// caller's rsp at 512, `state` at 520, the next block pair at 528, the
    /// number of blocks left at 536, and where the current loop of rounds
    /// without schedule ends at 544.
    ///
    /// The rounds are loops of 16 and 8, as in OpenSSL, which keeps the code
    /// small enough for the decoded micro-op cache. (The Rust version this
    /// replaced unrolled all 128 rounds into some 3,500 instructions, and
    /// lost to OpenSSL by 7%; the compiler's own loops of the same rounds
    /// spilled and shuffled registers.)
    macro_rules! compress {
        ($name:ident, $schedule:ident) => {
            #[unsafe(naked)]
            unsafe extern "sysv64" fn $name(state: *mut [u32; 8], data: *const u8, n: usize) {
                core::arch::naked_asm!(
                    "push rbx",
                    "push rbp",
                    "push r12",
                    "push r13",
                    "push r14",
                    "push r15",
                    "mov rax, rsp",
                    "sub rsp, 576",
                    "and rsp, -64",
                    "mov qword ptr [rsp + 512], rax",
                    "mov qword ptr [rsp + 520], rdi",
                    "mov qword ptr [rsp + 528], rsi",
                    "mov qword ptr [rsp + 536], rdx",
                    "lea rbp, [rip + {consts}]",
                    "vmovdqa ymm8, ymmword ptr [rbp + 512]",
                    // Each pair of blocks (a lone last block is paired with
                    // itself, and the copy's rounds skipped).
                    "7:",
                    "mov rsi, qword ptr [rsp + 528]",
                    "lea r12, [rsi + 64]",
                    "cmp qword ptr [rsp + 536], 1",
                    "cmove r12, rsi",
                    "vmovdqu xmm0, xmmword ptr [rsi]",
                    "vinserti128 ymm0, ymm0, xmmword ptr [r12], 1",
                    "vpshufb ymm0, ymm0, ymm8",
                    "vmovdqu xmm1, xmmword ptr [rsi + 16]",
                    "vinserti128 ymm1, ymm1, xmmword ptr [r12 + 16], 1",
                    "vpshufb ymm1, ymm1, ymm8",
                    "vmovdqu xmm2, xmmword ptr [rsi + 32]",
                    "vinserti128 ymm2, ymm2, xmmword ptr [r12 + 32], 1",
                    "vpshufb ymm2, ymm2, ymm8",
                    "vmovdqu xmm3, xmmword ptr [rsi + 48]",
                    "vinserti128 ymm3, ymm3, xmmword ptr [r12 + 48], 1",
                    "vpshufb ymm3, ymm3, ymm8",
                    "vpaddd ymm4, ymm0, ymmword ptr [rbp]",
                    "vmovdqa ymmword ptr [rsp], ymm4",
                    "vpaddd ymm4, ymm1, ymmword ptr [rbp + 32]",
                    "vmovdqa ymmword ptr [rsp + 32], ymm4",
                    "vpaddd ymm4, ymm2, ymmword ptr [rbp + 64]",
                    "vmovdqa ymmword ptr [rsp + 64], ymm4",
                    "vpaddd ymm4, ymm3, ymmword ptr [rbp + 96]",
                    "vmovdqa ymmword ptr [rsp + 96], ymm4",
                    "mov r12, qword ptr [rsp + 520]",
                    "mov eax, dword ptr [r12]",
                    "mov ebx, dword ptr [r12 + 4]",
                    "mov ecx, dword ptr [r12 + 8]",
                    "mov edx, dword ptr [r12 + 12]",
                    "mov r8d, dword ptr [r12 + 16]",
                    "mov r9d, dword ptr [r12 + 20]",
                    "mov r10d, dword ptr [r12 + 24]",
                    "mov r11d, dword ptr [r12 + 28]",
                    "mov esi, ebx",
                    "xor esi, ecx",
                    "xor r13d, r13d",
                    "mov r14d, r9d",
                    "xor edi, edi",
                    // Rounds 0-47 of the first block, 16 at a time, with the
                    // schedule of both blocks for 16 rounds later.
                    ".p2align 6",
                    "3:",
                    rounds4_a!(0, 4, 8, 12),
                    $schedule!(ymm0, ymm1, ymm2, ymm3, 128),
                    rounds4_e!(32, 36, 40, 44),
                    $schedule!(ymm1, ymm2, ymm3, ymm0, 160),
                    rounds4_a!(64, 68, 72, 76),
                    $schedule!(ymm2, ymm3, ymm0, ymm1, 192),
                    rounds4_e!(96, 100, 104, 108),
                    $schedule!(ymm3, ymm0, ymm1, ymm2, 224),
                    "add rdi, 128",
                    "cmp rdi, 384",
                    "jb 3b",
                    "mov qword ptr [rsp + 544], 512",
                    // Rounds without schedule, 8 at a time: 48-63 of the
                    // first block (rdi 384 to 512), all of the second's (rdi
                    // 16 to 528, its words being 16 bytes on).
                    ".p2align 6",
                    "4:",
                    rounds4_a!(0, 4, 8, 12),
                    rounds4_e!(32, 36, 40, 44),
                    "add rdi, 64",
                    "cmp rdi, qword ptr [rsp + 544]",
                    "jb 4b",
                    // The last round's Sigma0, then state += the variables.
                    "add eax, r13d",
                    "mov r12, qword ptr [rsp + 520]",
                    "add eax, dword ptr [r12]",
                    "mov dword ptr [r12], eax",
                    "add ebx, dword ptr [r12 + 4]",
                    "mov dword ptr [r12 + 4], ebx",
                    "add ecx, dword ptr [r12 + 8]",
                    "mov dword ptr [r12 + 8], ecx",
                    "add edx, dword ptr [r12 + 12]",
                    "mov dword ptr [r12 + 12], edx",
                    "add r8d, dword ptr [r12 + 16]",
                    "mov dword ptr [r12 + 16], r8d",
                    "add r9d, dword ptr [r12 + 20]",
                    "mov dword ptr [r12 + 20], r9d",
                    "add r10d, dword ptr [r12 + 24]",
                    "mov dword ptr [r12 + 24], r10d",
                    "add r11d, dword ptr [r12 + 28]",
                    "mov dword ptr [r12 + 28], r11d",
                    // After the first block, the second, unless it was alone.
                    "cmp edi, 512",
                    "jne 5f",
                    "cmp qword ptr [rsp + 536], 2",
                    "jb 5f",
                    "mov esi, ebx",
                    "xor esi, ecx",
                    "xor r13d, r13d",
                    "mov r14d, r9d",
                    "mov edi, 16",
                    "mov qword ptr [rsp + 544], 528",
                    "jmp 4b",
                    // The next pair, if any.
                    "5:",
                    "mov rdx, qword ptr [rsp + 536]",
                    "sub rdx, 2",
                    "jbe 6f",
                    "mov qword ptr [rsp + 536], rdx",
                    "add qword ptr [rsp + 528], 128",
                    "jmp 7b",
                    "6:",
                    "vzeroupper",
                    "mov rsp, qword ptr [rsp + 512]",
                    "pop r15",
                    "pop r14",
                    "pop r13",
                    "pop r12",
                    "pop rbp",
                    "pop rbx",
                    "ret",
                    consts = sym CONSTS,
                )
            }
        };
    }

    compress!(compress_avx2, schedule_avx2);
    compress!(compress_avx512vl, schedule_avx512vl);
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

    fn hex(d: [u8; 32]) -> String {
        d.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Bytes from a fixed xorshift generator.
    #[cfg(target_arch = "x86_64")]
    fn noise(n: usize, mut seed: u64) -> Vec<u8> {
        (0..n)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed >> 32) as u8
            })
            .collect()
    }

    #[test]
    fn reference_vectors() {
        // FIPS 180-2 appendix B and the NIST examples.
        let cases: [(&[u8], &str); 4] = [
            (
                b"",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                b"abc",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
            (
                b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu",
                "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1",
            ),
        ];
        for (input, want) in cases {
            let mut h = Sha256::new();
            h.update(input);
            assert_eq!(hex(h.finish()), want);
        }
        let mut h = Sha256::new();
        for _ in 0..1000 {
            h.update(&[b'a'; 1000]);
        }
        assert_eq!(
            hex(h.finish()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
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

    /// Both schedules (where the CPU has them), on every length up to three
    /// blocks past the two-block step, then random lengths and splits.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx2_paths_match_ring() {
        if !(is_x86_feature_detected!("avx2")
            && is_x86_feature_detected!("bmi1")
            && is_x86_feature_detected!("bmi2"))
        {
            return;
        }
        let data = noise(70_000, 0x9e37_79b9_7f4a_7c15);
        for vl in [false, true] {
            for len in (0..=320).chain([4095, 4096, 4097, 65_536, 70_000]) {
                let mut h = avx2::Avx2Sha256::with_vl(vl);
                h.update(&data[..len]);
                assert_eq!(h.finish(), ring_digest(&data[..len]), "vl {vl} len {len}");
            }
            let mut seed = 12345u64;
            let mut next = |m: u64| {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                (seed >> 33) % m
            };
            for _ in 0..200 {
                let len = next(20_000) as usize;
                let mut h = avx2::Avx2Sha256::with_vl(vl);
                let mut at = 0;
                while at < len {
                    let n = (next(700) as usize).min(len - at);
                    h.update(&data[at..at + n]);
                    at += n;
                }
                assert_eq!(h.finish(), ring_digest(&data[..len]), "vl {vl} len {len}");
            }
        }
    }
}
