use std::path::Path;

use crate::clean;
use crate::error::{Fail, Report};
use crate::exec::{self, CommandSpec, Host, SystemHost};
use crate::fmt;
use crate::publish;
use crate::qualify;
use crate::root::{self, Env};
use crate::scope::{self, Affected};
use crate::version;

pub struct Outcome {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

pub fn dispatch(repo: &Path, args: &[String]) -> Outcome {
    dispatch_with(repo, args, &Env::from_process(), &SystemHost { echo: true })
}

pub fn dispatch_with(repo: &Path, args: &[String], env: &Env, host: &dyn Host) -> Outcome {
    let repo = match repo.canonicalize() {
        Ok(path) => path,
        Err(err) => {
            return from_fail(Fail::new(
                "REPO_NOT_FOUND",
                format!("{}: {err}", repo.display()),
            ));
        }
    };
    if args.is_empty() || matches!(args[0].as_str(), "help" | "--help" | "-h") {
        return Outcome {
            code: 0,
            stdout: help_text(),
            stderr: String::new(),
        };
    }
    let command = args[0].as_str();
    let rest = &args[1..];
    let result = match command {
        "fmt" => cmd_fmt(host, &repo, env, rest),
        "check" => cmd_cargo(host, &repo, env, rest, false),
        "test" => cmd_cargo(host, &repo, env, rest, true),
        "verify" => cmd_verify(host, &repo, env, rest),
        "clean" => clean::parse_args(rest).and_then(|mode| clean::apply(host, &repo, mode)),
        "pre-release" => gate(host, &repo, env, rest, GateKind::PreRelease),
        "dogfood" => gate(host, &repo, env, rest, GateKind::Dogfood),
        "release" => gate(host, &repo, env, rest, GateKind::Release),
        "publish" => publish::run(host, &repo, env, rest),
        other => return unknown(other),
    };
    from_result(result)
}

enum GateKind {
    PreRelease,
    Dogfood,
    Release,
}

fn gate(
    host: &dyn Host,
    repo: &Path,
    env: &Env,
    args: &[String],
    kind: GateKind,
) -> Result<Report, Fail> {
    let (opts, rest) = root::extract_gate_opts(args)?;
    if let Some(extra) = rest.first() {
        return Err(Fail::new("UNKNOWN_COMMAND", format!("unknown argument: {extra}")).exit(2));
    }
    match kind {
        GateKind::PreRelease => qualify::run_pre_release(host, repo, env, &opts),
        GateKind::Dogfood => qualify::run_dogfood(host, repo, env, &opts),
        GateKind::Release => qualify::run_release(host, repo, env, &opts),
    }
}

fn cmd_fmt(host: &dyn Host, repo: &Path, env: &Env, args: &[String]) -> Result<Report, Fail> {
    let files = scope::owned_rust_files(host, repo, &positionals(args)?, env.owned.as_deref())?;
    if files.is_empty() {
        return Ok(Report::ok("NO_OWNED_CHANGES"));
    }
    fmt::format_files(host, repo, &files)?;
    Ok(Report::ok(format!("fmt: PASS\nfiles: {}", files.len())))
}

fn cmd_cargo(
    host: &dyn Host,
    repo: &Path,
    env: &Env,
    args: &[String],
    test: bool,
) -> Result<Report, Fail> {
    let files = scope::owned_scope(host, repo, &positionals(args)?, env.owned.as_deref())?;
    if files.is_empty() {
        return Ok(Report::ok("NO_OWNED_CHANGES"));
    }
    let affected = scope::affected(repo, &files)?;
    let (prefix, report) = run_affected(host, repo, &affected, test)?;
    Ok(Report::ok(format!("{prefix}{report}")))
}

fn cmd_verify(host: &dyn Host, repo: &Path, env: &Env, args: &[String]) -> Result<Report, Fail> {
    let files = scope::owned_scope(host, repo, &positionals(args)?, env.owned.as_deref())?;
    let mut lines = Vec::new();
    if files.is_empty() {
        lines.push("NO_OWNED_CHANGES".to_string());
    } else {
        let rust: Vec<_> = files
            .iter()
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("rs"))
            .cloned()
            .collect();
        if !rust.is_empty() {
            fmt::format_files(host, repo, &rust)?;
            lines.push("fmt: PASS".to_string());
        }
        let affected = scope::affected(repo, &files)?;
        let (check_prefix, _) = run_affected(host, repo, &affected, false)?;
        lines.push(format!("{check_prefix}check: PASS"));
        let (test_prefix, _) = run_affected(host, repo, &affected, true)?;
        lines.push(format!("{test_prefix}test: PASS"));
    }
    let local = version::read_local(repo)?;
    let current = version::require_local(&local)?;
    lines.push(format!("version: {current}"));
    lines.push("verify: PASS".to_string());
    Ok(Report::ok(lines.join("\n")))
}

fn run_affected(
    host: &dyn Host,
    repo: &Path,
    affected: &Affected,
    test: bool,
) -> Result<(String, String), Fail> {
    let spec = cargo_spec(repo, affected, test);
    let label = if test { "test" } else { "check" };
    let prefix = match affected {
        Affected::Workspace { reason } => format!("{label}: workspace ({reason})\n"),
        Affected::Packages(packages) => format!("{label}: {}\n", packages.join(", ")),
    };
    let out = host.run(&spec).map_err(|err| {
        if err.code == "TOOL_MISSING" {
            Fail::new("TOOL_MISSING", format!("{label}: cargo is not on PATH"))
        } else {
            err
        }
    })?;
    if out.status != 0 {
        let code = if test { "TEST_FAIL" } else { "CHECK_FAIL" };
        return Err(Fail::new(
            code,
            format!(
                "cargo {label} exited {}\n{}",
                out.status,
                exec::tail(&out.stderr, 40)
            ),
        ));
    }
    Ok((prefix, format!("{label}: PASS")))
}

fn cargo_spec(repo: &Path, affected: &Affected, test: bool) -> CommandSpec {
    let mut args = vec![if test { "test" } else { "check" }.to_string()];
    match affected {
        Affected::Workspace { .. } => args.push("--workspace".to_string()),
        Affected::Packages(packages) => {
            for package in packages {
                args.push("-p".to_string());
                args.push(package.clone());
            }
        }
    }
    if test {
        args.push("--no-fail-fast".to_string());
    }
    CommandSpec::new("cargo", args, repo)
}

fn positionals(args: &[String]) -> Result<Vec<String>, Fail> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            out.extend(args[index + 1..].iter().cloned());
            break;
        }
        if arg.starts_with('-') {
            return Err(Fail::new("UNKNOWN_COMMAND", format!("unknown argument: {arg}")).exit(2));
        }
        out.push(arg.clone());
        index += 1;
    }
    Ok(out)
}

fn unknown(command: &str) -> Outcome {
    Outcome {
        code: 2,
        stdout: help_text(),
        stderr: format!("FAIL UNKNOWN_COMMAND\nunknown command: {command}\n"),
    }
}

fn from_result(result: Result<Report, Fail>) -> Outcome {
    match result {
        Ok(report) => Outcome {
            code: report.code,
            stdout: with_nl(report.text),
            stderr: String::new(),
        },
        Err(err) => from_fail(err),
    }
}

fn from_fail(err: Fail) -> Outcome {
    Outcome {
        code: err.exit,
        stdout: String::new(),
        stderr: format!("{err}\n"),
    }
}

fn with_nl(text: String) -> String {
    if text.is_empty() || text.ends_with('\n') {
        text
    } else {
        format!("{text}\n")
    }
}

pub fn help_text() -> String {
    "\
Usage: ./dev <command>

Development
  fmt [path...]     rustfmt this task's Rust files
  check [path...]   cargo check the affected packages
  test [path...]    cargo test the affected packages
  verify [path...]  fmt, then check, then affected tests, then local version consistency

Maintenance
  clean             remove registered disposable artifacts
  clean --all       also cargo clean and registered build caches
  clean build       same as clean --all

Release
  pre-release       L0-L2 of the dogfood release gate; check only
  dogfood           release qualification, then RC qualification
  release           pre-release, then dogfood, on one frozen commit
  publish           bump, qualify, tag, and push (--patch is the default)
  publish --minor
  publish --major
  publish --dry-run
"
    .to_string()
}
