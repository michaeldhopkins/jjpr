#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

pub fn jj_available() -> bool {
    Command::new("jj")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A jj repo in a temp directory with a local bare-git "origin" remote.
/// `trunk()` resolves to `main@origin`.
pub struct JjTestRepo {
    _origin_dir: TempDir,
    repo_dir: TempDir,
}

impl JjTestRepo {
    pub fn new() -> Self {
        let origin_dir = TempDir::new().expect("create temp dir");
        let repo_dir = TempDir::new().expect("create temp dir");

        run_cmd("git", &["init", "--bare"], origin_dir.path());
        run_cmd("jj", &["git", "init", "--colocate"], repo_dir.path());

        let repo = repo_dir.path();
        run_cmd(
            "jj",
            &["config", "set", "--repo", "user.name", "Test User"],
            repo,
        );
        run_cmd(
            "jj",
            &[
                "config",
                "set",
                "--repo",
                "user.email",
                "test@jjpr.dev",
            ],
            repo,
        );

        let origin_url = origin_dir.path().to_str().expect("non-utf8 path");
        run_cmd(
            "jj",
            &["git", "remote", "add", "origin", origin_url],
            repo,
        );

        // Create initial commit and push main so trunk() resolves
        std::fs::write(repo.join("README.md"), "test repo\n").expect("write");
        run_cmd("jj", &["commit", "-m", "initial commit"], repo);
        run_cmd("jj", &["bookmark", "set", "main", "-r", "@-"], repo);
        run_cmd(
            "jj",
            &[
                "git",
                "push",
                "--remote",
                "origin",
                "--bookmark",
                "main",
                "--allow-new",
            ],
            repo,
        );

        Self {
            _origin_dir: origin_dir,
            repo_dir,
        }
    }

    pub fn run_jj(&self, args: &[&str]) -> String {
        let output = Command::new("jj")
            .args(args)
            .current_dir(self.path())
            .output()
            .expect("run jj");
        assert!(
            output.status.success(),
            "jj {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    pub fn write_file(&self, name: &str, content: &str) {
        let path = self.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(&path, content).expect("write");
    }

    pub fn commit(&self, message: &str) {
        self.run_jj(&["commit", "-m", message]);
    }

    pub fn set_bookmark(&self, name: &str) {
        self.run_jj(&["bookmark", "set", name, "-r", "@-"]);
    }

    /// Write file, commit, set bookmark on the committed change.
    pub fn commit_and_bookmark(&self, file: &str, content: &str, message: &str, bookmark: &str) {
        self.write_file(file, content);
        self.commit(message);
        self.set_bookmark(bookmark);
    }

    pub fn runner(&self) -> jjpr::jj::JjRunner {
        jjpr::jj::JjRunner::new(self.path().to_path_buf()).expect("create JjRunner")
    }

    pub fn path(&self) -> &Path {
        self.repo_dir.path()
    }
}

fn run_cmd(program: &str, args: &[&str], dir: &Path) {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {program}: {e}"));
    assert!(
        output.status.success(),
        "{} {} failed: {}",
        program,
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}
