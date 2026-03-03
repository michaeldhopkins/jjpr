use std::path::PathBuf;

use clap::CommandFactory;
use clap_complete::Shell;

fn main() {
    let out_dir = PathBuf::from(
        std::env::var("JJPR_ASSETS_DIR").unwrap_or_else(|_| "target/assets".into()),
    );
    let completions_dir = out_dir.join("completions");
    std::fs::create_dir_all(&completions_dir).expect("failed to create output directories");

    let mut cmd = jjpr::cli::Cli::command();

    let man = clap_mangen::Man::new(cmd.clone());
    let mut buf = Vec::new();
    man.render(&mut buf).expect("failed to render man page");
    std::fs::write(out_dir.join("jjpr.1"), buf).expect("failed to write man page");

    for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
        clap_complete::generate_to(shell, &mut cmd, "jjpr", &completions_dir)
            .expect("failed to generate completions");
    }

    println!("Generated assets in {}", out_dir.display());
}
