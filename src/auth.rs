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
            println!("jjpr uses the GitHub CLI (gh) for authentication.\n");
            println!("Setup:");
            println!("  1. Install gh: https://cli.github.com");
            println!("  2. Run: gh auth login");
            println!("  3. Verify: jjpr auth test\n");
            println!("Alternatively, set GITHUB_TOKEN or GH_TOKEN environment variable.");
        }
        ForgeKind::GitLab => {
            println!("jjpr uses the GitLab CLI (glab) for authentication.\n");
            println!("Setup:");
            println!("  1. Install glab: https://gitlab.com/gitlab-org/cli");
            println!("  2. Run: glab auth login");
            println!("  3. Verify: jjpr auth test\n");
            println!("Alternatively, set GITLAB_TOKEN environment variable.");
        }
        ForgeKind::Forgejo => {
            println!("jjpr uses token-based authentication for Forgejo/Codeberg.\n");
            println!("Setup:");
            println!("  1. Generate a token in your Forgejo/Codeberg account settings");
            println!("  2. Set FORGEJO_TOKEN environment variable");
            println!("  3. Verify: jjpr auth test");
        }
    }
}
