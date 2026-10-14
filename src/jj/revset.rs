//! Every revset jjpr hands to jj, and every bookmark name it passes on jj's
//! command line, is built here.
//!
//! A bookmark name is not a revset. Written bare, `foo@origin` is the remote
//! bookmark `foo` on `origin`, `a|b` is a union, and `x-` is a parent. Git
//! allows all of those characters in a branch name. So a name always goes in as
//! a jj string literal, `"foo@origin"`, which jj resolves as one symbol, the
//! way it resolves a bare simple name: a tag of that name first, then the local
//! bookmark (every side of it, when conflicted), then a commit or change id.
//! `tests/revset_quoting.rs` fails when a revset is formatted anywhere else.

/// The trunk revset, which jj and the user's config define.
pub const TRUNK: &str = "trunk()";

/// `s` as a jj string literal: double-quoted, with `\` and `"` escaped and
/// control characters written as escapes, so nothing in `s` can end the
/// literal. Accepted by every jj jjpr supports (0.36 and later).
pub fn string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\0' => out.push_str("\\0"),
            c if c.is_ascii_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The revset naming `name` as a symbol (a tag, bookmark, or commit or change
/// id, as above) of exactly that name, whatever characters it holds. Also how jj's
/// `bookmark set`/`create` read a name.
pub fn symbol(name: &str) -> String {
    string_literal(name)
}

/// The string pattern matching exactly `name`, for the commands that take a
/// pattern rather than a name (`bookmark forget`/`delete`, `git push
/// --bookmark`). Bare, those match a glob, so `feat*` would reach every
/// bookmark starting `feat`.
pub fn exact_name(name: &str) -> String {
    format!("exact:{}", string_literal(name))
}

/// Every commit whose change id is `id`, both copies when it is divergent.
/// (A bare change id is a symbol, which jj refuses to resolve when divergent.)
pub fn change_id(id: &str) -> String {
    format!("change_id({})", string_literal(id))
}

/// Commits authored by exactly `email`.
pub fn author(email: &str) -> String {
    format!("author(exact:{})", string_literal(email))
}

/// Commits authored by exactly any of `emails`. Each term is a function call,
/// so the union needs no parentheses.
pub fn any_author(emails: &[String]) -> String {
    let terms: Vec<String> = emails.iter().map(|e| author(e)).collect();
    terms.join(" | ")
}

/// The union of `revsets`, each parenthesized; `None` when there are none.
pub fn union<I, S>(revsets: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let parts: Vec<String> = revsets
        .into_iter()
        .map(|r| format!("({})", r.as_ref()))
        .collect();
    (!parts.is_empty()).then(|| parts.join(" | "))
}

/// `from..to`: ancestors of `to` that are not ancestors of `from`.
pub fn range(from: &str, to: &str) -> String {
    format!("({from})..({to})")
}

/// `from::to`: descendants of `from` that are ancestors of `to`.
pub fn dag_range(from: &str, to: &str) -> String {
    format!("({from})::({to})")
}

/// Commits in `revset` that have conflicts.
pub fn conflicts_in(revset: &str) -> String {
    format!("conflicts() & ({revset})")
}

/// The oldest conflicted commits in `revset`.
pub fn first_conflicts_in(revset: &str) -> String {
    format!("roots({})", conflicts_in(revset))
}

/// `@`, when it is a descendant of `revset` (empty otherwise).
pub fn working_copy_under(revset: &str) -> String {
    format!("@ & descendants({revset})")
}

/// Parents of `root` that are not ancestors of `base`.
pub fn parents_outside(root: &str, base: &str) -> String {
    format!("parents({root}) ~ ::({base})")
}

/// Commits that are children of both `a` and `b`: the merge of the two.
pub fn common_children(a: &str, b: &str) -> String {
    format!("children({a}) & children({b})")
}

/// Ancestors of `revset`, excluding trunk itself.
pub fn ancestors_off_trunk(revset: &str) -> String {
    format!("::({revset}) ~ {TRUNK}")
}

/// `revset` minus trunk.
pub fn off_trunk(revset: &str) -> String {
    format!("({revset}) ~ {TRUNK}")
}

/// The working copy and `owned`.
pub fn working_copy_or(owned: &str) -> String {
    format!("@ | ({owned})")
}

/// The commits to abandon below a merged `head` once the survivor has moved
/// off it: everything from trunk up to `head`, except what a bookmark, the
/// working copy or another line of work still stands on.
pub fn landed_below(head: &str) -> String {
    let landed = format!("({TRUNK}..{})", symbol(head));
    format!("{landed} ~ ::((visible_heads() ~ {landed}) | bookmarks() | @)")
}

/// Local bookmarks named exactly `name` (none, one, or the sides of a conflict).
pub fn local_bookmark_targets(name: &str) -> String {
    format!("bookmarks({})", exact_name(name))
}

/// The remote bookmark `name` on `remote`, matched exactly.
pub fn remote_bookmark_target(name: &str, remote: &str) -> String {
    format!(
        "remote_bookmarks({}, {})",
        exact_name(name),
        exact_name(remote)
    )
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// Read one jj string literal from the start of `s`, the way jj's revset
    /// grammar does: `"` then characters, where `\` starts one of `\" \\ \t \r \n
    /// \0 \e \xHH`, then a closing `"`. Returns the value and what follows it, or
    /// `None` when `s` does not start with a complete literal.
    fn read_literal(s: &str) -> Option<(String, &str)> {
        let mut chars = s.strip_prefix('"')?.char_indices();
        let body = s.strip_prefix('"')?;
        let mut out = String::new();
        while let Some((i, c)) = chars.next() {
            match c {
                '"' => return Some((out, &body[i + 1..])),
                '\\' => match chars.next()?.1 {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'n' => out.push('\n'),
                    '0' => out.push('\0'),
                    'e' => out.push('\x1b'),
                    'x' => {
                        let hex: String = [chars.next()?.1, chars.next()?.1].iter().collect();
                        out.push(char::from(u8::from_str_radix(&hex, 16).ok()?));
                    }
                    _ => return None,
                },
                c => out.push(c),
            }
        }
        None
    }

    #[test]
    fn a_plain_name_is_only_wrapped() {
        assert_eq!(string_literal("main"), r#""main""#);
        assert_eq!(symbol("feature/x"), r#""feature/x""#);
    }

    #[test]
    fn what_would_end_the_literal_is_escaped() {
        assert_eq!(string_literal(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(string_literal("a\tb\nc\rd\0e"), r#""a\tb\nc\rd\0e""#);
        assert_eq!(string_literal("a\x1bb\x7f"), r#""a\x1bb\x7f""#);
    }

    #[test]
    fn a_remote_looking_name_stays_one_symbol() {
        assert_eq!(symbol("foo@origin"), r#""foo@origin""#);
        assert_eq!(symbol("a|b"), r#""a|b""#);
    }

    #[test]
    fn a_pattern_matches_the_name_exactly() {
        assert_eq!(exact_name("feat*"), r#"exact:"feat*""#);
        assert_eq!(local_bookmark_targets("x@y"), r#"bookmarks(exact:"x@y")"#);
        assert_eq!(
            remote_bookmark_target("x", "origin"),
            r#"remote_bookmarks(exact:"x", exact:"origin")"#
        );
    }

    #[test]
    fn composites_parenthesize_their_operands() {
        assert_eq!(range(TRUNK, r#""c1""#), r#"(trunk())..("c1")"#);
        assert_eq!(dag_range("a | b", "c"), "(a | b)::(c)");
        assert_eq!(conflicts_in("a | b"), "conflicts() & (a | b)");
        assert_eq!(first_conflicts_in("x"), "roots(conflicts() & (x))");
        assert_eq!(working_copy_under("x"), "@ & descendants(x)");
        assert_eq!(parents_outside("r", "a | b"), "parents(r) ~ ::(a | b)");
        assert_eq!(common_children("a", "b"), "children(a) & children(b)");
        assert_eq!(ancestors_off_trunk("@"), "::(@) ~ trunk()");
        assert_eq!(off_trunk("mine()"), "(mine()) ~ trunk()");
        assert_eq!(working_copy_or("mine()"), "@ | (mine())");
        assert_eq!(change_id("kx"), r#"change_id("kx")"#);
    }

    #[test]
    fn a_union_of_nothing_is_none() {
        assert_eq!(union(Vec::<String>::new()), None);
        assert_eq!(union(["a", "b | c"]), Some("(a) | (b | c)".to_string()));
    }

    #[test]
    fn the_abandon_revset_reads_as_before() {
        assert_eq!(
            landed_below("c9"),
            r#"(trunk().."c9") ~ ::((visible_heads() ~ (trunk().."c9")) | bookmarks() | @)"#
        );
    }

    #[test]
    fn authors_join_as_before() {
        let emails = vec!["a@x.com".to_string(), r#"b"\c"#.to_string()];
        assert_eq!(
            any_author(&emails),
            r#"author(exact:"a@x.com") | author(exact:"b\"\\c")"#
        );
    }

    proptest! {
        /// Whatever the name, jj reads the literal back as exactly that name, and
        /// nothing after it: no character in a name can end the literal early.
        #[test]
        fn a_literal_reads_back_as_the_name(name in any::<String>()) {
            let quoted = string_literal(&name);
            let (value, rest) = read_literal(&quoted).expect("a complete literal");
            prop_assert_eq!(value, name);
            prop_assert_eq!(rest, "");
        }

        /// Embedded anywhere in a revset, the literal still ends where it began
        /// to: what follows it is the surrounding revset, untouched.
        #[test]
        fn a_literal_ends_where_the_builder_put_its_close(
            name in r#"[a-z"\\@|&~():. \t\n\x00-\x1f\x7f]{0,24}"#,
        ) {
            let built = dag_range(&change_id("kx"), &symbol(&name));
            let start = built.find("::(").expect("dag range") + 3;
            let (value, rest) = read_literal(&built[start..]).expect("a complete literal");
            prop_assert_eq!(value, name);
            prop_assert_eq!(rest, ")");
        }

        /// The output is printable: no raw control character reaches jj's command
        /// line or a log of it.
        #[test]
        fn a_literal_holds_no_raw_control_character(name in any::<String>()) {
            prop_assert!(!string_literal(&name).chars().any(|c| c.is_ascii_control()));
        }
    }
}
