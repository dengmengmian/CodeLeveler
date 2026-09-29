use std::path::{Path, PathBuf};

use crate::error::Fail;
use crate::exec::{self, Host};

pub fn format_files(host: &dyn Host, repo: &Path, files: &[PathBuf]) -> Result<(), Fail> {
    if files.is_empty() {
        return Ok(());
    }
    let config = repo.join("rustfmt.toml");
    let mut args = vec![
        "--edition".to_string(),
        "2024".to_string(),
        "--config-path".to_string(),
        config.display().to_string(),
    ];
    for file in files {
        args.push(file.display().to_string());
    }
    let spec = crate::exec::CommandSpec::new("rustfmt", args, repo);
    match host.run(&spec) {
        Err(err) if err.code == "TOOL_MISSING" => {
            Err(Fail::new("FMT_TOOL_MISSING", "rustfmt is not on PATH"))
        }
        Err(err) => Err(err),
        Ok(out) if out.status != 0 => Err(Fail::new(
            "FMT_FAIL",
            format!(
                "rustfmt exited {}\n{}",
                out.status,
                exec::first_lines(&out.stderr, 40)
            ),
        )),
        Ok(_) => Ok(()),
    }
}
