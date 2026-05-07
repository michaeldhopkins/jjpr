use std::process::{Command, Output};

use anyhow::{anyhow, Result};

use super::context::ParityContext;
use super::scenario::{JjprCommand, RunSpec, Scenario, SetupStep};

/// Captured output of the command-under-test.
pub struct RunOutput {
    pub status: std::process::ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl RunOutput {
    fn from_output(out: Output) -> Self {
        Self {
            status: out.status,
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }
}

/// Path to the freshly-built jjpr binary. Cargo populates this env var when
/// running integration tests, so we always invoke the just-compiled CLI
/// rather than whatever happens to be on PATH.
fn jjpr_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jjpr")
}

/// Resolve a bookmark reference inside the scenario file ("auth") to the
/// prefixed name in the live test repo ("p1234ab-2345cd-auth").
fn resolve_bookmark(ctx: &ParityContext, name: &str) -> String {
    ctx.prefixed(name)
}

/// Run all setup steps in order. Setup steps are best-effort fail-fast:
/// if any step errors, the scenario is reported as a setup failure (not a
/// test assertion failure).
pub fn run_setup(ctx: &ParityContext, scenario: &Scenario) -> Result<()> {
    for (i, step) in scenario.setup.iter().enumerate() {
        match step {
            SetupStep::Submit { extra_args } => {
                let target = scenario
                    .stack
                    .last()
                    .map(|e| resolve_bookmark(ctx, &e.bookmark))
                    .ok_or_else(|| anyhow!("setup.submit needs at least one stack entry"))?;
                let mut args: Vec<String> = vec!["submit".into(), target];
                args.extend(extra_args.iter().cloned());
                let out = invoke_jjpr(ctx, &args);
                if !out.status.success() {
                    return Err(anyhow!(
                        "setup step #{i} (submit) failed:\nstdout: {}\nstderr: {}",
                        out.stdout, out.stderr
                    ));
                }
            }
            SetupStep::ExternalAdminMerge { bookmark, method } => {
                let prefixed = resolve_bookmark(ctx, bookmark);
                ctx.external_admin_merge(&prefixed, *method);
                // Pull the merge into our local view so subsequent jjpr
                // invocations see the up-to-date forge state.
                ctx.run_jj(&["git", "fetch"]);
            }
            SetupStep::SetRemoteUrl { remote, url } => {
                let status = Command::new("git")
                    .args(["remote", "set-url", remote, url])
                    .current_dir(&ctx.repo_path)
                    .status()
                    .expect("git remote set-url");
                if !status.success() {
                    return Err(anyhow!(
                        "setup step #{i} (set_remote_url {remote} -> {url}) failed"
                    ));
                }
            }
            SetupStep::SetGitConfig { key, value } => {
                let status = Command::new("git")
                    .args(["config", "--local", key, value])
                    .current_dir(&ctx.repo_path)
                    .status()
                    .expect("git config --local");
                if !status.success() {
                    return Err(anyhow!(
                        "setup step #{i} (set_git_config {key}={value}) failed"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Run the command-under-test and capture its output. The harness always
/// targets the topmost bookmark in the stack so the scenario doesn't need
/// to know about the prefix.
pub fn run_command(ctx: &ParityContext, scenario: &Scenario) -> RunOutput {
    let target = scenario
        .stack
        .last()
        .map(|e| resolve_bookmark(ctx, &e.bookmark))
        .expect("scenario must define at least one stack entry");

    let mut args: Vec<String> = vec![scenario.run.command.as_str().into(), target];
    args.extend(scenario.run.extra_args.iter().cloned());
    apply_default_run_args(&scenario.run, &mut args);
    invoke_jjpr(ctx, &args)
}

/// Inject defaults that make scenarios runnable without per-file boilerplate.
/// Notably: watch needs a bounded timeout so the integration test exits.
fn apply_default_run_args(spec: &RunSpec, args: &mut Vec<String>) {
    if let JjprCommand::Watch = spec.command
        && !args.iter().any(|a| a == "--timeout")
    {
        let minutes = spec.timeout_minutes.unwrap_or(1);
        args.push("--timeout".into());
        args.push(minutes.to_string());
    }
}

fn invoke_jjpr(ctx: &ParityContext, args: &[String]) -> RunOutput {
    let out = Command::new(jjpr_binary())
        .args(args)
        .current_dir(&ctx.repo_path)
        .output()
        .expect("invoke jjpr binary");
    RunOutput::from_output(out)
}
