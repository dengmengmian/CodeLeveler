fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let repo = match repo_root() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(err.exit);
        }
    };
    let outcome = xtask::dispatch(&repo, &args);
    print!("{}", outcome.stdout);
    eprint!("{}", outcome.stderr);
    std::process::exit(outcome.code);
}

fn repo_root() -> Result<std::path::PathBuf, xtask::Fail> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|err| xtask::Fail::new("GIT_FAILED", err.to_string()))?;
    if !out.status.success() {
        return Err(xtask::Fail::new(
            "REPO_NOT_FOUND",
            "git rev-parse --show-toplevel failed; run ./dev from the CodeLeveler checkout",
        ));
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        return Err(xtask::Fail::new(
            "REPO_NOT_FOUND",
            "git did not report a repository root",
        ));
    }
    Ok(std::path::PathBuf::from(path))
}
