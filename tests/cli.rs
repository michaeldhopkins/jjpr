use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;

fn stk() -> assert_cmd::Command {
    cargo_bin_cmd!("stk")
}

#[test]
fn test_help_shows_usage() {
    stk()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Manage stacked pull requests"))
        .stdout(predicate::str::contains("submit"))
        .stdout(predicate::str::contains("auth"));
}

#[test]
fn test_submit_help() {
    stk()
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
    stk()
        .args(["auth", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Manage GitHub authentication"))
        .stdout(predicate::str::contains("test"))
        .stdout(predicate::str::contains("setup"));
}

#[test]
fn test_auth_test_help() {
    stk()
        .args(["auth", "test", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Test GitHub authentication"));
}

#[test]
fn test_auth_setup_help() {
    stk()
        .args(["auth", "setup", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Show authentication setup instructions"));
}

#[test]
fn test_draft_and_ready_conflict() {
    stk()
        .args(["submit", "--draft", "--ready"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn test_version() {
    stk()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("stacker"));
}
