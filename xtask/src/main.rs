//! `cargo xtask <fmt|clippy|test|cov|all>`
//!
//! `cov` builds the workspace with `-C instrument-coverage`, runs every test
//! (including the binaries spawned by integration tests), merges the
//! profiles with rustup's `llvm-tools` and fails unless every `poly-*/src`
//! file reaches 100% line coverage.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::{env, fs};

type Result<T = ()> = std::result::Result<T, String>;

const TASKS: &str = "usage: cargo xtask <fmt|clippy|test|cov|all>";

fn main() -> ExitCode {
    let task = env::args().nth(1).unwrap_or_default();
    let result = match task.as_str() {
        "fmt" => fmt(),
        "clippy" => clippy(),
        "test" => test(),
        "cov" => cov(),
        "all" => fmt()
            .and_then(|()| clippy())
            .and_then(|()| test())
            .and_then(|()| cov()),
        _ => Err(TASKS.to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_owned()
}

fn cargo() -> Command {
    let mut c = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    c.current_dir(root());
    c
}

fn run(mut cmd: Command) -> Result {
    eprintln!("xtask: running {cmd:?}");
    let status = cmd
        .status()
        .map_err(|e| format!("cannot run {cmd:?}: {e}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("{cmd:?} failed with {status}"))
}

fn output(mut cmd: Command) -> Result<String> {
    let out = cmd
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("cannot run {cmd:?}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{cmd:?} failed with {}", out.status));
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}

fn fmt() -> Result {
    let mut c = cargo();
    c.args(["fmt", "--all", "--", "--check"]);
    run(c)
}

fn clippy() -> Result {
    let mut c = cargo();
    c.args([
        "clippy",
        "--workspace",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ]);
    run(c)
}

fn test() -> Result {
    let mut c = cargo();
    c.args(["test", "--workspace"]);
    run(c)
}

/// Directory holding rustup's `llvm-tools` for the active toolchain.
fn llvm_bin() -> Result<PathBuf> {
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let sysroot = output({
        let mut c = Command::new(&rustc);
        c.args(["--print", "sysroot"]);
        c
    })?;
    let host = output({
        let mut c = Command::new(&rustc);
        c.arg("-vV");
        c
    })?;
    let host = host
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .ok_or("cannot determine rustc host triple")?;
    let bin = Path::new(sysroot.trim())
        .join("lib/rustlib")
        .join(host)
        .join("bin");
    if !bin.join("llvm-cov").exists() {
        return Err(format!(
            "{} missing: run `rustup component add llvm-tools`",
            bin.display()
        ));
    }
    Ok(bin)
}

/// Pull every `"executable":"..."` path out of cargo's JSON messages.
fn executables(json: &str) -> Vec<String> {
    const KEY: &str = "\"executable\":\"";
    json.lines()
        .filter_map(|l| {
            let start = l.find(KEY)? + KEY.len();
            let len = l[start..].find('"')?;
            Some(l[start..start + len].to_owned())
        })
        .collect()
}

/// Source files under `poly-*/src` that contain code.
fn sources(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut stack: Vec<PathBuf> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("poly-"))
        })
        .map(|p| p.join("src"))
        .collect();
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs")
                && fs::read_to_string(&path).is_ok_and(|s| s.contains("fn "))
            {
                let rel = path.strip_prefix(root).unwrap_or(&path);
                files.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    files.sort();
    files
}

/// Lines with a zero execution count in `llvm-cov show -show-line-counts` output.
fn zero_count_lines(show: &str) -> Vec<String> {
    let mut file = "";
    let mut out = Vec::new();
    for line in show.lines() {
        if !line.starts_with(' ') && line.ends_with(".rs:") {
            file = line.trim_end_matches(':');
        }
        let mut cols = line.splitn(3, '|');
        if let (Some(n), Some(count), Some(src)) = (cols.next(), cols.next(), cols.next())
            && count.trim() == "0"
        {
            out.push(format!("{file}:{}: {}", n.trim(), src.trim()));
        }
    }
    out
}

/// Check the `llvm-cov report` table: the third `%` column is line coverage.
/// Returns the files that fail, with reasons.
fn coverage_gate(report: &str, expected: &[String]) -> Vec<(String, String)> {
    let mut problems = Vec::new();
    let mut seen = Vec::new();
    for line in report.lines() {
        let mut cols = line.split_whitespace();
        let Some(file) = cols.next() else { continue };
        let Some(file) = expected
            .iter()
            .find(|e| file == e.as_str() || file.ends_with(&format!("/{e}")))
        else {
            continue;
        };
        seen.push(file.clone());
        let lines = cols.filter(|c| c.ends_with('%')).nth(2).unwrap_or("?");
        if lines != "100.00%" {
            problems.push((file.clone(), format!("{lines} lines")));
        }
    }
    for file in expected.iter().filter(|f| !seen.contains(f)) {
        problems.push((file.clone(), "missing from coverage report".to_owned()));
    }
    problems
}

fn cov() -> Result {
    let root = root();
    let bin = llvm_bin()?;
    let target = root.join("target/cov");
    let profraw = target.join("profraw");
    let _ = fs::remove_dir_all(&profraw);
    fs::create_dir_all(&profraw).map_err(|e| e.to_string())?;

    let instrumented = || {
        let mut c = cargo();
        c.env("RUSTFLAGS", "-C instrument-coverage")
            .env("CARGO_TARGET_DIR", &target)
            .env("LLVM_PROFILE_FILE", profraw.join("poly-%p-%m.profraw"))
            .args(["test", "--workspace", "--exclude", "xtask"]);
        c
    };
    run(instrumented())?;
    let mut build = instrumented();
    build.args(["--no-run", "--message-format=json"]);
    let objects = executables(&output(build)?);

    let raws: Vec<PathBuf> = fs::read_dir(&profraw)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .collect();
    let merged = target.join("poly.profdata");
    let mut merge = Command::new(bin.join("llvm-profdata"));
    merge
        .args(["merge", "-sparse", "-o"])
        .arg(&merged)
        .args(&raws);
    run(merge)?;

    let llvm_cov = |subcommand: &str| {
        let mut c = Command::new(bin.join("llvm-cov"));
        c.current_dir(&root)
            .arg(subcommand)
            .arg(format!("-instr-profile={}", merged.display()))
            .arg("-ignore-filename-regex=(/rustc/|/\\.cargo/|/tests/|xtask/)");
        for (i, obj) in objects.iter().enumerate() {
            if i > 0 {
                c.arg("-object");
            }
            c.arg(obj);
        }
        c
    };
    let table = output(llvm_cov("report"))?;
    print!("{table}");
    let problems = coverage_gate(&table, &sources(&root));
    if problems.is_empty() {
        eprintln!("xtask: coverage gate ok (100% lines on poly-* sources)");
        return Ok(());
    }
    let mut show = llvm_cov("show");
    show.args(["-show-line-counts", "-show-instantiations=false"])
        .args(problems.iter().map(|(file, _)| file));
    let uncovered = zero_count_lines(&output(show).unwrap_or_default());
    let summary: Vec<String> = problems
        .iter()
        .map(|(f, why)| format!("{f}: {why}"))
        .collect();
    Err(format!(
        "coverage gate failed:\n  {}\nuncovered lines:\n  {}\n\
         (a never-called closure counts as a missed line even when it shares a line with\n\
         executed code; `llvm-cov report -show-functions <file>` lists them)",
        summary.join("\n  "),
        uncovered.join("\n  ")
    ))
}
