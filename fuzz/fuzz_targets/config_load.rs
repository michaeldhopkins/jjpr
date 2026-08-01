#![no_main]

//! CONFIG target: `.jjpr.toml` is user-authored, hand-edited, and read on every run.
//!
//! Config parsing has to fail as a *diagnosis* — "unknown merge_method: yolo" — and
//! never as a panic, because it runs before jjpr has done anything else and a panic
//! here means the tool cannot start at all. The existing tests cover the enums by
//! example (`merge_method = "yolo"` rejects, `""` gives defaults); this covers the
//! shapes nobody enumerates: a key holding the wrong TOML *type*, a deeply nested
//! table where a scalar belongs, duplicate keys, a 4MB string.
//!
//! Runs the bytes twice through the same deserializer to also hold it to
//! determinism. A config that parsed one way and then another would make jjpr's
//! behaviour depend on allocation order — which is exactly the kind of bug that
//! survives a test suite and only shows up on someone else's machine.

use libfuzzer_sys::fuzz_target;

use jjpr::config::Config;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return; // TOML is defined over UTF-8; invalid bytes are the reader's job
    };

    let first = toml::from_str::<Config>(text);
    let second = toml::from_str::<Config>(text);

    assert_eq!(
        first.is_ok(),
        second.is_ok(),
        "config parsing is not deterministic for input: {text:?}"
    );

    if let (Ok(a), Ok(b)) = (first, second) {
        // Config has no PartialEq; compare the debug rendering, which is enough to
        // catch a field landing differently between two runs of the same input.
        assert_eq!(
            format!("{a:?}"),
            format!("{b:?}"),
            "config parsed to two different values for input: {text:?}"
        );
    }
});
