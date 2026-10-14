//! Every revset jjpr builds, and every bookmark name it puts on jj's command
//! line, goes through `src/jj/revset.rs`, which quotes names.
//!
//! Formatted anywhere else, a name lands in a revset bare, and a bookmark named
//! `foo@origin` resolves to the remote bookmark `foo` on `origin`. This reads
//! the production code (test-only items are skipped) as tokens, so a string or
//! comment cannot fool it, and fails on:
//!
//! 1. a string literal that interpolates (`{`) into revset syntax;
//! 2. a literal that is only a revset operator (`" | "`, `"::"`), which joins
//!    revsets, or one that holds revset syntax and starts or ends with an
//!    operator, which concatenates them;
//! 3. a bookmark name argument to jj (`bookmark set|create|move|rename|forget|
//!    delete|track|untrack <name>`, `--bookmark`, `-b`, `--branch`) that is not
//!    a `revset::` call or a variable the same function bound to one;
//! 4. a revision argument to jj (`-r`, `-d`, `-s`, `--from`, `jj new <rev>`,
//!    and the rest of `REVISION_FLAGS`) that is a variable the same function
//!    bound with `let` to something other than a `revset::` call, or a field
//!    or method result. A literal, a builder call and any other name (a
//!    parameter, a loop or pattern binding) pass: the `Jj` trait documents
//!    its parameters as revsets, so the check moves to the callers, which
//!    rules 1 and 2 cover.
//!
//! It reads tokens, not types, so it is a tripwire, not a proof: a name that
//! reaches jj through a `Jj` trait method or a helper's return value is not seen.
//!
//! `tests/revset_names.rs` checks the result against a real jj.

use std::path::{Path, PathBuf};

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::spanned::Spanned;
use syn::visit::Visit;

/// The one file that may format revsets.
const BUILDER: &str = "src/jj/revset.rs";

/// Files whose revset-looking literals are advice printed for the user to
/// run, never handed to jj by jjpr. The text is the owner's to change.
const ADVICE: &[&str] = &["src/hints.rs"];

/// Revset syntax that, next to an interpolation, means a revset is being built.
const REVSET_MARKERS: &[&str] = &[
    "trunk()",
    "descendants(",
    "ancestors(",
    "children(",
    "parents(",
    "roots(",
    "heads(",
    "conflicts(",
    "change_id(",
    "commit_id(",
    "author(",
    "bookmarks(",
    "remote_bookmarks(",
    "mine()",
    "visible_heads(",
    "working_copies(",
    "exact:",
    "glob:",
];

/// Revset operators. One beside an interpolation, with at most a space between,
/// builds a revset; doubled (`&&`, `||`) or an ellipsis it is prose.
const OPERATORS: &[&str] = &["::", "..", "~", "&", "|"];

/// Operators that build a revset written straight after an interpolation:
/// `format!("{b}@origin")`, `format!("{b}-")`. Before one (`watch-{pid}`,
/// `by @{user}`) or spaced (`  - {item}`) they are prose.
const SUFFIX_OPERATORS: &[char] = &['@', '-', '+'];

/// Variables, by file, that hold an id jj printed and that jjpr hands back to
/// jj as a revision. Reviewed: an id is never a bookmark name.
const JJ_IDS: &[(&str, &str)] = &[("src/undo/repo.rs", "then")];

/// `bookmark <sub> <name>` subcommands whose next argument is a name.
const NAME_SUBCOMMANDS: &[&str] = &[
    "set", "create", "move", "rename", "forget", "delete", "track", "untrack",
];

/// Flags whose next argument is a bookmark name (for `git push`/`fetch`;
/// `rebase -b` takes a revision and would want `revset::symbol` instead).
const NAME_FLAGS: &[&str] = &["--bookmark", "-b", "--branch"];

/// Flags whose next argument is a revision.
const REVISION_FLAGS: &[&str] = &[
    "-r",
    "--revision",
    "--revisions",
    "-d",
    "--destination",
    "-o",
    "--onto",
    "-s",
    "--source",
    "-A",
    "--insert-after",
    "-B",
    "--insert-before",
    "--from",
    "--to",
    "--into",
];

/// Subcommands whose next argument, unless it is a flag, is a revision.
const REVISION_SUBCOMMANDS: &[&str] = &["new", "edit"];

fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("test")
            || (a.path().is_ident("cfg")
                && a.parse_args::<syn::Meta>()
                    .is_ok_and(|m| m.path().is_ident("test")))
    })
}

/// The line ranges of test-only items.
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

/// A token of production code, flattened, with its line.
#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// A string literal's value (raw or not).
    Str(String),
    Ident(String),
    Punct(char),
    Open,
    Close,
}

fn string_value(lit: &str) -> Option<String> {
    let lit = lit.strip_prefix('b').unwrap_or(lit);
    if let Some(raw) = lit.strip_prefix('r') {
        let hashes = raw.len() - raw.trim_start_matches('#').len();
        let body = &raw[hashes..];
        return Some(body[1..body.len() - 1 - hashes].to_string());
    }
    let body = lit.strip_prefix('"')?.strip_suffix('"')?;
    Some(body.replace("\\\"", "\"").replace("\\\\", "\\"))
}

fn flatten(stream: TokenStream, tests: &[(usize, usize)], out: &mut Vec<(usize, Tok)>) {
    for tree in stream {
        let line = tree.span().start().line;
        if tests.iter().any(|(a, b)| (*a..=*b).contains(&line)) {
            continue;
        }
        match tree {
            TokenTree::Group(g) => {
                let bracketed = g.delimiter() != Delimiter::None;
                if bracketed {
                    out.push((line, Tok::Open));
                }
                flatten(g.stream(), tests, out);
                if bracketed {
                    out.push((g.span_close().start().line, Tok::Close));
                }
            }
            TokenTree::Ident(i) => out.push((line, Tok::Ident(i.to_string()))),
            TokenTree::Punct(p) => out.push((line, Tok::Punct(p.as_char()))),
            TokenTree::Literal(l) => {
                if let Some(s) = string_value(&l.to_string()) {
                    out.push((line, Tok::Str(s)));
                }
            }
        }
    }
}

/// The production tokens of a source file.
fn production_tokens(source: &str) -> Vec<(usize, Tok)> {
    let file = syn::parse_file(source).unwrap_or_else(|e| panic!("does not parse: {e}"));
    let mut tests = TestItems(Vec::new());
    tests.visit_file(&file);
    let stream: TokenStream = source.parse().expect("lexes");
    let mut out = Vec::new();
    flatten(stream, &tests.0, &mut out);
    out
}

/// Printed advice (`  jj rebase -s {} -d {base}`) is for the user to run;
/// jjpr never hands jj a command line as one string.
fn builds_a_revset(s: &str) -> bool {
    s.contains('{')
        && !s.contains("jj ")
        && (REVSET_MARKERS.iter().any(|m| s.contains(m))
            || operator_beside_interpolation(s)
            || suffix_operator(s))
}

/// `{a}-{b}` joins two values (a date, an id) and is not a revset; `{b}@{r}` is.
fn suffix_operator(s: &str) -> bool {
    SUFFIX_OPERATORS.iter().any(|&op| {
        s.match_indices(&format!("}}{op}")).any(|(at, _)| {
            let rest = &s[at + 2..];
            op == '@' || !rest.starts_with('{')
        })
    })
}

/// A URL query (`?a={x}&b={y}`) is not a revset: jj's revset syntax has no `=`.
fn operator_beside_interpolation(s: &str) -> bool {
    if s.contains('=') {
        return false;
    }
    OPERATORS.iter().any(|op| {
        s.match_indices(op).any(|(at, _)| {
            let before = s[..at].trim_end_matches(' ');
            let after = s[at + op.len()..].trim_start_matches(' ');
            let doubled = s[..at].ends_with(op.chars().last().unwrap_or(' '))
                || s[at + op.len()..].starts_with(op.chars().next().unwrap_or(' '));
            !doubled && (before.ends_with('}') || after.starts_with('{'))
        })
    })
}

/// What the identifier `name` was last bound to with `let`, before `at` and
/// inside the same function: `Some(true)` for a `revset::` call, `Some(false)`
/// for anything else, `None` when it is not bound there (a parameter).
fn binding(toks: &[(usize, Tok)], at: usize, name: &str) -> Option<Argument> {
    let tok = |i: usize| toks.get(i).map(|t| &t.1);
    for i in (0..at).rev() {
        if toks[i].1 == Tok::Ident("fn".into()) {
            return None;
        }
        if toks[i].1 != Tok::Ident("let".into()) {
            continue;
        }
        let mut j = i + 1;
        if tok(j) == Some(&Tok::Ident("mut".into())) {
            j += 1;
        }
        if tok(j) != Some(&Tok::Ident(name.into())) {
            continue;
        }
        j += 1;
        if tok(j) == Some(&Tok::Punct(':')) {
            while tok(j).is_some_and(|t| *t != Tok::Punct('=')) && j < at {
                j += 1;
            }
        }
        if tok(j) == Some(&Tok::Punct('=')) {
            return Some(built(toks, j + 1).unwrap_or(Argument::Other));
        }
    }
    None
}

/// How the jj argument starting at `at` was made.
#[derive(Debug, PartialEq)]
enum Argument {
    /// A `revset::` call, or a variable bound to one, naming the builder.
    Built(String),
    /// A string literal.
    Literal,
    /// A variable not bound in this function: a parameter.
    Parameter,
    /// Anything else, a field or a method's result included.
    Other,
}

/// `revset::<builder>` starting at `at`.
fn built(toks: &[(usize, Tok)], at: usize) -> Option<Argument> {
    let tok = |i: usize| toks.get(i).map(|t| &t.1);
    // A parameter may be called `revset` too; the builder is `revset::`.
    if tok(at) != Some(&Tok::Ident("revset".into())) || tok(at + 1) != Some(&Tok::Punct(':')) {
        return None;
    }
    match tok(at + 3) {
        Some(Tok::Ident(f)) => Some(Argument::Built(f.clone())),
        _ => Some(Argument::Other),
    }
}

/// Methods that hand back the variable's own text.
const PASS_THROUGH: &[&str] = &["as_str", "trim", "as_ref", "clone", "to_string"];

fn argument(toks: &[(usize, Tok)], mut at: usize) -> Argument {
    let tok = |i: usize| toks.get(i).map(|t| &t.1);
    while matches!(tok(at), Some(Tok::Punct(',' | '&'))) {
        at += 1;
    }
    if let Some(b) = built(toks, at) {
        return b;
    }
    match tok(at) {
        Some(Tok::Str(_)) => Argument::Literal,
        Some(Tok::Ident(i)) if tok(at + 1) != Some(&Tok::Punct(':')) => {
            let mut end = at + 1;
            while tok(end) == Some(&Tok::Punct('.'))
                && matches!(tok(end + 1), Some(Tok::Ident(m)) if PASS_THROUGH.contains(&m.as_str()))
                && tok(end + 2) == Some(&Tok::Open)
                && tok(end + 3) == Some(&Tok::Close)
            {
                end += 4;
            }
            if !matches!(tok(end), None | Some(Tok::Punct(',') | Tok::Close)) {
                return Argument::Other;
            }
            binding(toks, at, i).unwrap_or(Argument::Parameter)
        }
        _ => Argument::Other,
    }
}

/// The builder a bookmark-name slot needs: `bookmark set`/`create`/`rename` read a
/// symbol, the rest take a string pattern.
fn name_builder(flag: &str) -> &'static str {
    if matches!(flag, "set" | "create" | "rename") {
        "symbol"
    } else {
        "exact_name"
    }
}

/// The literal is a lone operator, or revset syntax that a `+` would extend.
fn joins_revsets(s: &str) -> bool {
    let t = s.trim();
    OPERATORS.contains(&t)
        || REVSET_MARKERS.iter().any(|m| t.contains(m))
            && OPERATORS
                .iter()
                .any(|op| t.starts_with(op) || t.ends_with(op))
}

/// Every violation in one file's production tokens, as `line: what`.
fn violations(relative: &str, toks: &[(usize, Tok)]) -> Vec<String> {
    if relative == BUILDER {
        return Vec::new();
    }
    let mut found = Vec::new();
    for (i, (line, tok)) in toks.iter().enumerate() {
        let Tok::Str(s) = tok else { continue };
        if builds_a_revset(s) && !ADVICE.contains(&relative) {
            found.push(format!(
                "{relative}:{line}: revset formatted outside {BUILDER}: {s:?}"
            ));
        }
        if joins_revsets(s) && !ADVICE.contains(&relative) {
            found.push(format!(
                "{relative}:{line}: revsets joined outside {BUILDER}: {s:?}"
            ));
        }
        let after_bookmark = NAME_SUBCOMMANDS.contains(&s.as_str())
            && i >= 2
            && toks[i - 2].1 == Tok::Str("bookmark".into());
        if NAME_FLAGS.contains(&s.as_str()) || after_bookmark {
            let names = if s == "rename" { 2 } else { 1 };
            let mut at = i + 1;
            for _ in 0..names {
                let want = name_builder(s);
                if argument(toks, at) != Argument::Built(want.into()) {
                    found.push(format!(
                        "{relative}:{line}: bookmark name after {s:?} not built by revset::{want}"
                    ));
                }
                at = next_element(toks, at);
            }
        }
        let after_subcommand = REVISION_SUBCOMMANDS.contains(&s.as_str())
            && first_element(toks, i)
            && !matches!(element(toks, i + 1), Some(Tok::Str(f)) if f.starts_with('-'));
        let jj_id = matches!(element(toks, i + 1), Some(Tok::Ident(v))
            if JJ_IDS.contains(&(relative, v.as_str())));
        if (REVISION_FLAGS.contains(&s.as_str()) || after_subcommand)
            && argument(toks, i + 1) == Argument::Other
            && !jj_id
        {
            found.push(format!(
                "{relative}:{line}: revision after {s:?} not built by revset::"
            ));
        }
    }
    found
}

/// The token after the `,` following position `at`.
fn element(toks: &[(usize, Tok)], at: usize) -> Option<&Tok> {
    let mut i = at;
    while matches!(toks.get(i).map(|t| &t.1), Some(Tok::Punct(',' | '&'))) {
        i += 1;
    }
    toks.get(i).map(|t| &t.1)
}

/// The position of the next array element after the one starting at `at`.
fn next_element(toks: &[(usize, Tok)], at: usize) -> usize {
    let mut i = at;
    while matches!(toks.get(i).map(|t| &t.1), Some(Tok::Punct(',' | '&'))) {
        i += 1;
    }
    let mut depth = 0usize;
    while let Some((_, t)) = toks.get(i) {
        match t {
            Tok::Open => depth += 1,
            Tok::Close if depth == 0 => return i,
            Tok::Close => depth -= 1,
            Tok::Punct(',') if depth == 0 => return i,
            _ => {}
        }
        i += 1;
    }
    i
}

/// Whether the literal at `at` is an element of an array of jj arguments,
/// rather than data that happens to read `"new"`: it opens the array, or a
/// flag literal comes before it.
fn first_element(toks: &[(usize, Tok)], at: usize) -> bool {
    let before = at.checked_sub(1).map(|i| &toks[i].1);
    let flag_before = at
        .checked_sub(2)
        .is_some_and(|i| matches!(&toks[i].1, Tok::Str(f) if f.starts_with("--")));
    before == Some(&Tok::Open) && toks.get(at + 1).map(|t| &t.1) == Some(&Tok::Punct(','))
        || before == Some(&Tok::Punct(',')) && flag_before
}

fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read src").flatten() {
        let path = entry.path();
        if path.is_dir() {
            sources(&path, found);
        } else if path.extension().is_some_and(|e| e == "rs") {
            found.push(path);
        }
    }
}

#[test]
fn every_revset_and_bookmark_argument_goes_through_the_builder() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&root.join("src"), &mut files);
    assert!(files.len() > 50, "found only {} source files", files.len());
    assert!(
        files.iter().any(|f| f.ends_with("jj/revset.rs")),
        "no builder"
    );
    let found: Vec<String> = files
        .iter()
        .flat_map(|path| {
            let relative = path.strip_prefix(root).expect("under root");
            let relative = relative.to_string_lossy().replace('\\', "/");
            let source = std::fs::read_to_string(path).expect("read");
            violations(&relative, &production_tokens(&source))
        })
        .collect();
    assert!(found.is_empty(), "{}", found.join("\n"));
}

fn check(source: &str) -> Vec<String> {
    violations("src/x.rs", &production_tokens(source))
}

#[test]
fn a_formatted_revset_is_caught() {
    assert_eq!(
        check("fn f(b: &str) -> String { format!(\"trunk()..{b}\") }").len(),
        1
    );
    assert_eq!(
        check("fn f(b: &str) -> String { format!(\"{b}::\") }").len(),
        1
    );
    assert_eq!(
        check("fn f(r: &str) -> String { format!(\"roots({r})\") }").len(),
        1
    );
    assert_eq!(
        check("fn f(r: &str) -> String { format!(r#\"x \"{r}\" & {r}\"#) }").len(),
        1
    );
}

#[test]
fn a_name_with_a_remote_or_parent_operator_is_caught() {
    for src in [
        "fn f(b: &str) -> String { format!(\"{b}@origin\") }",
        "fn f(b: &str, r: &str) -> String { format!(\"{b}@{r}\") }",
        "fn f(b: &str) -> String { format!(\"{b}-\") }",
        "fn f(b: &str) -> String { format!(\"{b}+\") }",
    ] {
        assert_eq!(check(src).len(), 1, "{src}");
    }
}

#[test]
fn display_text_with_braces_is_not_a_revset() {
    for src in [
        "fn f(n: &str) -> String { format!(\"Blocked at '{n}' ({n})\") }",
        "fn f(p: u32) -> String { format!(\"watch-{p}\") }",
        "fn f(a: u32, b: u32) -> String { format!(\"{a}-{b}\") }",
        "fn f(u: &str) -> String { format!(\" by @{u}\") }",
        "fn f(n: &str) -> String { format!(\"  jj rebase -s {n} -d {n}@origin\") }",
    ] {
        assert!(check(src).is_empty(), "{src}");
    }
}

#[test]
fn a_join_of_revsets_is_caught() {
    for src in [
        "fn f(v: Vec<String>) -> String { v.join(\" | \") }",
        "fn f(v: Vec<String>) -> String { v.join(\"|\") }",
        "fn f(v: Vec<String>) -> String { v.join(\"::\") }",
        "fn f(b: String) -> String { b + \"..trunk()\" }",
    ] {
        assert_eq!(check(src).len(), 1, "{src}");
    }
    assert!(check("fn f(v: Vec<String>) -> String { v.join(\" + \") }").is_empty());
}

#[test]
fn a_revision_bound_to_anything_but_the_builder_is_caught() {
    for src in [
        "fn f(b: &B) { let n = b.name.clone(); run(&[\"log\", \"-r\", &n]); }",
        "fn f(b: &B) { let n = b.name.clone(); run(&[\"rebase\", \"-d\", &n]); }",
        "fn f(b: &B) { let n = b.name.clone(); run(&[\"new\", &n]); }",
        "fn f(b: &B) { let n = b.name.clone(); run(&[\"--quiet\", \"new\", &n]); }",
    ] {
        assert_eq!(check(src).len(), 1, "{src}");
    }
}

#[test]
fn a_revision_that_is_a_literal_parameter_or_built_passes() {
    for src in [
        "fn f() { run(&[\"log\", \"-r\", \"@\"]); }",
        "fn f(revset: &str) { run(&[\"log\", \"-r\", revset]); }",
        "fn f(n: &str) { run(&[\"log\", \"-r\", &revset::symbol(n)]); }",
        "fn f(n: &str) { let r = revset::symbol(n); run(&[\"new\", &r]); }",
        "fn f() { run(&[\"new\", \"-m\", \"x\"]); }",
        "fn f() { let s = vec![\"old\", \"new\", \"x\"]; }",
    ] {
        assert!(check(src).is_empty(), "{src}");
    }
}

#[test]
fn a_revision_from_a_field_or_method_is_caught() {
    for src in [
        "fn f(s: &S) { run(&[\"rebase\", \"-d\", &s.bookmark.name]); }",
        "fn f(b: &B) { run(&[\"log\", \"-r\", b.name()]); }",
        "fn f(v: Vec<B>) { for b in v { run(&[\"log\", \"-r\", &b.name]); } }",
        "fn f(b: &B) { let n: String = b.name.clone(); run(&[\"log\", \"-r\", &n]); }",
        "fn f(b: &B) { let mut n = b.name.clone(); run(&[\"log\", \"-r\", &n]); }",
    ] {
        assert_eq!(check(src).len(), 1, "{src}");
    }
    for src in [
        "fn f(d: &String) { run(&[\"rebase\", \"-d\", d.as_str()]); }",
        "fn f(d: &str) { run(&[\"rebase\", \"-d\", d.trim(), \"-s\", d]); }",
        "fn f(n: &str) { let mut r: String = revset::symbol(n); run(&[\"log\", \"-r\", &r]); }",
    ] {
        assert!(check(src).is_empty(), "{src}");
    }
}

#[test]
fn a_name_slot_needs_its_own_builder() {
    let wrong_forget = "fn f(n: &str) { run(&[\"bookmark\", \"forget\", &revset::symbol(n)]); }";
    assert_eq!(check(wrong_forget).len(), 1);
    let wrong_set =
        "fn f(n: &str) { run(&[\"bookmark\", \"set\", &revset::exact_name(n), \"-r\", \"@\"]); }";
    assert_eq!(check(wrong_set).len(), 1);
    let right_set =
        "fn f(n: &str) { run(&[\"bookmark\", \"set\", &revset::symbol(n), \"-r\", \"@\"]); }";
    assert!(check(right_set).is_empty());
}

#[test]
fn a_binding_in_another_function_does_not_count() {
    let src = "fn g(n: &str) { let p = revset::exact_name(n); } \
               fn f(p: &str) { run(&[\"bookmark\", \"forget\", p]); }";
    assert_eq!(check(src).len(), 1);
}

#[test]
fn a_bare_bookmark_argument_is_caught() {
    let bare = "fn f(n: &str) { run(&[\"bookmark\", \"forget\", n]); }";
    assert_eq!(check(bare).len(), 1);
    let bare_push = "fn f(n: &str) { run(&[\"git\", \"push\", \"--bookmark\", n]); }";
    assert_eq!(check(bare_push).len(), 1);
    let bare_set = "fn f(n: &str) { run(&[\"bookmark\", \"set\", n, \"-r\", \"@\"]); }";
    assert_eq!(check(bare_set).len(), 1);
    let bare_b = "fn f(n: &str) { run(&[\"git\", \"push\", \"-b\", n]); }";
    assert_eq!(check(bare_b).len(), 1);
    let bare_move = "fn f(n: &str) { run(&[\"bookmark\", \"move\", n, \"--to\", \"@\"]); }";
    assert_eq!(check(bare_move).len(), 1);
    let bare_rename = "fn f(a: &str, b: &str) { run(&[\"bookmark\", \"rename\", a, b]); }";
    assert_eq!(check(bare_rename).len(), 2, "both names");
    let fetch = "fn f(n: &str) { run(&[\"git\", \"fetch\", \"--branch\", n]); }";
    assert_eq!(check(fetch).len(), 1);
}

#[test]
fn a_built_bookmark_argument_passes() {
    let inline = "fn f(n: &str) { run(&[\"bookmark\", \"forget\", &revset::exact_name(n)]); }";
    assert!(check(inline).is_empty());
    let bound = "fn f(n: &str) { let p = revset::exact_name(n); \
                 run(&[\"--bookmark\", &p]); }";
    assert!(check(bound).is_empty());
    let rebound = "fn f(n: &str) { let p = revset::exact_name(n); let p = n; \
                   run(&[\"--bookmark\", &p]); }";
    assert_eq!(check(rebound).len(), 1, "the nearest binding counts");
}

#[test]
fn test_code_is_not_read() {
    let src =
        "#[cfg(test)]\nmod tests {\n fn f(b: &str) -> String { format!(\"trunk()..{b}\") }\n}\n";
    assert!(check(src).is_empty());
}

#[test]
fn the_builder_and_advice_are_exempt() {
    let src = "fn f(b: &str) -> String { format!(\"trunk()..{b}\") }";
    assert!(violations(BUILDER, &production_tokens(src)).is_empty());
    assert!(violations(ADVICE[0], &production_tokens(src)).is_empty());
}
