//! The instruction-file budget.
//!
//! Every `AGENTS.md` and `CLAUDE.md` is injected whole into every agent session, and Claude Code
//! warns once the files it loads pass 150,000 characters in all. jjpr's AGENTS.md had reached
//! 41 KB with nothing to stop it. So an instruction file stays under 24 KB, the detail lives in
//! `docs/agents/<topic>.md` files under 8 KB that are read when their moment comes, and AGENTS.md
//! names every one of them. A topic file nobody names is never read; a name with no file behind
//! it is worse, because nothing says it is missing.
//!
//! A file already over the limit is pinned at its size: it may shrink, never grow, and a shrink
//! lowers the pin in the same change, so it cannot grow back. A pin for a file under the limit,
//! or for one that no longer exists, fails too, so the pin drops when the file gets there.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

const INSTRUCTIONS_LIMIT: usize = 24 * 1024;
const TOPIC_LIMIT: usize = 8 * 1024;
const TOPICS: &str = "docs/agents";

/// Instruction files over the limit, each pinned at its size in bytes. None today: AGENTS.md
/// was pinned at 41,385 bytes until its detail moved into `docs/agents/`.
fn pinned() -> HashMap<&'static str, usize> {
    HashMap::new()
}

/// What is wrong with the instruction files, given each one's path and size, the pins, the topic
/// files' paths and sizes, and the text of the root AGENTS.md.
fn problems(
    instructions: &[(String, usize)],
    pins: &HashMap<&str, usize>,
    topics: &[(String, usize)],
    index: &str,
) -> Vec<String> {
    let mut found = Vec::new();
    for (path, size) in instructions {
        match pins.get(path.as_str()) {
            Some(&pin) if *size > pin => found.push(format!(
                "{path} is {size} bytes, over its pin of {pin}: move detail into {TOPICS}/"
            )),
            Some(&pin) if *size < pin && *size > INSTRUCTIONS_LIMIT => found.push(format!(
                "{path} shrank to {size} bytes: lower its pin from {pin} to {size}"
            )),
            Some(_) if *size <= INSTRUCTIONS_LIMIT => found.push(format!(
                "{path} is {size} bytes, within {INSTRUCTIONS_LIMIT}: remove its pin"
            )),
            Some(_) => {}
            None if *size > INSTRUCTIONS_LIMIT => found.push(format!(
                "{path} is {size} bytes, over {INSTRUCTIONS_LIMIT}: move detail into {TOPICS}/"
            )),
            None => {}
        }
    }
    for path in pins.keys() {
        if !instructions.iter().any(|(p, _)| p == path) {
            found.push(format!(
                "{path} is pinned but does not exist: remove its pin"
            ));
        }
    }
    for (path, size) in topics {
        if *size > TOPIC_LIMIT {
            found.push(format!(
                "{path} is {size} bytes, over {TOPIC_LIMIT}: split it"
            ));
        }
        if !index.contains(&format!("`{path}`")) {
            found.push(format!(
                "{path} is not named in AGENTS.md, so nothing will read it"
            ));
        }
    }
    let prefix = format!("`{TOPICS}/");
    for named in index.split(&prefix).skip(1) {
        let Some(end) = named.find('`') else {
            continue;
        };
        if !named[..end].ends_with(".md") {
            continue;
        }
        let path = format!("{TOPICS}/{}", &named[..end]);
        if !topics.iter().any(|(p, _)| *p == path) {
            found.push(format!("AGENTS.md names {path}, which does not exist"));
        }
    }
    found.sort();
    found
}

fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, usize)>, keep: &dyn Fn(&Path) -> bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if !matches!(
                name.as_ref(),
                "target" | ".jj" | ".git" | "corpus" | "artifacts" | "node_modules"
            ) {
                walk(&path, root, out, keep);
            }
        } else if keep(&path) {
            let size = std::fs::metadata(&path).map_or(0, |m| m.len() as usize);
            let relative = path.strip_prefix(root).unwrap_or(&path);
            out.push((relative.to_string_lossy().into_owned(), size));
        }
    }
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn instruction_files_stay_within_budget_and_every_topic_is_named() {
    let root = root();
    let mut instructions = Vec::new();
    walk(&root, &root, &mut instructions, &|p| {
        matches!(
            p.file_name().and_then(|n| n.to_str()),
            Some("AGENTS.md" | "CLAUDE.md")
        )
    });
    let mut topics = Vec::new();
    walk(&root.join(TOPICS), &root, &mut topics, &|p| {
        p.extension().is_some_and(|e| e == "md")
    });
    assert!(
        instructions.iter().any(|(p, _)| p == "AGENTS.md"),
        "found no root AGENTS.md; the walk is looking in the wrong place"
    );
    assert!(!topics.is_empty(), "found no {TOPICS} files");
    let index = std::fs::read_to_string(root.join("AGENTS.md")).expect("read AGENTS.md");
    let found = problems(&instructions, &pinned(), &topics, &index);
    assert!(found.is_empty(), "{}", found.join("\n"));
}

fn one(path: &str, size: usize) -> Vec<(String, usize)> {
    vec![(path.to_string(), size)]
}

#[test]
fn an_unpinned_instruction_file_over_the_limit_is_refused() {
    let none = HashMap::new();
    let found = problems(&one("AGENTS.md", INSTRUCTIONS_LIMIT + 1), &none, &[], "");
    assert_eq!(found.len(), 1);
    assert!(found[0].contains("over 24576"), "{found:?}");
    assert!(problems(&one("AGENTS.md", INSTRUCTIONS_LIMIT), &none, &[], "").is_empty());
}

#[test]
fn a_pinned_file_may_not_grow_and_a_shrink_lowers_the_pin() {
    let pins = HashMap::from([("AGENTS.md", 30_000)]);
    assert!(problems(&one("AGENTS.md", 30_000), &pins, &[], "").is_empty());
    let grew = problems(&one("AGENTS.md", 30_001), &pins, &[], "");
    assert!(grew[0].contains("over its pin of 30000"), "{grew:?}");
    let shrank = problems(&one("AGENTS.md", 29_000), &pins, &[], "");
    assert!(shrank[0].contains("lower its pin"), "{shrank:?}");
}

#[test]
fn a_pin_drops_once_the_file_is_within_the_limit_or_gone() {
    let pins = HashMap::from([("AGENTS.md", 30_000)]);
    let within = problems(&one("AGENTS.md", INSTRUCTIONS_LIMIT), &pins, &[], "");
    assert!(within[0].contains("remove its pin"), "{within:?}");
    let gone = problems(&[], &pins, &[], "");
    assert!(gone[0].contains("does not exist"), "{gone:?}");
}

#[test]
fn a_topic_over_the_limit_or_unnamed_is_refused() {
    let none = HashMap::new();
    let path = "docs/agents/fuzzing.md";
    let named = "see `docs/agents/fuzzing.md`";
    assert!(problems(&[], &none, &one(path, TOPIC_LIMIT), named).is_empty());
    let big = problems(&[], &none, &one(path, TOPIC_LIMIT + 1), named);
    assert!(big.iter().any(|p| p.contains("split it")), "{big:?}");
    let unnamed = problems(&[], &none, &one(path, 10), "fuzzing.md is mentioned bare");
    assert!(
        unnamed.iter().any(|p| p.contains("not named")),
        "{unnamed:?}"
    );
}

#[test]
fn a_name_with_no_file_behind_it_is_refused() {
    let found = problems(
        &[],
        &HashMap::new(),
        &[],
        "read `docs/agents/gone.md` first",
    );
    assert_eq!(
        found,
        vec!["AGENTS.md names docs/agents/gone.md, which does not exist"]
    );
}
