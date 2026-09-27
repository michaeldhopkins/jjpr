//! jjpr's jj templates and parsers against real output from every jj version captured
//! under `tests/fixtures/jj/<version>/`, one test per version, so a regression names the
//! version it broke.
//!
//! The fixtures are captured, not written: `capture_fixtures` (ignored by default) drives
//! a given jj binary through a scratch repository with a pushed trunk, a stack, bookmarks
//! of every kind (synced, moved ahead of their remote, never pushed, deleted locally but
//! still tracked on the remote, conflicted), a rename and a divergent change, and saves
//! what jjpr's own commands print. To add a version:
//!
//! ```sh
//! JJPR_CAPTURE_JJ=/path/to/jj cargo test --test jj_compat capture_fixtures -- --ignored
//! ```
//!
//! then add a line to `version_tests!` below. Change and commit ids differ per capture;
//! the assertions are about structure and names, never ids.
//!
//! These cover the parsers. The commands themselves (revsets, flags) are exercised by
//! the rest of the suite against whichever jj is on `PATH`; CI's `jj-versions` job runs
//! it against each release captured here.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use jjpr::jj::templates::{
    BOOKMARK_TEMPLATE, LOG_TEMPLATE, parse_bookmark_output, parse_default_branch, parse_log_output,
    parse_remote_list,
};
use jjpr::jj::version::{parse_jj_version, push_new_bookmark_args};

const EMAIL: &str = "test@example.com";

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/jj")
}

// --- capture --------------------------------------------------------------------

struct Capture {
    jj: PathBuf,
    config: PathBuf,
    repo: PathBuf,
    stderr: String,
}

impl Capture {
    fn run(&self, args: &[&str]) -> Output {
        Command::new(&self.jj)
            .args(args)
            .current_dir(&self.repo)
            .env("JJ_CONFIG", &self.config)
            .output()
            .expect("run jj")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "jj {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("jj printed UTF-8")
    }

    /// A command jjpr itself runs: its stdout is the fixture, and any warning it
    /// prints (a deprecation, say) is kept in `stderr.txt`. jj's hints are not.
    fn read(&mut self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "jj {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("Warning") {
            self.stderr
                .push_str(&format!("$ jj {}\n{stderr}", args.join(" ")));
        }
        String::from_utf8(out.stdout).expect("jj printed UTF-8")
    }

    /// The first spelling this jj accepts: pattern syntax moved across versions.
    fn first_ok(&self, spellings: &[&[&str]]) {
        let mut errors = Vec::new();
        for args in spellings {
            let out = self.run(args);
            if out.status.success() {
                return;
            }
            errors.push(format!(
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        panic!("no spelling worked:\n{}", errors.join("\n"));
    }

    fn one_line(&self, args: &[&str]) -> String {
        self.ok(args).trim().to_string()
    }

    fn op_id(&self) -> String {
        self.one_line(&[
            "op",
            "log",
            "-n1",
            "--no-graph",
            "-T",
            "id",
            "--ignore-working-copy",
        ])
    }

    fn commit_id(&self, rev: &str) -> String {
        self.one_line(&["log", "-r", rev, "--no-graph", "-T", "commit_id"])
    }

    fn push(&self, push_args: &[&str], bookmarks: &[&str]) {
        let mut args = push_args.to_vec();
        args.extend(["git", "push", "--remote", "origin"]);
        for b in bookmarks {
            args.extend(["--bookmark", b]);
        }
        self.ok(&args);
    }
}

#[test]
#[ignore = "captures fixtures from the jj named by JJPR_CAPTURE_JJ"]
fn capture_fixtures() {
    let jj = PathBuf::from(std::env::var("JJPR_CAPTURE_JJ").expect("set JJPR_CAPTURE_JJ"));
    let raw_version = String::from_utf8(
        Command::new(&jj)
            .arg("--version")
            .output()
            .expect("jj --version")
            .stdout,
    )
    .expect("utf-8");
    let v = parse_jj_version(&raw_version).expect("a jj version");
    let push_args = push_new_bookmark_args(Some(v));
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = tmp.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[user]\nname = \"Test User\"\nemail = \"{EMAIL}\"\n\
             [ui]\ncolor = \"never\"\npaginate = \"never\"\n\
             [debug]\ncommit-timestamp = \"2001-02-03T04:05:06+07:00\"\n\
             operation-timestamp = \"2001-02-03T04:05:06+07:00\"\n\
             [operation]\nhostname = \"host\"\nusername = \"user\"\n"
        ),
    )
    .expect("write config");
    let remote = tmp.path().join("remote.git");
    let git = Command::new("git")
        .args(["init", "--bare", "-q"])
        .arg(&remote)
        .output()
        .expect("git init");
    assert!(git.status.success());
    let remote = remote.canonicalize().expect("remote exists");
    let remote_url = remote.to_str().expect("utf-8 path").to_string();

    let mut c = Capture {
        jj,
        config,
        repo: tmp.path().to_path_buf(),
        stderr: String::new(),
    };
    c.ok(&["git", "init", "--colocate", "repo"]);
    c.repo = tmp.path().join("repo");
    let root = c.repo.clone();
    c.ok(&["git", "remote", "add", "origin", &remote_url]);

    // Trunk: `main` pushed, so trunk() resolves to main@origin.
    std::fs::write(root.join("README.md"), "readme\n").expect("write");
    c.ok(&["commit", "-m", "base"]);
    // `landed` shares trunk's commit, as a fast-forwarded PR branch does until it is
    // deleted; it sorts before `main`.
    c.ok(&["bookmark", "create", "main", "landed", "-r", "@-"]);
    c.push(push_args, &["main", "landed"]);

    // A three-commit stack: A (synced, and a tracked `feat@v2` deleted below), B
    // (never-pushed `local-only`, and where `feature` was pushed), C (a rename, and
    // where `feature` has moved to since).
    std::fs::write(root.join("a.txt"), "a\na\na\n").expect("write");
    c.ok(&["commit", "-m", "stack A\n\nsecond line"]);
    c.ok(&[
        "bookmark",
        "create",
        "synced",
        "\"feat@v2\"",
        "conflicted",
        "-r",
        "@-",
    ]);
    std::fs::write(root.join("b.txt"), "b\n").expect("write");
    c.ok(&["commit", "-m", "stack B"]);
    c.ok(&["bookmark", "create", "feature", "local-only", "-r", "@-"]);
    c.push(push_args, &["synced", "feature"]);
    c.first_ok(&[
        &[
            push_args,
            &[
                "git",
                "push",
                "--remote",
                "origin",
                "--bookmark",
                "exact:feat@v2",
            ],
        ]
        .concat(),
        &[
            push_args,
            &[
                "git",
                "push",
                "--remote",
                "origin",
                "--bookmark",
                "exact:\"feat@v2\"",
            ],
        ]
        .concat(),
    ]);
    std::fs::rename(root.join("a.txt"), root.join("renamed.txt")).expect("rename");
    c.ok(&["commit", "-m", "stack C: rename"]);
    c.ok(&["bookmark", "set", "feature", "-r", "@-"]);
    let tip = c.commit_id("@-");

    // A tracked bookmark deleted locally: jj lists it with no local target.
    c.first_ok(&[
        &["bookmark", "delete", "exact:feat@v2"],
        &["bookmark", "delete", "exact:\"feat@v2\""],
    ]);

    // A bookmark-free sibling of B, made divergent below.
    c.ok(&["new", "synced", "-m", "side"]);
    let side = c.one_line(&["log", "-r", "@", "--no-graph", "-T", "change_id"]);
    c.ok(&["new", &tip]);

    // A conflicted bookmark: two concurrent operations move it from A to B and to
    // the side commit. (Moving it to a commit and that commit's descendant does not
    // conflict: jj resolves it.)
    let b = c.commit_id(&format!("{tip}-"));
    let before = c.op_id();
    c.ok(&["bookmark", "set", "conflicted", "-r", &b]);
    c.ok(&[
        "--at-op",
        &before,
        "--ignore-working-copy",
        "bookmark",
        "set",
        "conflicted",
        "-r",
        &side,
    ]);
    c.ok(&["status"]);

    // A divergent change: the side commit described twice concurrently.
    let before = c.op_id();
    c.ok(&["describe", "-r", &side, "-m", "side v2"]);
    c.ok(&[
        "--at-op",
        &before,
        "--ignore-working-copy",
        "describe",
        "-r",
        &side,
        "-m",
        "side v3",
    ]);
    c.ok(&["status"]);

    let owned = format!("author(exact:\"{EMAIL}\")");
    let bookmarks = |revset: &str| -> Vec<String> {
        [
            "--ignore-working-copy",
            "bookmark",
            "list",
            "--revisions",
            revset,
            "--template",
            BOOKMARK_TEMPLATE,
        ]
        .map(str::to_string)
        .to_vec()
    };

    let mine = bookmarks("(mine()) ~ trunk()");
    let owned_list = bookmarks(&format!("({owned}) ~ trunk()"));
    let status = bookmarks("::(@ | (mine())) ~ trunk()");
    let changes_revset = format!("trunk()..\"{tip}\"");
    let all_side = format!("change_id({side})");

    let captures = [
        ("version.txt", raw_version.clone()),
        ("bookmark-list-mine.txt", c.read(&as_strs(&mine))),
        ("bookmark-list-owned.txt", c.read(&as_strs(&owned_list))),
        ("bookmark-list-status.txt", c.read(&as_strs(&status))),
        (
            "log.txt",
            c.read(&[
                "--ignore-working-copy",
                "log",
                "--revisions",
                &changes_revset,
                "--no-graph",
                "--template",
                LOG_TEMPLATE,
            ]),
        ),
        (
            "trunk-bookmarks.txt",
            c.read(&[
                "--ignore-working-copy",
                "log",
                "--revisions",
                "trunk()",
                "--no-graph",
                "--limit",
                "1",
                "--template",
                r#"remote_bookmarks.map(|b| b.name()).join(",")"#,
            ]),
        ),
        (
            "resolve-divergent.txt",
            c.read(&[
                "--ignore-working-copy",
                "log",
                "-r",
                &all_side,
                "--no-graph",
                "-T",
                r#"commit_id ++ "\n""#,
            ]),
        ),
        // The remote's URL is a scratch path on the capturing machine; it is not kept.
        (
            "remote-list.txt",
            c.read(&["--ignore-working-copy", "git", "remote", "list"])
                .replace(&remote_url, "/scratch/remote.git"),
        ),
    ];
    let dir = fixtures_root().join(format!("{}.{}.{}", v.major, v.minor, v.patch));
    std::fs::create_dir_all(&dir).expect("mkdir fixtures");
    for (name, body) in captures {
        std::fs::write(dir.join(name), body).expect("write fixture");
    }
    std::fs::write(dir.join("stderr.txt"), &c.stderr).expect("write stderr");
}

fn as_strs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

// --- per-version assertions -----------------------------------------------------

fn fixture(version: &str, name: &str) -> String {
    let path = fixtures_root().join(version).join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn check_version(version: &str) {
    let parsed = parse_jj_version(&fixture(version, "version.txt")).expect("version.txt parses");
    assert_eq!(
        format!("{}.{}.{}", parsed.major, parsed.minor, parsed.patch),
        version
    );
    for name in [
        "bookmark-list-mine.txt",
        "bookmark-list-owned.txt",
        "bookmark-list-status.txt",
    ] {
        check_bookmarks(version, name);
    }
    check_log(version);
    assert_eq!(
        parse_default_branch(&fixture(version, "trunk-bookmarks.txt")).as_deref(),
        Some("main"),
        "jj {version}: trunk also carries `landed`, which sorts first"
    );
    let copies: Vec<String> = fixture(version, "resolve-divergent.txt")
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    assert_eq!(
        copies.len(),
        2,
        "jj {version}: both copies of the divergent change"
    );
    assert_ne!(copies[0], copies[1], "jj {version}");
    let remotes = parse_remote_list(&fixture(version, "remote-list.txt"));
    let remotes: Vec<_> = remotes
        .iter()
        .map(|r| (r.name.as_str(), r.url.as_str()))
        .collect();
    assert_eq!(
        remotes,
        vec![("origin", "/scratch/remote.git")],
        "jj {version}"
    );
    assert_eq!(
        fixture(version, "stderr.txt"),
        "",
        "jj {version} warned about something jjpr runs"
    );
}

/// The same bookmarks whichever revset jjpr lists them with: `feature` moved ahead
/// of `feature@origin`, `local-only` sharing that remote's commit, `synced` pushed
/// as is. The deleted `feat@v2` has no local target, so `--revisions` never lists
/// it, and the conflicted bookmark is skipped with a warning.
fn check_bookmarks(version: &str, name: &str) {
    let (bookmarks, warnings) = parse_bookmark_output(&fixture(version, name)).unwrap();
    let mut got: Vec<(&str, bool, bool)> = bookmarks
        .iter()
        .map(|b| (b.name.as_str(), b.has_remote, b.is_synced))
        .collect();
    got.sort_unstable();
    assert_eq!(
        got,
        vec![
            ("feature", true, false),
            ("local-only", false, false),
            ("synced", true, true),
        ],
        "jj {version} {name}: (name, has_remote, is_synced)"
    );
    assert_eq!(
        warnings,
        vec!["conflicted".to_string()],
        "jj {version} {name}"
    );
    assert!(
        bookmarks
            .iter()
            .all(|b| !b.commit_id.is_empty() && !b.change_id.is_empty()),
        "jj {version} {name}"
    );
}

fn check_log(version: &str) {
    let entries = parse_log_output(&fixture(version, "log.txt")).unwrap();
    let firsts: Vec<&str> = entries
        .iter()
        .map(|e| e.description_first_line.as_str())
        .collect();
    assert_eq!(
        firsts,
        vec!["stack C: rename", "stack B", "stack A"],
        "jj {version}: trunk()..tip, newest first"
    );
    assert_eq!(
        entries[2].description, "stack A\n\nsecond line\n",
        "jj {version}"
    );
    for pair in entries.windows(2) {
        assert_eq!(
            pair[0].parents,
            vec![pair[1].commit_id.clone()],
            "jj {version}"
        );
    }
    for e in &entries {
        assert_eq!(e.author_name, "Test User", "jj {version}");
        assert_eq!(e.author_email, EMAIL, "jj {version}");
        assert!(
            !e.is_working_copy && !e.conflict && !e.empty,
            "jj {version}: {e:?}"
        );
    }
    assert_eq!(entries[0].local_bookmarks, vec!["feature"], "jj {version}");
    assert!(
        entries[1]
            .local_bookmarks
            .contains(&"local-only".to_string()),
        "jj {version}: {:?}",
        entries[1].local_bookmarks
    );
    assert!(
        entries[1]
            .remote_bookmarks
            .contains(&"feature@origin".to_string()),
        "jj {version}: {:?}",
        entries[1].remote_bookmarks
    );
    assert!(
        entries[2]
            .remote_bookmarks
            .contains(&"synced@origin".to_string()),
        "jj {version}: {:?}",
        entries[2].remote_bookmarks
    );
}

#[test]
fn every_captured_version_has_a_test() {
    let mut captured: Vec<String> = std::fs::read_dir(fixtures_root())
        .expect("tests/fixtures/jj")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    captured.sort();
    assert!(
        captured.len() >= 2,
        "fixtures from at least two jj versions: {captured:?}"
    );
    assert_eq!(
        captured, TESTED,
        "add a version_tests! line for each captured version, and nothing else"
    );
}

macro_rules! version_tests {
    ($($name:ident => $version:literal),* $(,)?) => {
        const TESTED: &[&str] = &[$($version),*];
        $(
            #[test]
            fn $name() {
                check_version($version);
            }
        )*
    };
}

// 0.33 is below jjpr's supported floor (0.36); it is kept because its output is
// identical, which is worth knowing before raising or lowering the floor.
version_tests! {
    jj_0_33_0 => "0.33.0",
    jj_0_36_0 => "0.36.0",
    jj_0_37_0 => "0.37.0",
    jj_0_38_0 => "0.38.0",
    jj_0_40_0 => "0.40.0",
    jj_0_45_1 => "0.45.1",
}
