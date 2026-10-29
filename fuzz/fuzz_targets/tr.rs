#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| dogfish_internal::fuzz_api::tr(data));
