use anyhow::Result;

use crate::forge::{Forge, ForgeKind};

/// Test forge authentication and display user info.
pub fn test_auth(forge: &dyn Forge) -> Result<()> {
    let login = forge.get_authenticated_user()?;
    println!("Authenticated as: {login}");
    Ok(())
}

/// Print authentication setup help for the given forge.
pub fn print_auth_help(kind: ForgeKind) {
    match kind {
        ForgeKind::GitHub => {
            println!("GitHub authentication options (in order of priority):\n");
            println!("  1. Set GITHUB_TOKEN (or GH_TOKEN) environment variable");
            println!("  2. Run `gh auth login` (jjpr reads gh's stored credentials)\n");
            println!("Verify: jjpr auth test");
        }
        ForgeKind::GitLab => {
            println!("GitLab authentication options (in order of priority):\n");
            println!("  1. Set GITLAB_TOKEN environment variable");
            println!("  2. Run `glab auth login` (jjpr reads glab's stored credentials)\n");
            println!("Verify: jjpr auth test");
        }
        ForgeKind::Forgejo => {
            println!("Forgejo/Codeberg authentication:\n");
            println!("  1. Generate a token in your Forgejo/Codeberg account settings");
            println!("  2. Set FORGEJO_TOKEN environment variable\n");
            println!("Verify: jjpr auth test");
        }
    }
}

/// Print authentication setup help for all supported forges.
pub fn print_auth_help_all() {
    println!("Could not detect forge from the current directory.\n");
    println!("Supported forges:\n");

    println!("--- GitHub ---");
    print_auth_help(ForgeKind::GitHub);
    println!();

    println!("--- GitLab ---");
    print_auth_help(ForgeKind::GitLab);
    println!();

    println!("--- Forgejo/Codeberg ---");
    print_auth_help(ForgeKind::Forgejo);
}
