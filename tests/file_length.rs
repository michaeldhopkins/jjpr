//! The file-length gate for `src/`.
//!
//! Its `production_lines` rule is the subtle part; see the comment on it. Function-level lints
//! (`too_many_lines`) never see a file growing one function at a time, which is how nine files
//! here were over the limit when it went in, `main.rs` at 1,893 production lines.
//!
//! Two decisions make it useful rather than annoying:
//!
//! 1. **Inline tests don't count.** Every module here keeps `#[cfg(test)] mod tests` in the same
//!    file, so counting whole files would mean adding tests can break the build. Only lines
//!    outside test-only items are measured.
//!
//! 2. **It ratchets.** A file already over the limit is pinned at the size it was when the gate
//!    went in (2026-09-26): allowed to shrink, never to grow, and a shrink must lower its pin in
//!    the same change (the test says to), so the file cannot grow back. New code goes in a new
//!    module, never into a pinned file, and the limit is never raised to fit a file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use syn::spanned::Spanned;
use syn::visit::Visit;

/// The ceiling for a file under `src/`, in production lines.
const LIMIT: usize = 400;

/// Files over the limit, each pinned at its production size when the gate went in: it may
/// shrink, never grow. A file that outgrows the limit later is split, never pinned.
fn pinned() -> HashMap<&'static str, usize> {
    HashMap::from([
        ("src/forge/github.rs", 682),
        ("src/forge/gitlab.rs", 471),
        ("src/forge/http.rs", 541),
        ("src/forge/types.rs", 409),
        ("src/main.rs", 1879),
        ("src/merge/execute.rs", 1086),
        ("src/submit/execute.rs", 733),
        ("src/submit/plan.rs", 735),
        ("src/watch.rs", 1198),
    ])
}

/// Is this item compiled only for tests (`#[test]`, `#[cfg(test)]`)?
fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("test")
            || (a.path().is_ident("cfg")
                && a.parse_args::<syn::Meta>()
                    .is_ok_and(|m| m.path().is_ident("test")))
    })
}
/// The 1-based line ranges of every test-only item, its attributes and doc comments included.
struct TestItems(Vec<(usize, usize)>);

impl TestItems {
    fn add(&mut self, attrs: &[syn::Attribute], item: &impl Spanned) -> bool {
        if !test_only(attrs) {
            return false;
        }
        let span = item.span();
        self.0.push((span.start().line, span.end().line));
        true
    }
}

impl<'a> Visit<'a> for TestItems {
    fn visit_item(&mut self, i: &'a syn::Item) {
        let attrs = match i {
            syn::Item::Const(x) => &x.attrs,
            syn::Item::Enum(x) => &x.attrs,
            syn::Item::Fn(x) => &x.attrs,
            syn::Item::Impl(x) => &x.attrs,
            syn::Item::Macro(x) => &x.attrs,
            syn::Item::Mod(x) => &x.attrs,
            syn::Item::Static(x) => &x.attrs,
            syn::Item::Struct(x) => &x.attrs,
            syn::Item::Trait(x) => &x.attrs,
            syn::Item::Type(x) => &x.attrs,
            syn::Item::Use(x) => &x.attrs,
            _ => return syn::visit::visit_item(self, i),
        };
        if !self.add(attrs, i) {
            syn::visit::visit_item(self, i);
        }
    }
    fn visit_impl_item_fn(&mut self, f: &'a syn::ImplItemFn) {
        if !self.add(&f.attrs, f) {
            syn::visit::visit_impl_item_fn(self, f);
        }
    }
}

/// The file's lines outside its test-only items (`#[cfg(test)]` modules, helpers and impls,
/// `#[test]` functions), wherever they sit: a module may keep test modules between its functions.
///
/// Read from the parsed file, never the text. Every text rule tried for this gate was fooled by a
/// shape of ordinary code: taking any `#[cfg(test)]` as the start of the tests waved through
/// arbitrarily large files, stopping at the first test module measured a 7,993-line file at 74,
/// and ending a module at the first `}` in column 0 was fooled by fixture strings, which hold one
/// as often as the module's end does.
fn production_lines(source: &str) -> usize {
    let file = syn::parse_file(source).unwrap_or_else(|e| panic!("does not parse: {e}"));
    let mut tests = TestItems(Vec::new());
    tests.visit_file(&file);
    let total = source.lines().count();
    (1..=total)
        .filter(|line| !tests.0.iter().any(|(a, b)| (a..=b).contains(&line)))
        .count()
}

fn crate_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            sources(&path, found);
        } else if path.extension().is_some_and(|e| e == "rs") {
            found.push(path);
        }
    }
}

/// The verdict for one file, or `None` when it is within its allowance. Split out from the
/// walk so the ratchet stays testable: with a tree that satisfies every pin, these branches
/// would go unexercised and rot.
fn verdict(
    relative: &str,
    lines: usize,
    limit: usize,
    pinned: &HashMap<&'static str, usize>,
) -> Option<String> {
    match pinned.get(relative) {
        Some(&ceiling) if lines > ceiling => Some(format!(
            "{relative}: {lines} lines, up from its pinned {ceiling}. It is already over the {limit}-line limit; split it rather than growing it further."
        )),
        Some(_) if lines <= limit => Some(format!(
            "{relative}: down to {lines} lines — under the {limit} limit, so remove its entry from `pinned()` and let the real limit hold it there."
        )),
        // The ratchet clicks: a pin left above the file's size would let it grow back.
        Some(&ceiling) if lines < ceiling => Some(format!(
            "{relative}: down to {lines} lines from its pinned {ceiling}. Lower its pin to {lines} so it cannot grow back."
        )),
        Some(_) => None,
        None if lines > limit => Some(format!(
            "{relative}: {lines} lines, over the {limit} limit. Split it into pieces that each do one thing (inline tests are not counted, so they are not the cause)."
        )),
        None => None,
    }
}

#[test]
fn no_file_outgrows_its_limit() {
    let root = crate_root();
    let pinned = pinned();
    let mut files = Vec::new();
    sources(&root.join("src"), &mut files);
    assert!(
        !files.is_empty(),
        "no `.rs` files under `src` — the walk is broken, not the tree"
    );
    let mut failures = Vec::new();
    for path in files {
        let relative = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(&path).unwrap_or_default();
        if let Some(f) = verdict(&relative, production_lines(&source), LIMIT, &pinned) {
            failures.push(f);
        }
    }
    failures.sort();
    assert!(failures.is_empty(), "\n{}\n", failures.join("\n"));
}

#[test]
fn the_ratchet_holds_a_pinned_file_to_its_size() {
    let pinned = HashMap::from([("a.rs", 500)]);
    assert!(
        verdict("a.rs", 500, 400, &pinned).is_none(),
        "at its ceiling is fine"
    );
    assert!(
        verdict("a.rs", 450, 400, &pinned).is_some_and(|m| m.contains("Lower its pin to 450")),
        "a shrink must lower the pin, or the file could grow back"
    );
    assert!(
        verdict("a.rs", 501, 400, &pinned).is_some_and(|m| m.contains("up from its pinned")),
        "a pinned file may not grow"
    );
    assert!(
        verdict("a.rs", 400, 400, &pinned).is_some_and(|m| m.contains("remove its entry")),
        "once under the limit the pin must go"
    );
    assert!(
        verdict("b.rs", 400, 400, &pinned).is_none(),
        "unpinned, at the limit"
    );
    assert!(
        verdict("b.rs", 401, 400, &pinned).is_some_and(|m| m.contains("over the")),
        "unpinned, over the limit"
    );
}

#[test]
fn every_pinned_file_still_exists() {
    // A rename leaving a stale entry would exempt nothing, and the gate would quietly stop
    // protecting the file it names.
    let root = crate_root();
    let missing: Vec<&str> = pinned()
        .keys()
        .copied()
        .filter(|r| !root.join(r).exists())
        .collect();
    assert!(
        missing.is_empty(),
        "pinned files no longer at these paths: {missing:?}"
    );
}

#[test]
fn only_test_items_are_left_out_of_the_count() {
    assert_eq!(
        production_lines("fn a() {}\nfn b() {}\n#[cfg(test)]\nmod tests {\n // lots\n}\n"),
        2
    );
    assert_eq!(
        production_lines("fn a() {}\n"),
        1,
        "a file with no tests counts whole"
    );
    assert_eq!(production_lines(""), 0);
    let cases: &[(&str, &str, usize)] = &[
        (
            "a test-only mod declaration is its own line, not the rest of the file",
            "mod real;\n#[cfg(test)]\nmod test_support;\n\nfn a() {}\nfn b() {}\n",
            4,
        ),
        (
            "a test-only helper is the helper, not the rest of the file",
            "fn a() {}\n#[cfg(test)]\nfn helper() {}\nfn b() {}\nfn c() {}\n",
            3,
        ),
        (
            "an attribute between the guard and the item goes with the item",
            "#[cfg(test)]\n#[path = \"t.rs\"]\nmod tests;\n\nfn a() {}\nfn b() {}\n",
            3,
        ),
        (
            "a same-line test module body is still skipped",
            "fn a() {}\n#[cfg(test)] mod tests {\n    fn t() {}\n}\n",
            1,
        ),
        (
            "a one-line test module",
            "fn a() {}\n#[cfg(test)] mod tests { fn t() {} }\nfn b() {}\n",
            2,
        ),
        (
            "production code after a test module still counts (a module may keep them between functions)",
            "fn a() {}\n#[cfg(test)]\nmod a_tests {\n    #[test]\n    fn t() {\n    }\n}\n\nfn b() {}\nfn c() {}\n",
            4,
        ),
        (
            "a `}` in column 0 inside a test's string does not end the module",
            "#[cfg(test)]\nmod tests {\n    const FIX: &str = \"\n}\n\";\n    fn t() {}\n}\nfn b() {}\n",
            1,
        ),
        (
            "`#[cfg(test)]` in a string or comment is not an attribute",
            "// #[cfg(test)]\nconst A: &str = \"\n#[cfg(test)]\nmod x {\";\nfn b() {}\n",
            5,
        ),
        (
            "a test-only method in a production impl",
            "impl A {\n    fn a() {}\n    #[cfg(test)]\n    fn t() {}\n}\n",
            3,
        ),
        (
            "doc comments go with their item",
            "/// tests\n#[cfg(test)]\nmod t {}\nfn b() {}\n",
            1,
        ),
    ];
    for (why, source, expected) in cases {
        assert_eq!(production_lines(source), *expected, "{why}");
    }
}
