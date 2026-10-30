//! MD5 for `md5sum`.
//!
//! On x86 and x86-64 the compression function is `md5-asm`'s assembly, which
//! md-5 0.10 used behind its `asm` feature; md-5 0.11 dropped that feature,
//! and its portable Rust ran md5sum ~17% slower than GNU. The padding and
//! length encoding (RFC 1321, section 3) are here. Elsewhere, md-5 0.11.

/// An MD5 computation.
#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
pub(crate) struct Md5 {
    state: [u32; 4],
    /// Bytes not yet compressed; fewer than 64 between calls.
    buf: [u8; 64],
    buf_len: usize,
    /// The message length in bits, modulo 2^64 as RFC 1321 specifies.
    bits: u64,
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
impl Md5 {
    pub(crate) fn new() -> Self {
        Self {
            state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476],
            buf: [0; 64],
            buf_len: 0,
            bits: 0,
        }
    }

    pub(crate) fn update(&mut self, mut data: &[u8]) {
        self.bits = self.bits.wrapping_add((data.len() as u64).wrapping_mul(8));
        if self.buf_len > 0 {
            let n = data.len().min(64 - self.buf_len);
            self.buf[self.buf_len..self.buf_len + n].copy_from_slice(&data[..n]);
            self.buf_len += n;
            data = &data[n..];
            if self.buf_len < 64 {
                return;
            }
            md5_asm::compress(&mut self.state, std::slice::from_ref(&self.buf));
            self.buf_len = 0;
        }
        let (blocks, rest) = data.as_chunks::<64>();
        md5_asm::compress(&mut self.state, blocks);
        self.buf[..rest.len()].copy_from_slice(rest);
        self.buf_len = rest.len();
    }

    pub(crate) fn finish(mut self) -> [u8; 16] {
        // 0x80, zeros to 56 bytes mod 64, then the bit length little-endian.
        let bits = self.bits;
        self.buf[self.buf_len] = 0x80;
        self.buf[self.buf_len + 1..].fill(0);
        if self.buf_len >= 56 {
            md5_asm::compress(&mut self.state, std::slice::from_ref(&self.buf));
            self.buf.fill(0);
        }
        self.buf[56..].copy_from_slice(&bits.to_le_bytes());
        md5_asm::compress(&mut self.state, std::slice::from_ref(&self.buf));
        let mut out = [0u8; 16];
        for (o, w) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            *o = w.to_le_bytes();
        }
        out
    }
}

/// An MD5 computation (md-5 0.11: no assembly for this architecture).
#[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
pub(crate) struct Md5(md5::Md5);

#[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
impl Md5 {
    pub(crate) fn new() -> Self {
        Self(digest::Digest::new())
    }

    pub(crate) fn update(&mut self, data: &[u8]) {
        digest::Digest::update(&mut self.0, data);
    }

    pub(crate) fn finish(self) -> [u8; 16] {
        digest::Digest::finalize(self.0).into()
    }
}

#[cfg(test)]
mod tests {
    use super::Md5;
    use digest::Digest;

    fn reference(data: &[u8]) -> [u8; 16] {
        md5::Md5::digest(data).into()
    }

    fn ours(data: &[u8]) -> [u8; 16] {
        let mut h = Md5::new();
        h.update(data);
        h.finish()
    }

    fn input(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 + i / 251) as u8).collect()
    }

    #[test]
    fn matches_md5_crate_for_every_length_to_200() {
        let data = input(200);
        for n in 0..=200 {
            assert_eq!(ours(&data[..n]), reference(&data[..n]), "length {n}");
        }
    }

    #[test]
    fn matches_md5_crate_at_padding_boundaries() {
        for n in [55, 56, 63, 64, 65, 119, 120, 127, 128, 129] {
            let data = input(n);
            assert_eq!(ours(&data), reference(&data), "length {n}");
        }
    }

    #[test]
    fn matches_md5_crate_for_one_mib() {
        let data = input(1 << 20);
        assert_eq!(ours(&data), reference(&data));
    }

    #[test]
    fn streaming_in_odd_chunks_matches_one_update() {
        let data = input((1 << 20) + 77);
        let want = reference(&data);
        for chunk in [1, 3, 7, 55, 63, 65, 127, 4099] {
            let mut h = Md5::new();
            for c in data.chunks(chunk) {
                h.update(c);
            }
            assert_eq!(h.finish(), want, "chunks of {chunk}");
        }
        // Uneven, growing chunks, with empty updates between.
        let mut h = Md5::new();
        let (mut at, mut step) = (0, 1);
        while at < data.len() {
            let end = (at + step).min(data.len());
            h.update(&data[at..end]);
            h.update(&[]);
            at = end;
            step = step * 3 % 1013 + 1;
        }
        assert_eq!(h.finish(), want);
    }

    /// RFC 1321 appendix A.5.
    #[test]
    fn rfc_1321_vectors() {
        let hex = |d: [u8; 16]| data_encoding::HEXLOWER.encode(&d);
        assert_eq!(hex(ours(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(ours(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex(ours(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            )),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }
}
