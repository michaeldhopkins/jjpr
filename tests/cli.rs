use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;

fn jjpr() -> assert_cmd::Command {
    cargo_bin_cmd!("jjpr")
}

#[test]
fn test_help_shows_usage() {
    jjpr()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Manage stacked pull requests"))
        .stdout(predicate::str::contains("submit"))
        .stdout(predicate::str::contains("auth"));
}

#[test]
fn test_submit_help() {
    jjpr()
        .args(["submit", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Submit a bookmark stack"))
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
        .stdout(predicate::str::contains("Manage GitHub authentication"))
        .stdout(predicate::str::contains("test"))
        .stdout(predicate::str::contains("setup"));
}

#[test]
fn test_auth_test_help() {
    jjpr()
        .args(["auth", "test", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Test GitHub authentication"));
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
