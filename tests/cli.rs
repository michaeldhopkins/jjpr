use std::path::Path;
use std::process::Command;

use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use tempfile::TempDir;

fn jjpr() -> assert_cmd::Command {
    cargo_bin_cmd!("jjpr")
}

fn jj_available() -> bool {
    Command::new("jj")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn run_cmd(program: &str, args: &[&str], dir: &Path) {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {program}: {e}"));
    assert!(
        output.status.success(),
        "{} {} failed: {}",
        program,
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Create a jj repo with a self-hosted Forgejo remote and a repo-local config
/// that sets `forge = "forgejo"` with a custom token env var.
fn setup_forgejo_config_repo(forge_token_env: &str) -> (TempDir, TempDir) {
    let origin_dir = TempDir::new().expect("create temp dir");
    let repo_dir = TempDir::new().expect("create temp dir");

    run_cmd("git", &["init", "--bare"], origin_dir.path());
    run_cmd("jj", &["git", "init", "--colocate"], repo_dir.path());

    let repo = repo_dir.path();
    run_cmd("jj", &["config", "set", "--repo", "user.name", "Test"], repo);
    run_cmd("jj", &["config", "set", "--repo", "user.email", "t@t.dev"], repo);

    // Self-hosted Forgejo URL that won't be auto-detected
    run_cmd(
        "jj",
        &["git", "remote", "add", "origin", "https://forgejo.mycompany.com/team/project.git"],
        repo,
    );

    // Create a commit and bookmark so submit gets past bookmark inference
    run_cmd("jj", &["new", "-m", "test change"], repo);
    std::fs::write(repo.join("file.txt"), "test").expect("write file");
    run_cmd("jj", &["bookmark", "set", "test-branch"], repo);

    // Write repo-local config
    let config = format!(
        "forge = \"forgejo\"\nforge_token_env = \"{forge_token_env}\"\n"
    );
    std::fs::write(repo.join(".jj").join("jjpr.toml"), config).expect("write config");

    (origin_dir, repo_dir)
}

#[test]
fn test_help_shows_usage() {
    jjpr()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Manage stacked pull requests"))
        .stdout(predicate::str::contains("Run with no arguments"))
        .stdout(predicate::str::contains("read-only"))
        .stdout(predicate::str::contains("submit"))
        .stdout(predicate::str::contains("auth"));
}

#[test]
fn test_submit_help() {
    jjpr()
        .args(["submit", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Push bookmarks and create/update pull requests"))
        .stdout(predicate::str::contains("--reviewer"))
        .stdout(predicate::str::contains("--remote"))
        .stdout(predicate::str::contains("--draft"))
        .stdout(predicate::str::contains("--ready"))
        .stdout(predicate::str::contains("--dry-run"));
}

#[test]
fn test_auth_help() {
    jjpr()
        .args(["auth", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Manage forge authentication"))
        .stdout(predicate::str::contains("test"))
        .stdout(predicate::str::contains("setup"));
}

#[test]
fn test_auth_test_help() {
    jjpr()
        .args(["auth", "test", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Test forge authentication"));
}

#[test]
fn test_auth_setup_help() {
    jjpr()
        .args(["auth", "setup", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Show authentication setup instructions"));
}

#[test]
fn test_draft_and_ready_conflict() {
    jjpr()
        .args(["submit", "--draft", "--ready"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn test_version() {
    jjpr()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("jjpr"));
}

#[test]
fn test_version_short() {
    jjpr()
        .arg("-v")
        .assert()
        .success()
        .stdout(predicate::str::contains("jjpr"));
}

#[test]
fn test_help_shows_no_fetch() {
    jjpr()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("--no-fetch"));
}

#[test]
fn test_submit_help_shows_no_fetch() {
    jjpr()
        .args(["submit", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--no-fetch"));
}

#[test]
fn test_merge_help() {
    jjpr()
        .args(["merge", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Merge a stack of PRs from the bottom up"))
        .stdout(predicate::str::contains("--merge-method"))
        .stdout(predicate::str::contains("--required-approvals"))
        .stdout(predicate::str::contains("--no-ci-check"))
        .stdout(predicate::str::contains("--remote"))
        .stdout(predicate::str::contains("--dry-run"));
}

#[test]
fn test_config_init_help() {
    jjpr()
        .args(["config", "init", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Create a default config file"));
}

#[test]
fn test_help_shows_merge_command() {
    jjpr()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("merge"))
        .stdout(predicate::str::contains("config"));
}

#[test]
fn test_submit_base_flag_in_help() {
    jjpr()
        .args(["submit", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--base"));
}

#[test]
fn test_merge_base_flag_in_help() {
    jjpr()
        .args(["merge", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--base"));
}

/// Issue #2 regression: when config sets forge_token_env to a custom var,
/// the error message must mention that var name, not the default "FORGEJO_TOKEN".
#[test]
fn test_submit_forgejo_custom_token_env_in_error() {
    if !jj_available() {
        eprintln!("skipping: jj not available");
        return;
    }

    let custom_env = "MY_CUSTOM_FORGEJO_TOKEN";
    let (_origin, repo) = setup_forgejo_config_repo(custom_env);

    jjpr()
        .args(["submit", "--no-fetch"])
        .current_dir(repo.path())
        .env_remove(custom_env)
        .assert()
        .failure()
        .stderr(predicate::str::contains(custom_env));
}

/// Issue #2 regression (default path): when forge_token_env is NOT set in config,
/// the error should mention the default "FORGEJO_TOKEN".
#[test]
fn test_submit_forgejo_default_token_env_in_error() {
    if !jj_available() {
        eprintln!("skipping: jj not available");
        return;
    }

    let (_origin, repo) = setup_forgejo_config_repo("FORGEJO_TOKEN");

    jjpr()
        .args(["submit", "--no-fetch"])
        .current_dir(repo.path())
        .env_remove("FORGEJO_TOKEN")
        .assert()
        .failure()
        .stderr(predicate::str::contains("FORGEJO_TOKEN"));
}

/// Issue #1 regression: `auth test` for config-set Forgejo should use the
/// configured token env var, not silently pass None.
/// We verify the error mentions the custom env var (not the default).
#[test]
fn test_auth_test_forgejo_uses_custom_token_env() {
    if !jj_available() {
        eprintln!("skipping: jj not available");
        return;
    }

    let custom_env = "MY_FORGEJO_AUTH_TOKEN";
    let (_origin, repo) = setup_forgejo_config_repo(custom_env);

    // With the token NOT set, auth test should error mentioning the custom var.
    // Before the fix, it would pass None and hit the generic "FORGEJO_TOKEN not set" error.
    jjpr()
        .args(["auth", "test"])
        .current_dir(repo.path())
        .env_remove(custom_env)
        .assert()
        .failure()
        .stderr(predicate::str::contains(custom_env));
}
