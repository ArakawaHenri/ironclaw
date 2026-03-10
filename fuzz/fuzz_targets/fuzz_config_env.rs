#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        // Exercise TOML config parsing with arbitrary input
        let _ = toml::from_str::<toml::Value>(s);

        // Exercise JSON config parsing
        let _ = serde_json::from_str::<serde_json::Value>(s);
    }
});
