#![no_main]

//! NEVER-PANICS target: everything jjpr parses out of a `jj` subprocess.
//!
//! jjpr does not read a repository, it reads the *stdout of another program*. Every
//! bookmark, change id, commit id and description reaching jjpr's logic has been
//! through `parse_bookmark_output` or `parse_log_output` first, so these two are the
//! real trust boundary — and unlike a file format nobody owns the schema: a jj
//! upgrade can change a template's output underneath us, and descriptions are
//! arbitrary user text that has already survived one round of `escape_json`.
//!
//! The contract is availability: any bytes at all, and jjpr reports an error rather
//! than panicking. A panic here is not cosmetic — it aborts a `jjpr merge` partway
//! through a stack, which is exactly the state jjpr's recovery machinery exists to
//! avoid ever being in.
//!
//! Discards the parse result, so it finds availability bugs only; a *wrong* parse is
//! out of scope for this target. The structural invariants that a successful parse
//! must satisfy are asserted in `graph_invariants`, which consumes this one's output.

use libfuzzer_sys::fuzz_target;

use jjpr::jj::templates::{parse_bookmark_output, parse_log_output};

fuzz_target!(|data: &[u8]| {
    // Both parsers take &str. Lossy rather than a UTF-8 guard on purpose: jj's output
    // is not guaranteed valid UTF-8 (a description can carry any bytes the user
    // committed), and jjpr itself converts lossily, so rejecting invalid UTF-8 here
    // would fuzz a path jjpr never takes.
    let text = String::from_utf8_lossy(data);

    let _ = parse_log_output(&text);
    let _ = parse_bookmark_output(&text);
});
