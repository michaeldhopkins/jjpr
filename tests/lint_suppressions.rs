// @template-sha256: 558f4df0296f9f9deb308ce4a43221acdf0194ff28bfaab95169e6a609a54641
//! Lint-suppression and baseline quality ratchet.
//! Per-repo configuration lives in `tests/ratchets.toml`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[rustfmt::skip]
struct Config {
    lint_roots: Vec<String>,
    suppressions: usize,
    forbid_unsafe_in: Vec<String>,
    check_baseline_lints: bool,
    pinned_thresholds: BTreeMap<String, i64>,
    shell_dirs: Vec<String>,
    shell_suppressions: usize,
}

#[rustfmt::skip]
fn strip_toml_comment(line: &str) -> &str {
    let mut in_quotes = false;
    for (idx, ch) in line.char_indices() {
        if ch == '"' {
            in_quotes = !in_quotes;
        } else if ch == '#' && !in_quotes {
            return &line[..idx];
        }
    }
    line
}

#[rustfmt::skip]
fn parse_string_list(raw: &str) -> Vec<String> {
    let (mut out, mut cur, mut in_q) = (Vec::new(), String::new(), false);
    for ch in raw.chars() {
        if ch == '"' {
            if in_q {
                out.push(std::mem::take(&mut cur));
            }
            in_q = !in_q;
        } else if in_q {
            cur.push(ch);
        }
    }
    out
}

#[rustfmt::skip]
fn apply_list_key(cfg: &mut Config, key: &str, items: Vec<String>) {
    match key {
        "lint_roots" => cfg.lint_roots = items,
        "forbid_unsafe_in" => cfg.forbid_unsafe_in = items,
        "shell_dirs" => cfg.shell_dirs = items,
        _ => {}
    }
}

#[rustfmt::skip]
fn load_config(root: &Path) -> Config {
    let mut cfg = Config {
        lint_roots: vec!["src".to_string(), "tests".to_string()],
        suppressions: 0,
        forbid_unsafe_in: Vec::new(),
        check_baseline_lints: false,
        pinned_thresholds: BTreeMap::new(),
        shell_dirs: Vec::new(),
        shell_suppressions: 0,
    };
    let Ok(raw) = fs::read_to_string(root.join("tests/ratchets.toml")) else {
        return cfg;
    };
    let (mut section, mut pending_key, mut pending_arr) =
        (String::new(), None::<String>, String::new());
    for raw_line in raw.lines() {
        let line = strip_toml_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(key) = &pending_key {
            pending_arr.push(' ');
            pending_arr.push_str(line);
            if line.contains(']') {
                apply_list_key(&mut cfg, key, parse_string_list(&pending_arr));
                pending_key = None;
                pending_arr.clear();
            }
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (key, val) = (k.trim().trim_matches('"'), v.trim());
        if section.is_empty() {
            if val.starts_with('[') && !val.contains(']') {
                pending_key = Some(key.to_string());
                pending_arr = val.to_string();
            } else {
                match key {
                    "lint_roots" | "forbid_unsafe_in" | "shell_dirs" => {
                        apply_list_key(&mut cfg, key, parse_string_list(val));
                    }
                    "suppressions" => cfg.suppressions = val.parse().expect("valid suppressions"),
                    "shell_suppressions" => {
                        cfg.shell_suppressions = val.parse().expect("valid shell_suppressions")
                    }
                    "check_baseline_lints" => cfg.check_baseline_lints = val == "true",
                    _ => {}
                }
            }
        } else if section == "pinned_thresholds" {
            let ceiling: i64 = val.parse().expect("valid threshold in [pinned_thresholds]");
            cfg.pinned_thresholds.insert(key.to_string(), ceiling);
        }
    }
    cfg
}

#[rustfmt::skip]
fn scrub_block_comment(b: &[u8], mut i: usize, out: &mut String) -> usize {
    i += 2;
    let mut depth = 1usize;
    while i < b.len() && depth > 0 {
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            depth += 1;
            i += 2;
        } else if b[i] == b'*' && i + 1 < b.len() && b[i + 1] == b'/' {
            depth -= 1;
            i += 2;
        } else {
            if b[i] == b'\n' {
                out.push('\n');
            }
            i += 1;
        }
    }
    i
}

#[rustfmt::skip]
fn scrub_raw_string(b: &[u8], i: usize, out: &mut String) -> Option<usize> {
    let mut hashes = 0usize;
    let mut j = i + 1;
    while j < b.len() && b[j] == b'#' {
        hashes += 1;
        j += 1;
    }
    if j >= b.len() || b[j] != b'"' {
        return None;
    }
    let mut k = j + 1;
    while k < b.len() {
        if b[k] == b'"' && k + hashes < b.len() && b[k + 1..=k + hashes].iter().all(|&x| x == b'#')
        {
            return Some(k + hashes + 1);
        }
        if b[k] == b'\n' {
            out.push('\n');
        }
        k += 1;
    }
    Some(k)
}

#[rustfmt::skip]
fn scrub_rust_literals_and_comments(source: &str) -> String {
    let b = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i = scrub_block_comment(b, i, &mut out);
        } else if b[i] == b'\'' && i + 2 < b.len() && b[i + 1] != b'\\' && b[i + 2] == b'\'' {
            i += 3;
        } else if b[i] == b'\'' && i + 3 < b.len() && b[i + 1] == b'\\' {
            i += 3;
            while i < b.len() && b[i] != b'\n' {
                if b[i] == b'\'' {
                    i += 1;
                    break;
                }
                i += 1;
            }
        } else if b[i] == b'r'
            && i + 1 < b.len()
            && (b[i + 1] == b'#' || b[i + 1] == b'"')
            && let Some(next) = scrub_raw_string(b, i, &mut out)
        {
            i = next;
        } else if b[i] == b'"' {
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' && i + 1 < b.len() {
                    if b[i + 1] == b'\n' {
                        out.push('\n');
                    }
                    i += 2;
                } else if b[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    if b[i] == b'\n' {
                        out.push('\n');
                    }
                    i += 1;
                }
            }
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
    out
}

#[rustfmt::skip]
fn suppressions_in(source: &str) -> usize {
    let scrubbed = scrub_rust_literals_and_comments(source);
    let compact: String = scrubbed.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = compact.as_bytes();
    let mut count = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' {
            let mut j = i + 1;
            if j < bytes.len() && bytes[j] == b'!' {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'[' {
                let mut depth = 1usize;
                let start = j + 1;
                j += 1;
                while j < bytes.len() && depth > 0 {
                    if bytes[j] == b'[' {
                        depth += 1;
                    } else if bytes[j] == b']' {
                        depth -= 1;
                    }
                    j += 1;
                }
                let attr = &compact[start..j.saturating_sub(1)];
                if attr.starts_with("allow(")
                    || attr.starts_with("expect(")
                    || (attr.starts_with("cfg_attr(")
                        && (attr.contains(",allow(") || attr.contains(",expect(")))
                {
                    count += 1;
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }
    count
}

#[rustfmt::skip]
fn suppression_verdict(count: usize, pinned: usize) -> Option<String> {
    if count > pinned {
        Some(format!("Rust lint suppressions grew from {pinned} to {count}"))
    } else if count < pinned {
        Some(format!(
            "Rust lint suppressions dropped from {pinned} to {count}; lower `suppressions = {count}` in tests/ratchets.toml"
        ))
    } else {
        None
    }
}

#[rustfmt::skip]
fn parse_cargo_lints(cargo_toml: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut in_lints = false;
    for raw in cargo_toml.lines() {
        let line = strip_toml_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let sec = line[1..line.len() - 1].trim();
            in_lints = matches!(
                sec,
                "lints.clippy" | "lints.rust" | "workspace.lints.clippy" | "workspace.lints.rust"
            );
            continue;
        }
        if !in_lints {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim().replace('-', "_");
        let val = v.trim();
        let level = if val.starts_with('{') {
            val.split("level").nth(1).and_then(|s| s.split('"').nth(1)).unwrap_or("")
        } else {
            val.trim_matches('"')
        };
        if !level.is_empty() {
            out.insert(key, level.to_string());
        }
    }
    out
}

#[rustfmt::skip]
fn baseline_verdicts(levels: &BTreeMap<String, String>) -> Vec<String> {
    let strength = |l: &str| ["allow", "warn", "deny", "forbid"].iter().position(|&x| x == l);
    let required = [
        ("struct_excessive_bools", "deny"),
        ("fn_params_excessive_bools", "deny"),
        ("collapsible_if", "deny"),
        ("too_many_arguments", "deny"),
        ("needless_range_loop", "deny"),
        ("extend_with_drain", "deny"),
        ("unwrap_used", "deny"),
        ("match_bool", "warn"),
        ("bool_to_int_with_if", "warn"),
        ("cognitive_complexity", "warn"),
        ("too_many_lines", "warn"),
    ];
    let mut failures: Vec<String> = required
        .iter()
        .filter(|&&(name, want)| levels.get(name).and_then(|got| strength(got)) < strength(want))
        .map(|&(name, want)| format!("`{name}` must be at least \"{want}\" in Cargo.toml [lints]"))
        .collect();
    let unsafe_ok = levels.get("unsafe_code").and_then(|g| strength(g)) >= strength("forbid")
        || levels.get("undocumented_unsafe_blocks").and_then(|g| strength(g)) >= strength("deny");
    if !unsafe_ok {
        failures.push("Cargo.toml [lints] must set `unsafe_code = \"forbid\"` or `undocumented_unsafe_blocks = \"deny\"`".to_string());
    }
    failures
}

#[rustfmt::skip]
fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for path in paths {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if matches!(
            name,
            "target"
                | ".git"
                | ".jj"
                | "node_modules"
                | "corpus"
                | "artifacts"
                | "coverage"
                | "new"
                | "workflows"
        ) {
            continue;
        }
        if path.is_dir() {
            walk_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[rustfmt::skip]
#[test]
fn lint_suppression_count_matches_pinned_ratchet() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let cfg = load_config(root);
    let mut files = Vec::new();
    for rel in &cfg.lint_roots {
        let before = files.len();
        walk_files(&root.join(rel), &mut files);
        files.retain(|p| p.extension().and_then(|e| e.to_str()) == Some("rs"));
        assert!(files.len() > before, "found no .rs files under {rel}");
    }
    let total: usize =
        files.iter().map(|f| suppressions_in(&fs::read_to_string(f).expect("read rs file"))).sum();
    if let Some(msg) = suppression_verdict(total, cfg.suppressions) {
        panic!("{msg}");
    }
    if !cfg.shell_dirs.is_empty() {
        let mut sh_files = Vec::new();
        for rel in &cfg.shell_dirs {
            let before = sh_files.len();
            walk_files(&root.join(rel), &mut sh_files);
            sh_files.retain(|p| p.extension().is_none_or(|e| e == "sh"));
            assert!(sh_files.len() > before, "found no shell scripts under {rel}");
        }
        let sh_total: usize = sh_files
            .iter()
            .map(|p| {
                fs::read_to_string(p)
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| {
                        let t = l.trim_start();
                        t.starts_with('#') && t.contains("shellcheck") && t.contains("disable=")
                    })
                    .count()
            })
            .sum();
        assert_eq!(sh_total, cfg.shell_suppressions, "shellcheck disable count changed");
    }
}

#[rustfmt::skip]
#[test]
fn clippy_thresholds_and_baseline_lints_hold() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let cfg = load_config(root);
    for rel in &cfg.forbid_unsafe_in {
        let content = fs::read_to_string(root.join(rel)).expect("read forbid_unsafe_in file");
        assert!(
            content.lines().any(|l| l.trim() == "#![forbid(unsafe_code)]"),
            "{rel} must contain `#![forbid(unsafe_code)]`"
        );
    }
    if let Ok(clippy_toml) = fs::read_to_string(root.join("clippy.toml")) {
        let defaults = [
            ("too-many-arguments-threshold", 7i64),
            ("cognitive-complexity-threshold", 25),
            ("too-many-lines-threshold", 100),
            ("type-complexity-threshold", 250),
            ("max-struct-bools", 3),
            ("max-fn-params-bools", 3),
        ];
        let switches = ["allow-unwrap-in-tests", "allow-expect-in-tests", "allow-dbg-in-tests"];
        let mut seen = BTreeMap::new();
        for raw in clippy_toml.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let (key, val) = (k.trim(), v.trim());
            if switches.contains(&key) {
                continue;
            }
            let parsed: i64 =
                val.parse().unwrap_or_else(|_| panic!("non-integer clippy.toml key {key} = {val}"));
            seen.insert(key.to_string(), parsed);
            let Some(&(_, default_val)) = defaults.iter().find(|&&(dk, _)| dk == key) else {
                panic!("unknown clippy.toml threshold `{key}`");
            };
            if let Some(&pinned) = cfg.pinned_thresholds.get(key) {
                assert!(parsed <= pinned, "{key}: {parsed} exceeds pinned {pinned}");
                assert!(
                    parsed > default_val,
                    "{key}: {parsed} <= default {default_val}; remove from [pinned_thresholds]"
                );
                assert_eq!(
                    parsed, pinned,
                    "{key}: down to {parsed} from {pinned}; lower pin in tests/ratchets.toml"
                );
            } else {
                assert!(parsed <= default_val, "{key}: {parsed} exceeds default {default_val}");
            }
        }
        for key in cfg.pinned_thresholds.keys() {
            assert!(
                seen.contains_key(key),
                "`{key}` is pinned in tests/ratchets.toml but not set in clippy.toml"
            );
        }
    }
    if cfg.check_baseline_lints {
        let cargo_toml = fs::read_to_string(root.join("Cargo.toml")).expect("read Cargo.toml");
        let levels = parse_cargo_lints(&cargo_toml);
        let failures = baseline_verdicts(&levels);
        assert!(failures.is_empty(), "Baseline lint check failed:\n  {}", failures.join("\n  "));
    }
}

#[rustfmt::skip]
#[test]
fn suppressions_and_baseline_verdicts_ratchet_correctly() {
    assert_eq!(suppressions_in("#[allow(dead_code)]\nfn a() {}"), 1);
    assert_eq!(suppressions_in("#![expect(clippy::too_many_lines)]"), 1);
    assert_eq!(suppressions_in("#[cfg_attr(test, allow(unused))]\nfn a() {}"), 1);
    assert_eq!(
        suppressions_in("mod m { fn a() { let q = '\\''; #[allow(unused)] let x = 1; } }"),
        1
    );
    assert_eq!(suppressions_in("proptest! { #[allow(unused)] fn p() {} }"), 1);
    assert_eq!(suppressions_in(r##"const S: &str = "#[allow(x)]"; // #[allow(y)]"##), 0);
    assert!(suppression_verdict(2, 2).is_none());
    assert!(suppression_verdict(3, 2).is_some_and(|m| m.contains("grew from 2 to 3")));
    assert!(suppression_verdict(1, 2).is_some_and(|m| m.contains("lower `suppressions = 1`")));
    let weak = parse_cargo_lints("[lints.clippy]\nunwrap_used = \"warn\"\n");
    assert!(
        baseline_verdicts(&weak)
            .iter()
            .any(|m| m.contains("`unwrap_used` must be at least \"deny\""))
    );
}
