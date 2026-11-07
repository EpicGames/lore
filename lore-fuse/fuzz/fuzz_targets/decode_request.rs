#![no_main]

use libfuzzer_sys::fuzz_target;

// Any byte sequence must decode to Ok or a DecodeError, never panic.
fuzz_target!(|data: &[u8]| {
    let _ = lore_fuse::codec::decode(data);
});
