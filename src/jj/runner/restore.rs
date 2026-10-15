//! Rolling the repo back to an operation without touching the working copy.

use anyhow::Result;

use super::JjRunner;
use crate::jj::Jj;

impl JjRunner {
    /// The commit ids `@` is built on, read without touching the working copy.
    pub(super) fn parents_of_working_copy(&self) -> Result<Vec<String>> {
        let output = self.run_jj(&[
            "log",
            "-r",
            "parents(@)",
            "--no-graph",
            "--template",
            r#"commit_id ++ "\n""#,
        ])?;
        Ok(output.lines().map(str::to_string).collect())
    }

    /// `jj op restore` that leaves the working copy alone, for an `@` that is
    /// the same commit at `op_id` and now. When `@` has moved since (the
    /// rewrite snapshotted the edits into a new `@`), the restore would leave
    /// the working copy stale and updating it would remove the edits, so it
    /// refuses and restores nothing.
    pub(super) fn restore_keeping_working_copy(&self, op_id: &str) -> Result<()> {
        let then = self.run_jj(&[
            "--at-op",
            op_id,
            "log",
            "-r",
            "@",
            "--no-graph",
            "-T",
            "commit_id",
        ])?;
        if then != self.get_working_copy_commit_id()? {
            anyhow::bail!(
                "the working copy moved after operation {op_id}, so restoring around it \
                 would strand its edits"
            );
        }
        self.run_jj(&["op", "restore", op_id])?;
        Ok(())
    }
}
