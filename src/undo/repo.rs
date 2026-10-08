//! The jj operations undo needs, behind a trait so its logic can be tested
//! without a repository.
//!
//! These read and restore the operation log, which jjpr's other commands never
//! need, so they live here rather than on [`crate::jj::Jj`].

use std::path::PathBuf;

use anyhow::Result;
use vcs_runner::{run_jj_utf8, run_jj_utf8_ignore_wc};

use crate::jj::version;

/// One operation in jj's operation log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub id: String,
    pub description: String,
}

/// Where a bookmark points locally and on one remote. `None`: it does not
/// exist there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Targets {
    pub local: Option<String>,
    pub remote: Option<String>,
}

pub trait UndoRepo: Send + Sync {
    /// The full id of the newest operation.
    fn current_op(&self) -> Result<String>;
    /// Take any working-copy edits into `@`, as any jj command would.
    fn snapshot(&self) -> Result<()>;
    /// Everything about the repo that undo must find unchanged, as text: each
    /// local bookmark's target, and every head that is not just a remote
    /// branch's. Remote-tracking refs are left out: the forge is asked about
    /// those directly.
    fn view_fingerprint(&self) -> Result<String>;
    /// [`UndoRepo::view_fingerprint`] as of operation `op`.
    fn view_fingerprint_at(&self, op: &str) -> Result<String>;
    /// The operations after `op`, newest first, or `None` when `op` is not
    /// among the newest `limit`.
    fn ops_since(&self, op: &str, limit: usize) -> Result<Option<Vec<Operation>>>;
    fn op_exists(&self, op: &str) -> Result<bool>;
    /// `jj op restore --what repo`: the local repo goes back, the
    /// remote-tracking refs stay as they are, so a push afterwards still
    /// expects what is really on the remote.
    fn restore_repo_only(&self, op: &str) -> Result<()>;
    fn targets(&self, bookmark: &str, remote: &str) -> Result<Targets>;
    /// Point `bookmark` at `commit`, creating it or moving it backwards.
    fn set_bookmark(&self, bookmark: &str, commit: &str) -> Result<()>;
    fn delete_bookmark(&self, bookmark: &str) -> Result<()>;
    /// Push `bookmark` to `remote`: its new target, or its deletion.
    fn push_bookmark(&self, bookmark: &str, remote: &str) -> Result<()>;
    /// Files whose working copy differs from what `@` held at `op`: the edits a
    /// restore to `op` would take off the disk.
    fn files_changed_since(&self, op: &str) -> Result<Vec<String>>;
    /// Each workspace's working-copy commit as `(name, commit)`, at `op`, or now.
    fn working_copies(&self, op: Option<&str>) -> Result<Vec<(String, String)>>;
    /// The workspaces whose working copy is `@`: this one, and any sharing its commit.
    fn own_working_copies(&self) -> Result<Vec<String>>;
}

/// How far back undo looks for what changed since an entry.
pub const OPS_SEARCHED: usize = 1000;

/// Each local bookmark and where it points. A conflicted one lists every side.
const LOCAL_BOOKMARKS: &str = r#"if(remote, "", name ++ "	" ++ if(conflict, "conflict:" ++ added_targets.map(|c| c.commit_id()).join(","), if(normal_target, normal_target.commit_id(), "absent")) ++ "\n")"#;

/// Heads that something other than a remote branch keeps visible.
const OWN_HEADS: &str = "(visible_heads() ~ ::remote_bookmarks()) | working_copies()";

const OP_LINE: &str = r#"id ++ "	" ++ description.first_line() ++ "\n""#;

/// Each working-copy commit with the workspaces on it (`a@ b@`).
const WORKING_COPIES: &str = r#"working_copies ++ "\t" ++ commit_id ++ "\n""#;

pub struct JjRepo {
    path: PathBuf,
}

impl JjRepo {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn read(&self, args: &[&str]) -> Result<String> {
        Ok(run_jj_utf8_ignore_wc(&self.path, args)?)
    }
}

impl UndoRepo for JjRepo {
    fn current_op(&self) -> Result<String> {
        self.read(&["op", "log", "-n1", "--no-graph", "-T", "id"])
    }

    fn snapshot(&self) -> Result<()> {
        run_jj_utf8(&self.path, &["status"])?;
        Ok(())
    }

    fn view_fingerprint(&self) -> Result<String> {
        self.fingerprint_with(&[])
    }

    fn view_fingerprint_at(&self, op: &str) -> Result<String> {
        self.fingerprint_with(&["--at-op", op])
    }

    fn ops_since(&self, op: &str, limit: usize) -> Result<Option<Vec<Operation>>> {
        let limit = limit.to_string();
        let out = self.read(&["op", "log", "--no-graph", "-n", &limit, "-T", OP_LINE])?;
        Ok(parse_ops_since(&out, op))
    }

    fn op_exists(&self, op: &str) -> Result<bool> {
        let args = ["--at-op", op, "op", "log", "-n1", "--no-graph", "-T", "id"];
        match run_jj_utf8_ignore_wc(&self.path, &args) {
            Ok(_) => Ok(true),
            Err(e) if e.to_string().contains("No operation ID matching") => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    fn restore_repo_only(&self, op: &str) -> Result<()> {
        // Not --ignore-working-copy: the restore must update the checkout too.
        run_jj_utf8(&self.path, &["op", "restore", "--what", "repo", op])?;
        Ok(())
    }

    fn targets(&self, bookmark: &str, remote: &str) -> Result<Targets> {
        let quoted = quote(bookmark);
        let local = format!("bookmarks(exact:{quoted})");
        let tracked = format!("remote_bookmarks(exact:{quoted}, exact:{})", quote(remote));
        Ok(Targets {
            local: self.one_commit(&local)?,
            remote: self.one_commit(&tracked)?,
        })
    }

    fn set_bookmark(&self, bookmark: &str, commit: &str) -> Result<()> {
        let args = [
            "bookmark",
            "set",
            bookmark,
            "-r",
            commit,
            "--allow-backwards",
        ];
        self.read(&args)?;
        Ok(())
    }

    fn delete_bookmark(&self, bookmark: &str) -> Result<()> {
        self.read(&["bookmark", "delete", &format!("exact:{bookmark}")])?;
        Ok(())
    }

    fn push_bookmark(&self, bookmark: &str, remote: &str) -> Result<()> {
        let mut args = version::push_new_bookmark_args(version::installed_jj_version()).to_vec();
        args.extend(["git", "push", "--remote", remote, "--bookmark"]);
        let exact = format!("exact:{bookmark}");
        args.push(&exact);
        self.read(&args)?;
        Ok(())
    }

    fn files_changed_since(&self, op: &str) -> Result<Vec<String>> {
        let then = self.read(&[
            "--at-op",
            op,
            "log",
            "--no-graph",
            "-r",
            "@",
            "-T",
            "commit_id",
        ])?;
        let out = self.read(&["diff", "--from", then.trim(), "--to", "@", "--name-only"])?;
        Ok(out
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect())
    }

    fn working_copies(&self, op: Option<&str>) -> Result<Vec<(String, String)>> {
        let at: Vec<&str> = op.map(|op| vec!["--at-op", op]).unwrap_or_default();
        let args = [
            at.as_slice(),
            &[
                "log",
                "--no-graph",
                "-r",
                "working_copies()",
                "-T",
                WORKING_COPIES,
            ],
        ]
        .concat();
        Ok(parse_working_copies(&self.read(&args)?))
    }

    fn own_working_copies(&self) -> Result<Vec<String>> {
        let out = self.read(&["log", "--no-graph", "-r", "@", "-T", WORKING_COPIES])?;
        Ok(parse_working_copies(&out)
            .into_iter()
            .map(|(name, _)| name)
            .collect())
    }
}

impl JjRepo {
    fn fingerprint_with(&self, global: &[&str]) -> Result<String> {
        let bookmarks = [global, &["bookmark", "list", "-T", LOCAL_BOOKMARKS]].concat();
        let heads = [
            global,
            &[
                "log",
                "--no-graph",
                "-r",
                OWN_HEADS,
                "-T",
                "commit_id ++ \"\\n\"",
            ],
        ];
        Ok(fingerprint(
            &self.read(&bookmarks)?,
            &self.read(&heads.concat())?,
        ))
    }

    /// The one commit `revset` names, `None` for none. A conflicted bookmark
    /// names several, and has no single target to record.
    fn one_commit(&self, revset: &str) -> Result<Option<String>> {
        let out = self.read(&[
            "log",
            "--no-graph",
            "-r",
            revset,
            "-T",
            "commit_id ++ \"\\n\"",
        ])?;
        let ids: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        match ids.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some((*one).to_string())),
            _ => anyhow::bail!("'{revset}' names {} commits, not one", ids.len()),
        }
    }
}

/// A revset string literal.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Order-independent: the same repo always gives the same text.
pub fn fingerprint(bookmarks: &str, heads: &str) -> String {
    let mut b: Vec<&str> = bookmarks.lines().filter(|l| !l.is_empty()).collect();
    let mut h: Vec<&str> = heads.lines().filter(|l| !l.is_empty()).collect();
    b.sort_unstable();
    h.sort_unstable();
    h.dedup();
    format!("bookmarks:\n{}\nheads:\n{}\n", b.join("\n"), h.join("\n"))
}

/// `(workspace, commit)` pairs from the working-copies template output, one per workspace.
pub fn parse_working_copies(text: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for line in text.lines() {
        let Some((names, commit)) = line.split_once('\t') else {
            continue;
        };
        for name in names.split_whitespace() {
            let name = name.strip_suffix('@').unwrap_or(name);
            pairs.push((name.to_string(), commit.to_string()));
        }
    }
    pairs.sort();
    pairs
}

/// The operations before `op` in `log` (newest first, one `id<TAB>description`
/// per line), or `None` when `op` is not there. `op` may be a prefix.
pub fn parse_ops_since(log: &str, op: &str) -> Option<Vec<Operation>> {
    let mut since = Vec::new();
    for line in log.lines().filter(|l| !l.is_empty()) {
        let (id, description) = line.split_once('\t').unwrap_or((line, ""));
        if !op.is_empty() && id.starts_with(op) {
            return Some(since);
        }
        since.push(Operation {
            id: id.to_string(),
            description: description.to_string(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_ignores_order_and_repeated_heads() {
        let a = fingerprint("b\tc2\na\tc1\n", "h2\nh1\nh1\n");
        let b = fingerprint("a\tc1\nb\tc2", "h1\nh2");
        assert_eq!(a, b);
        assert_ne!(a, fingerprint("a\tc1\nb\tc3", "h1\nh2"));
        assert_ne!(a, fingerprint("a\tc1\nb\tc2", "h1"));
    }

    #[test]
    fn ops_since_lists_newer_operations_newest_first() {
        let log = "o3\tpush\no2\tdescribe commit\no1\tsnapshot working copy\n";
        let since = parse_ops_since(log, "o1").unwrap();
        let ids: Vec<_> = since.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, vec!["o3", "o2"]);
        assert_eq!(since[1].description, "describe commit");
        assert_eq!(parse_ops_since(log, "o3"), Some(vec![]));
    }

    #[test]
    fn ops_since_is_none_when_the_operation_is_not_found() {
        assert_eq!(parse_ops_since("o3\tx\no2\ty\n", "o9"), None);
        assert_eq!(parse_ops_since("o3\tx\n", ""), None);
    }

    #[test]
    fn working_copies_split_names_sharing_a_commit() {
        let text = "verbose@\tc1\ndefault@ undo@\tc2\n\nnot a line\n";
        assert_eq!(
            parse_working_copies(text),
            vec![
                ("default".to_string(), "c2".to_string()),
                ("undo".to_string(), "c2".to_string()),
                ("verbose".to_string(), "c1".to_string()),
            ]
        );
        assert!(parse_working_copies("").is_empty());
    }

    #[test]
    fn quote_escapes_what_ends_a_revset_string() {
        assert_eq!(quote("a"), "\"a\"");
        assert_eq!(quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }
}
