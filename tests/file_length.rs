// @template-sha256: 3e59cb772645dd540e20678d9fae0054672fc7a15bc594363eb79c3f4e087b77
//! File-length quality ratchet.
//! Per-repo configuration lives in `tests/ratchets.toml`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[rustfmt::skip]
struct Config {
    rust_roots: Vec<String>,
    rust_limit: usize,
    rust_pinned: BTreeMap<String, usize>,
    shell_limit: usize,
    shell_scripts: Vec<String>,
    shell_dirs: Vec<String>,
    shell_pinned: BTreeMap<String, usize>,
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
        "rust_roots" => cfg.rust_roots = items,
        "shell_scripts" => cfg.shell_scripts = items,
        "shell_dirs" => cfg.shell_dirs = items,
        _ => {}
    }
}

#[rustfmt::skip]
fn load_config(root: &Path) -> Config {
    let mut cfg = Config {
        rust_roots: vec!["src".to_string()],
        rust_limit: 400,
        rust_pinned: BTreeMap::new(),
        shell_limit: 200,
        shell_scripts: Vec::new(),
        shell_dirs: Vec::new(),
        shell_pinned: BTreeMap::new(),
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
                    "rust_roots" | "shell_scripts" | "shell_dirs" => {
                        apply_list_key(&mut cfg, key, parse_string_list(val));
                    }
                    "rust_limit" => cfg.rust_limit = val.parse().expect("valid rust_limit"),
                    "shell_limit" => cfg.shell_limit = val.parse().expect("valid shell_limit"),
                    _ => {}
                }
            }
        } else {
            let ceiling: usize = val.parse().expect("valid pinned ceiling");
            match section.as_str() {
                "rust_pinned" => {
                    cfg.rust_pinned.insert(key.to_string(), ceiling);
                }
                "shell_pinned" => {
                    cfg.shell_pinned.insert(key.to_string(), ceiling);
                }
                _ => {}
            }
        }
    }
    cfg
}

#[rustfmt::skip]
fn scrub_block_comment(b: &[u8], mut i: usize, out: &mut String) -> usize {
    out.push_str("  ");
    i += 2;
    let mut depth = 1usize;
    while i < b.len() && depth > 0 {
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            depth += 1;
            out.push_str("  ");
            i += 2;
        } else if b[i] == b'*' && i + 1 < b.len() && b[i + 1] == b'/' {
            depth -= 1;
            out.push_str("  ");
            i += 2;
        } else {
            out.push(if b[i] == b'\n' { '\n' } else { ' ' });
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
    for _ in i..=j {
        out.push(' ');
    }
    let mut k = j + 1;
    while k < b.len() {
        if b[k] == b'"' && k + hashes < b.len() && b[k + 1..=k + hashes].iter().all(|&x| x == b'#')
        {
            for _ in 0..=hashes {
                out.push(' ');
            }
            return Some(k + hashes + 1);
        }
        out.push(if b[k] == b'\n' { '\n' } else { ' ' });
        k += 1;
    }
    Some(k)
}

#[rustfmt::skip]
fn scrub_rust_literals_and_comments(source: &str) -> Vec<String> {
    let b = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                out.push(' ');
                i += 1;
            }
        } else if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i = scrub_block_comment(b, i, &mut out);
        } else if b[i] == b'\'' && i + 2 < b.len() && b[i + 1] != b'\\' && b[i + 2] == b'\'' {
            out.push_str("   ");
            i += 3;
        } else if b[i] == b'\'' && i + 3 < b.len() && b[i + 1] == b'\\' {
            out.push_str("   ");
            i += 3;
            while i < b.len() && b[i] != b'\n' {
                out.push(' ');
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
            out.push(' ');
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' && i + 1 < b.len() {
                    out.push(' ');
                    out.push(if b[i + 1] == b'\n' { '\n' } else { ' ' });
                    i += 2;
                } else if b[i] == b'"' {
                    out.push(' ');
                    i += 1;
                    break;
                } else {
                    out.push(if b[i] == b'\n' { '\n' } else { ' ' });
                    i += 1;
                }
            }
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
    out.split('\n').map(str::to_string).collect()
}

#[rustfmt::skip]
fn is_test_attr_line(scrubbed: &str) -> bool {
    let compact: String = scrubbed.chars().filter(|c| !c.is_whitespace()).collect();
    compact.contains("#[cfg(test)]") || compact.contains("#[test]") || compact.contains("::test]")
}

#[rustfmt::skip]
fn analyze_source(source: &str) -> (usize, Vec<(String, Option<String>)>) {
    if source.is_empty() {
        return (0, Vec::new());
    }
    let raw_lines: Vec<&str> = source.lines().collect();
    let scrubbed = scrub_rust_literals_and_comments(source);
    let n = raw_lines.len();
    let mut excluded = vec![false; n];
    let mut out_mods = Vec::new();
    let mut idx = 0;
    while idx < n {
        if !is_test_attr_line(&scrubbed[idx]) {
            idx += 1;
            continue;
        }
        let mut start = idx;
        while start > 0 {
            let (prev_raw, prev_scrub) = (raw_lines[start - 1].trim(), scrubbed[start - 1].trim());
            if prev_raw.starts_with("///")
                || (prev_scrub.starts_with("#[") && prev_scrub.ends_with(']'))
            {
                start -= 1;
            } else {
                break;
            }
        }
        let (mut depth, mut saw_brace, mut in_attr, mut end) = (0i32, false, false, idx);
        'scan: for (k, line) in scrubbed.iter().enumerate().skip(idx).take(n - idx) {
            end = k;
            for ch in line.chars() {
                if ch == '[' && depth == 0 && !saw_brace {
                    in_attr = true;
                } else if ch == ']' && in_attr {
                    in_attr = false;
                } else if !in_attr {
                    if ch == '{' {
                        depth += 1;
                        saw_brace = true;
                    } else if ch == '}' && saw_brace {
                        depth -= 1;
                        if depth <= 0 {
                            break 'scan;
                        }
                    } else if ch == ';' && !saw_brace && depth == 0 {
                        break 'scan;
                    }
                }
            }
        }
        if !saw_brace {
            let mut path_attr = None;
            for raw in &raw_lines[start..=end] {
                let t = raw.trim();
                if t.starts_with("#[path")
                    && let Some((_, q)) = t.split_once('"')
                    && let Some((p, _)) = q.split_once('"')
                {
                    path_attr = Some(p.to_string());
                }
            }
            for sc in &scrubbed[idx..=end] {
                for tok in sc.split_whitespace().collect::<Vec<_>>().windows(2) {
                    if tok[0] == "mod"
                        && let Some(name) = tok[1].strip_suffix(';')
                    {
                        out_mods.push((name.to_string(), path_attr.clone()));
                    }
                }
            }
        }
        for flag in &mut excluded[start..=end] {
            *flag = true;
        }
        idx = end + 1;
    }
    (excluded.into_iter().filter(|&ex| !ex).count(), out_mods)
}

#[rustfmt::skip]
fn production_lines(source: &str) -> usize {
    analyze_source(source).0
}

#[rustfmt::skip]
fn module_files(file: &Path, name: &str, path_attr: Option<&str>) -> Vec<PathBuf> {
    let dir = file.parent().unwrap_or(Path::new(""));
    if let Some(p) = path_attr {
        return vec![dir.join(p)];
    }
    let owns_dir =
        matches!(file.file_name().and_then(|n| n.to_str()), Some("mod.rs" | "lib.rs" | "main.rs"));
    let base =
        if owns_dir { dir.to_path_buf() } else { dir.join(file.file_stem().unwrap_or_default()) };
    vec![base.join(format!("{name}.rs")), base.join(name).join("mod.rs")]
}

#[rustfmt::skip]
fn children_dir(module_file: &Path) -> PathBuf {
    if module_file.file_name().is_some_and(|n| n == "mod.rs") {
        module_file.parent().unwrap_or(Path::new("")).to_path_buf()
    } else {
        module_file.with_extension("")
    }
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
fn check_ratchet(
    rel: &str,
    lines: usize,
    limit: usize,
    pinned: &BTreeMap<String, usize>,
    violations: &mut Vec<String>,
) {
    if let Some(&ceiling) = pinned.get(rel) {
        if lines > ceiling {
            violations.push(format!(
                "{rel}: {lines} lines, up from its pinned {ceiling} (limit {limit})"
            ));
        } else if lines <= limit {
            violations.push(format!(
                "{rel}: down to {lines} lines (<= {limit}); remove from tests/ratchets.toml"
            ));
        } else if lines < ceiling {
            violations.push(format!(
                "{rel}: down to {lines} lines from its pinned {ceiling}; lower its pin to {lines}"
            ));
        }
    } else if lines > limit {
        violations.push(format!("{rel}: {lines} lines, over the {limit}-line limit"));
    }
}

#[rustfmt::skip]
#[test]
fn rust_files_stay_within_line_limit() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let cfg = load_config(root);
    let mut files = Vec::new();
    for rel in &cfg.rust_roots {
        let before = files.len();
        walk_files(&root.join(rel), &mut files);
        files.retain(|p| p.extension().and_then(|e| e.to_str()) == Some("rs"));
        assert!(files.len() > before, "found no .rs files under {rel}");
    }
    let mut test_roots = Vec::new();
    for file in &files {
        let source = fs::read_to_string(file).expect("read source file");
        for (name, path_attr) in analyze_source(&source).1 {
            test_roots.extend(module_files(file, &name, path_attr.as_deref()));
        }
    }
    files.retain(|f| !test_roots.iter().any(|r| f == r || f.starts_with(children_dir(r))));

    let mut violations = Vec::new();
    for (pinned_path, &ceiling) in &cfg.rust_pinned {
        if !root.join(pinned_path).exists() {
            violations.push(format!(
                "{pinned_path}: pinned at {ceiling} in tests/ratchets.toml, but file does not exist"
            ));
        }
    }
    for file in &files {
        let rel = file.strip_prefix(root).unwrap_or(file).to_string_lossy().replace('\\', "/");
        let source = fs::read_to_string(file).expect("read source file");
        check_ratchet(
            &rel,
            production_lines(&source),
            cfg.rust_limit,
            &cfg.rust_pinned,
            &mut violations,
        );
    }
    assert!(
        violations.is_empty(),
        "Rust file-length ratchet failed:\n  {}",
        violations.join("\n  ")
    );
}

#[rustfmt::skip]
#[test]
fn shell_scripts_stay_within_line_limit() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let cfg = load_config(root);
    let mut violations = Vec::new();
    for (pinned_path, &ceiling) in &cfg.shell_pinned {
        if !root.join(pinned_path).exists() {
            violations.push(format!(
                "{pinned_path}: pinned at {ceiling} in tests/ratchets.toml, but file does not exist"
            ));
        }
    }
    for rel in &cfg.shell_scripts {
        match fs::read_to_string(root.join(rel)) {
            Ok(text) => check_ratchet(
                rel,
                text.lines().count(),
                cfg.shell_limit,
                &cfg.shell_pinned,
                &mut violations,
            ),
            Err(err) => violations.push(format!("{rel}: cannot read ({err})")),
        }
    }
    for d in &cfg.shell_dirs {
        let mut found = Vec::new();
        walk_files(&root.join(d), &mut found);
        found.retain(|p| p.extension().is_none_or(|e| e == "sh"));
        assert!(!found.is_empty(), "found no shell scripts under {d}");
        for path in found {
            let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
            let lines = fs::read_to_string(&path).expect("read script").lines().count();
            check_ratchet(&rel, lines, cfg.shell_limit, &cfg.shell_pinned, &mut violations);
        }
    }
    assert!(
        violations.is_empty(),
        "Shell file-length ratchet failed:\n  {}",
        violations.join("\n  ")
    );
}

#[rustfmt::skip]
#[test]
fn only_test_items_are_left_out_of_the_count() {
    assert_eq!(
        production_lines("fn a() {}\nfn b() {}\n#[cfg(test)]\nmod tests {\n // lots\n}\n"),
        2
    );
    assert_eq!(
        production_lines("fn a() {}\n#[cfg(test)]\nfn helper() {}\nfn b() {}\nfn c() {}\n"),
        3
    );
    assert_eq!(
        production_lines(
            "#[cfg(test)]\nmod tests {\n    const FIX: &str = \"\n}\n\";\n    let q = '\\'';\n}\nfn b() {}\n"
        ),
        1
    );
    assert_eq!(
        production_lines(
            "// #[cfg(test)]\nconst A: &str = \"\n#[cfg(test)]\nmod x {\";\nfn b() {}\n"
        ),
        5
    );
    assert_eq!(
        analyze_source(
            "#[cfg(test)]\n#[path = \"x_tests.rs\"]\npub(crate) mod tests;\nfn a() {}\n"
        )
        .1,
        vec![("tests".to_string(), Some("x_tests.rs".to_string()))]
    );
}

#[rustfmt::skip]
#[test]
fn the_ratchet_holds_a_pinned_file_to_its_size() {
    let pins = BTreeMap::from([("a.rs".to_string(), 500usize)]);
    let check = |f: &str, n: usize| {
        let mut v = Vec::new();
        check_ratchet(f, n, 400, &pins, &mut v);
        v
    };
    assert!(check("a.rs", 500).is_empty());
    assert!(check("a.rs", 501)[0].contains("up from its pinned 500"));
    assert!(check("a.rs", 450)[0].contains("lower its pin to 450"));
    assert!(check("a.rs", 400)[0].contains("remove from tests/ratchets.toml"));
    assert!(check("b.rs", 400).is_empty());
    assert!(check("b.rs", 401)[0].contains("over the 400-line limit"));
}
