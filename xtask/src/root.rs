use std::path::{Path, PathBuf};

use crate::error::Fail;
use crate::version::DEFAULT_MODEL;

#[derive(Clone, Debug, Default)]
pub struct Env {
    pub dogfood_root: Option<String>,
    pub model: Option<String>,
    pub owned: Option<String>,
}

impl Env {
    pub fn from_process() -> Self {
        Self {
            dogfood_root: std::env::var("DOGFOOD_ROOT").ok().filter(|v| !v.is_empty()),
            model: std::env::var("DEV_DOGFOOD_MODEL")
                .ok()
                .filter(|v| !v.is_empty()),
            owned: std::env::var("DEV_OWNED").ok(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct GateOpts {
    pub dogfood_root: Option<String>,
    pub model: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Lab {
    pub root: PathBuf,
    pub model: String,
}

#[derive(Clone, Debug, Default)]
struct Config {
    dogfood_root: Option<String>,
    model: Option<String>,
}

pub fn extract_gate_opts(args: &[String]) -> Result<(GateOpts, Vec<String>), Fail> {
    let mut dogfood_root = None;
    let mut model = None;
    let mut rest = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--dogfood-root" => {
                let value = args.get(index + 1).ok_or_else(|| {
                    Fail::new("DOGFOOD_ROOT_INVALID", "--dogfood-root needs a path")
                })?;
                dogfood_root = Some(value.clone());
                index += 2;
            }
            "--model" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| Fail::new("UNKNOWN_COMMAND", "--model needs a value").exit(2))?;
                model = Some(value.clone());
                index += 2;
            }
            _ => {
                rest.push(args[index].clone());
                index += 1;
            }
        }
    }
    Ok((
        GateOpts {
            dogfood_root,
            model,
        },
        rest,
    ))
}

/// Config, then env, then a unique sibling. An explicit path that is not a lab does not fall through.
pub fn resolve_lab(repo: &Path, opts: &GateOpts, env: &Env) -> Result<Lab, Fail> {
    let config = read_config(&repo.join(".dev/config"))?;
    let model = opts
        .model
        .clone()
        .or(config.model)
        .or(env.model.clone())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let root = if let Some(path) = &opts.dogfood_root {
        require_lab(Path::new(path), "DOGFOOD_ROOT_INVALID")?
    } else if let Some(path) = &env.dogfood_root {
        require_lab(Path::new(path), "DOGFOOD_ROOT_INVALID")?
    } else if let Some(path) = &config.dogfood_root {
        require_lab(Path::new(path), "DOGFOOD_ROOT_INVALID")?
    } else {
        discover(repo)?
    };
    Ok(Lab { root, model })
}

fn discover(repo: &Path) -> Result<PathBuf, Fail> {
    let parent = repo.parent().ok_or_else(|| {
        Fail::new(
            "DOGFOOD_ROOT_NOT_FOUND",
            "repository has no parent directory to search for a dogfood lab",
        )
    })?;
    let mut hits = Vec::new();
    for name in ["dogfood", "dogfood-eval"] {
        let candidate = parent.join(name);
        if is_lab(&candidate) {
            hits.push(candidate);
        }
    }
    match hits.len() {
        0 => Err(Fail::new(
            "DOGFOOD_ROOT_NOT_FOUND",
            "no dogfood lab configured, and no unique sibling named dogfood or dogfood-eval",
        )),
        1 => hits
            .remove(0)
            .canonicalize()
            .map_err(|err| Fail::new("DOGFOOD_ROOT_NOT_FOUND", err.to_string())),
        _ => Err(Fail::new(
            "DOGFOOD_ROOT_AMBIGUOUS",
            "both dogfood and dogfood-eval siblings look like labs; set dogfood_root in .dev/config",
        )),
    }
}

fn require_lab(path: &Path, code: &str) -> Result<PathBuf, Fail> {
    let resolved = path
        .canonicalize()
        .map_err(|_| Fail::new(code, format!("{} is not a dogfood lab", path.display())))?;
    if !is_lab(&resolved) {
        return Err(Fail::new(
            code,
            format!(
                "{} is missing eval/scripts/release_check.py or eval/scripts/rc_check.py",
                resolved.display()
            ),
        ));
    }
    Ok(resolved)
}

pub fn is_lab(path: &Path) -> bool {
    path.join("eval/scripts/release_check.py").is_file()
        && path.join("eval/scripts/rc_check.py").is_file()
}

fn read_config(path: &Path) -> Result<Config, Fail> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(err) => {
            return Err(Fail::new(
                "DOGFOOD_ROOT_INVALID",
                format!("could not read {}: {err}", path.display()),
            ));
        }
    };
    let mut config = Config::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(Fail::new(
                "DOGFOOD_ROOT_INVALID",
                format!("bad config line in {}: {line}", path.display()),
            ));
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "dogfood_root" => config.dogfood_root = Some(value.to_string()),
            "model" => config.model = Some(value.to_string()),
            _ => {}
        }
    }
    Ok(config)
}
