use anyhow::Result;

use crate::github::GitHub;

/// Test GitHub authentication and display user info.
pub fn test_auth(github: &dyn GitHub) -> Result<()> {
    let login = github.get_authenticated_user()?;
    println!("Authenticated as: {login}");
    Ok(())
}

/// Print authentication setup help.
pub fn print_auth_help() {
    println!("stacker uses the GitHub CLI (gh) for authentication.\n");
    println!("Setup:");
    println!("  1. Install gh: https://cli.github.com");
    println!("  2. Run: gh auth login");
    println!("  3. Verify: stk auth test\n");
    println!("Alternatively, set GITHUB_TOKEN or GH_TOKEN environment variable.");
}
