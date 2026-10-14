//! Which branch trunk is.

use anyhow::Result;

use super::JjRunner;
use crate::jj::templates;

impl JjRunner {
    /// The branch `trunk()` names: read from a `name@remote` alias when the user
    /// set one, else from the remote bookmarks on trunk's commit. A quoted name
    /// in the alias (`"rel@2"@origin`) is left to the second way, which reads
    /// names unquoted.
    pub(super) fn default_branch(&self) -> Result<String> {
        if let Ok(alias) = self.run_jj(&["config", "get", r#"revset-aliases."trunk()""#]) {
            let alias = alias.trim();
            if let Some((name, _remote)) = alias.split_once('@')
                && !name.is_empty()
                && !name
                    .contains(|c: char| c.is_whitespace() || matches!(c, '(' | '|' | '"' | '\''))
            {
                return Ok(name.to_string());
            }
        }

        let output = self.run_jj(&[
            "log",
            "--revisions",
            "trunk()",
            "--no-graph",
            "--limit",
            "1",
            "--template",
            templates::TRUNK_BOOKMARKS_TEMPLATE,
        ])?;

        templates::parse_default_branch(&output)
            .ok_or_else(|| anyhow::anyhow!("could not determine default branch"))
    }
}
