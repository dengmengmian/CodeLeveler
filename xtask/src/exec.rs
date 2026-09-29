use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;

use crate::error::Fail;

#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

impl CommandSpec {
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: cwd.into(),
            env: Vec::new(),
        }
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

#[derive(Clone, Debug)]
pub struct ProcessOut {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

pub trait Host {
    fn run(&self, spec: &CommandSpec) -> Result<ProcessOut, Fail>;
}

pub struct SystemHost {
    pub echo: bool,
}

impl Host for SystemHost {
    fn run(&self, spec: &CommandSpec) -> Result<ProcessOut, Fail> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args).current_dir(&spec.cwd);
        for (key, value) in &spec.env {
            cmd.env(key, value);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                Fail::new("TOOL_MISSING", format!("{} is not on PATH", spec.program))
            } else {
                Fail::new(
                    "TOOL_FAILED",
                    format!("could not start {}: {err}", spec.program),
                )
            }
        })?;
        let echo = self.echo;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let out_thread = thread::spawn(move || read_tee(stdout, echo, false));
        let err_thread = thread::spawn(move || read_tee(stderr, echo, true));
        let status = child
            .wait()
            .map_err(|err| Fail::new("TOOL_FAILED", err.to_string()))?;
        let stdout = out_thread.join().unwrap_or_default();
        let stderr = err_thread.join().unwrap_or_default();
        Ok(ProcessOut {
            status: status.code().unwrap_or(1),
            stdout,
            stderr,
        })
    }
}

fn read_tee<R: Read>(reader: Option<R>, echo: bool, to_stderr: bool) -> String {
    let Some(mut reader) = reader else {
        return String::new();
    };
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if echo {
                    if to_stderr {
                        let mut out = std::io::stderr();
                        let _ = out.write_all(&chunk[..n]);
                        let _ = out.flush();
                    } else {
                        let mut out = std::io::stdout();
                        let _ = out.write_all(&chunk[..n]);
                        let _ = out.flush();
                    }
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

pub fn git(cwd: &std::path::Path, args: &[&str]) -> CommandSpec {
    CommandSpec::new("git", args.iter().copied(), cwd)
}

pub fn ok_stdout(host: &dyn Host, spec: CommandSpec, code: &str) -> Result<String, Fail> {
    let out = host.run(&spec)?;
    if out.status != 0 {
        let detail = format!(
            "{} {}\n{}",
            spec.program,
            spec.args.join(" "),
            first_lines(&out.stderr, 40)
        );
        return Err(Fail::new(code, detail.trim().to_string()));
    }
    Ok(out.stdout.trim().to_string())
}

pub fn first_lines(text: &str, n: usize) -> String {
    text.lines().take(n).collect::<Vec<_>>().join("\n")
}

pub fn tail(text: &str, n: usize) -> String {
    let lines: Vec<_> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[cfg(test)]
pub struct FakeHost {
    log: std::sync::Mutex<Vec<CommandSpec>>,
    handler: std::sync::Mutex<Box<dyn FnMut(&CommandSpec) -> Result<ProcessOut, Fail> + Send>>,
}

#[cfg(test)]
impl FakeHost {
    pub fn new(
        handler: impl FnMut(&CommandSpec) -> Result<ProcessOut, Fail> + Send + 'static,
    ) -> Self {
        Self {
            log: std::sync::Mutex::new(Vec::new()),
            handler: std::sync::Mutex::new(Box::new(handler)),
        }
    }

    pub fn log(&self) -> Vec<CommandSpec> {
        self.log.lock().expect("fake host log").clone()
    }
}

#[cfg(test)]
impl Host for FakeHost {
    fn run(&self, spec: &CommandSpec) -> Result<ProcessOut, Fail> {
        self.log.lock().expect("fake host log").push(spec.clone());
        (self.handler.lock().expect("fake host handler"))(spec)
    }
}
