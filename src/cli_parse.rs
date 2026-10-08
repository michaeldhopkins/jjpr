//! Parsing the command line, with a list of jjpr's commands after an unknown
//! one. clap's own message names only the usage line, and a reader who guessed
//! a command (`jjpr list`) is left to conclude jjpr cannot do it: a small model
//! did exactly that in a blind run (2026-10-08).

use clap::error::ErrorKind;
use clap::{CommandFactory, Parser};

use crate::cli::Cli;

/// The command line, or exit with clap's message, extended for an unknown
/// command.
pub fn parse() -> Cli {
    Cli::try_parse().unwrap_or_else(|e| {
        if e.kind() == ErrorKind::InvalidSubcommand {
            eprint!("{}", e.render());
            eprintln!("\n{}", commands(&Cli::command()));
            std::process::exit(2);
        }
        e.exit()
    })
}

/// "jjpr's commands:" and each visible subcommand with its one-line summary.
pub fn commands(cmd: &clap::Command) -> String {
    let subs: Vec<&clap::Command> = cmd
        .get_subcommands()
        .filter(|s| !s.is_hide_set() && s.get_name() != "help")
        .collect();
    let width = subs.iter().map(|s| s.get_name().len()).max().unwrap_or(0);
    let mut text = format!("{}'s commands:", cmd.get_name());
    for s in subs {
        let about = s.get_about().map(|a| a.to_string()).unwrap_or_default();
        text.push_str(&format!("\n  {:width$}  {about}", s.get_name()));
    }
    text.push_str(&format!(
        "\nEach has its own --help: {} <command> --help",
        cmd.get_name()
    ));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_command_is_answered_with_the_real_ones() {
        let text = commands(&Cli::command());
        assert!(text.starts_with("jjpr's commands:\n  submit"), "{text}");
        assert!(
            text.contains("\n  undo    Take back the last jjpr command"),
            "{text}"
        );
        assert!(text.contains("\n  redo    "), "{text}");
        assert!(!text.contains("\n  help"), "{text}");
        assert!(text.ends_with("Each has its own --help: jjpr <command> --help"));
    }

    #[test]
    fn only_an_unknown_command_gets_the_list() {
        let kind = |args: &[&str]| Cli::try_parse_from(args).err().map(|e| e.kind());
        assert_eq!(kind(&["jjpr", "list"]), Some(ErrorKind::InvalidSubcommand));
        assert_eq!(
            kind(&["jjpr", "undo", "--nonsense"]),
            Some(ErrorKind::UnknownArgument)
        );
    }
}
