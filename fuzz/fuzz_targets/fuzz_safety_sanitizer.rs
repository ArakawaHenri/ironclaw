#![no_main]
use libfuzzer_sys::fuzz_target;
use ironclaw::safety::Sanitizer;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let sanitizer = Sanitizer::new();

        // Exercise the main sanitization path
        let result = sanitizer.sanitize(s);
        // Verify invariant: warnings should have valid ranges
        for w in &result.warnings {
            assert!(w.location.end <= s.len());
        }

        // Exercise detection-only path
        let warnings = sanitizer.detect(s);
        assert_eq!(warnings.len(), result.warnings.len());
    }
});
