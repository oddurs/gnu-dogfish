//! Type-check-only stand-in for `ring` (cross-target `cargo check` without an SDK).
pub mod digest {
    pub struct Algorithm;
    pub static SHA256: Algorithm = Algorithm;
    pub struct Context;
    pub struct Digest([u8; 32]);
    impl AsRef<[u8]> for Digest { fn as_ref(&self) -> &[u8] { &self.0 } }
    impl Context {
        pub fn new(_: &'static Algorithm) -> Self { Context }
        pub fn update(&mut self, _: &[u8]) {}
        pub fn finish(self) -> Digest { Digest([0; 32]) }
    }
}
