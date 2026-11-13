//! `cargo xtask <check|fmt|clippy|build|test|coverage|dev-check|dist>`
//!
//! `dev-check` is the pre-push gate: check, fmt, clippy, build, test and
//! coverage, in that order, stopping at the first failure (`all` is an alias).
//!
//! `coverage` (alias `cov`) builds the workspace with `-C instrument-coverage`, runs every test
//! (including the binaries spawned by integration tests), merges the
//! profiles with rustup's `llvm-tools` and fails unless every gated source
//! file (`unibin-*/src` and `hwtest/probe/src`) reaches 100% region,
//! function and line coverage.
//!
//! `dist` cross-builds one binary package for every slice target with
//! cargo-zigbuild / cargo-xwin and packs the slices into a single polyglot
//! file with `unibin-pack`.
//!
//! `hwkit` builds the hardware-test kit: the `hwprobe` payload packed twice
//! (every slice, and x86_64 slices only to exercise the Rosetta, Prism and
//! `--emu` fallbacks) next to the `hwtest/` scripts.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::{env, fs};

type Result<T = ()> = std::result::Result<T, String>;

const TASKS: &str = "usage: cargo xtask <check|fmt|clippy|build|test|coverage|dev-check>
       cargo xtask dist [-p <package>] [--emu <x86_64-emulator>] [-o <out>]
       cargo xtask hwkit [--emu <x86_64-emulator>] [-o <dir>]

  dev-check   pre-push gate: check, fmt, clippy, build, test, coverage (alias: all)
  coverage    100% region/function/line coverage gate (alias: cov)
  dist        cross-build every slice target and pack a release polyglot
  hwkit       build the hardware-test kit (default dir: target/hwkit)";

type Step = (&'static str, fn() -> Result);

/// The pre-push gate, in order.
const DEV_CHECK: &[Step] = &[
    ("check", check),
    ("fmt", fmt),
    ("clippy", clippy),
    ("build", build),
    ("test", test),
    ("coverage", coverage),
];

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let task = args.first().map(String::as_str).unwrap_or_default();
    let result = match task {
        "dev-check" | "all" => dev_check(),
        "cov" => coverage(),
        "dist" => parse_dist(&args[1..]).and_then(|d| dist(&d)),
        "hwkit" => parse_hwkit(&args[1..]).and_then(|d| hwkit(&d)),
        _ => match DEV_CHECK.iter().find(|(name, _)| *name == task) {
            Some((_, step)) => step(),
            None => Err(TASKS.to_owned()),
        },
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

fn dev_check() -> Result {
    for (name, step) in DEV_CHECK {
        eprintln!("xtask: dev-check: {name}");
        step().map_err(|e| format!("dev-check failed at {name}: {e}"))?;
    }
    // `coverage` runs last, so its gate message is the final line.
    Ok(())
}

fn check() -> Result {
    let mut c = cargo();
    c.args(["check", "--workspace", "--all-targets"]);
    run(c)
}

fn build() -> Result {
    let mut c = cargo();
    c.args(["build", "--workspace", "--all-targets"]);
    run(c)
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

/// Slice targets built by `dist`. cargo-zigbuild merges `universal2-apple-darwin`
/// into one fat Mach-O; the x86_64 Windows target is also the launcher's,
/// which picks the arm64 slice natively on Windows on Arm.
const DIST_TARGETS: &[&str] = &[
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "universal2-apple-darwin",
    "x86_64-unknown-freebsd",
    WINDOWS_TARGET,
    "aarch64-pc-windows-msvc",
    BUILD_STD_TARGET,
];
const WINDOWS_TARGET: &str = "x86_64-pc-windows-msvc";
/// No prebuilt std ships for this target, so it is built with `-Zbuild-std`
/// on a pinned nightly (needs its `rust-src` component and network access).
const BUILD_STD_TARGET: &str = "aarch64-unknown-freebsd";
const BUILD_STD_TOOLCHAIN: &str = "nightly-2026-11-08";

#[derive(Debug, PartialEq, Eq)]
struct Dist {
    /// Package whose binary of the same name is packed.
    package: String,
    emu: Option<PathBuf>,
    out: Option<PathBuf>,
}

fn parse_dist(args: &[String]) -> Result<Dist> {
    let mut d = Dist {
        package: "unibin-pack".to_owned(),
        emu: None,
        out: None,
    };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value\n{TASKS}"))
        };
        match flag.as_str() {
            "-p" | "--package" => d.package = value()?,
            "--emu" => d.emu = Some(absolute(&value()?)?),
            "-o" | "--output" => d.out = Some(absolute(&value()?)?),
            _ => return Err(format!("dist: unknown argument {flag:?}\n{TASKS}")),
        }
    }
    Ok(d)
}

/// User paths are relative to the invoking directory; cargo runs in the root.
fn absolute(p: &str) -> Result<PathBuf> {
    std::path::absolute(p).map_err(|e| format!("{p}: {e}"))
}

/// Where cargo puts `bin` for `target` under `dir`.
fn artifact(dir: &Path, target: &str, bin: &str) -> PathBuf {
    let exe = if target.contains("-windows-") {
        ".exe"
    } else {
        ""
    };
    dir.join(target).join("release").join(format!("{bin}{exe}"))
}

fn cross_build(dir: &Path, target: &str, package: &str) -> Result<PathBuf> {
    let mut c = if target == BUILD_STD_TARGET {
        let mut c = Command::new("rustup");
        c.current_dir(root())
            .args(["run", BUILD_STD_TOOLCHAIN, "cargo"]);
        c
    } else {
        cargo()
    };
    c.env("CARGO_TARGET_DIR", dir);
    if target.contains("-windows-") {
        c.args(["xwin", "build"]);
    } else {
        c.arg("zigbuild");
    }
    c.args([
        "--release",
        "-p",
        package,
        "--bin",
        package,
        "--target",
        target,
    ]);
    if target == BUILD_STD_TARGET {
        // The release profile uses panic = "abort".
        c.arg("-Zbuild-std=std,panic_abort");
    }
    run(c)?;
    Ok(artifact(dir, target, package))
}

fn require_cross_tools() -> Result {
    for tool in ["zigbuild", "xwin"] {
        let mut c = cargo();
        c.args([tool, "--help"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        c.status()
            .ok()
            .filter(|s| s.success())
            .ok_or_else(|| {
            format!("`cargo {tool}` is unavailable: install cargo-{tool}, or run in the Dockerfile image (see README)")
        })?;
    }
    Ok(())
}

fn cross_build_all(dir: &Path, targets: &[&str], package: &str) -> Result<Vec<PathBuf>> {
    targets
        .iter()
        .map(|t| cross_build(dir, t, package))
        .collect()
}

/// Run the workspace's own `unibin-pack` (release build under `dir`).
fn pack(dir: &Path, launcher: &Path, slices: &[PathBuf], emu: Option<&Path>, out: &Path) -> Result {
    let mut pack = cargo();
    pack.env("CARGO_TARGET_DIR", dir)
        .args(["run", "-q", "--release", "-p", "unibin-pack", "--"])
        .arg("--launcher")
        .arg(launcher);
    for slice in slices {
        pack.arg("--slice").arg(slice);
    }
    if let Some(emu) = emu {
        pack.arg("--emu").arg(emu);
    }
    pack.arg("-o").arg(out);
    run(pack)
}

fn dist(d: &Dist) -> Result {
    require_cross_tools()?;
    let dir = root().join("target/dist");
    let launcher = cross_build(&dir, WINDOWS_TARGET, "unibin-launch")?;
    let slices = cross_build_all(&dir, DIST_TARGETS, &d.package)?;
    let out = d
        .out
        .clone()
        .unwrap_or_else(|| dir.join(format!("{}.com", d.package)));
    pack(&dir, &launcher, &slices, d.emu.as_deref(), &out)
}

const HWKIT_PACKAGE: &str = "hwprobe";
/// x86_64 slices only. On arm64 hosts they exercise each OS's fallback:
/// Rosetta, Prism and `--emu`. macOS needs a thin x86_64 Mach-O, because the
/// kernel would run the arm64 half of a fat one natively.
const HWKIT_X86_64_TARGETS: &[&str] = &[
    "x86_64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "x86_64-unknown-freebsd",
    WINDOWS_TARGET,
];
/// Copied from `hwtest/` into the kit.
const HWKIT_FILES: &[&str] = &["README.md", "run.sh", "run.ps1"];

/// `hwkit` takes `dist`'s `--emu` and `-o` (a directory), but no package.
fn parse_hwkit(args: &[String]) -> Result<Dist> {
    let d = parse_dist(args)?;
    if d.package != "unibin-pack" {
        return Err(format!("hwkit always packs {HWKIT_PACKAGE}\n{TASKS}"));
    }
    Ok(d)
}

fn hwkit(d: &Dist) -> Result {
    require_cross_tools()?;
    let root = root();
    let dir = root.join("target/dist");
    let out = d.out.clone().unwrap_or_else(|| root.join("target/hwkit"));
    fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let launcher = cross_build(&dir, WINDOWS_TARGET, "unibin-launch")?;

    let all = cross_build_all(&dir, DIST_TARGETS, HWKIT_PACKAGE)?;
    pack(&dir, &launcher, &all, None, &out.join("probe.com"))?;
    let x86_64 = cross_build_all(&dir, HWKIT_X86_64_TARGETS, HWKIT_PACKAGE)?;
    if d.emu.is_none() {
        eprintln!(
            "xtask: hwkit: no --emu, so Linux-aarch64 hosts check the no-slice error instead of the emulator"
        );
    }
    let fallback = out.join("probe-x86_64.com");
    pack(&dir, &launcher, &x86_64, d.emu.as_deref(), &fallback)?;

    for file in HWKIT_FILES {
        let (from, to) = (root.join("hwtest").join(file), out.join(file));
        fs::copy(&from, &to).map_err(|e| format!("{}: {e}", from.display()))?;
    }
    eprintln!("xtask: hwkit ready in {}", out.display());
    Ok(())
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

/// Crates outside `unibin-*` whose sources are gated too. xtask is not: it
/// drives cargo and the cross toolchains, so tests cannot reach most of it.
const GATED_EXTRA: &[&str] = &["hwtest/probe"];

/// The `%` columns of `llvm-cov report`, in order. Branch coverage reads `-`
/// (branch instrumentation is off), so it is not among them.
const GATED_METRICS: [&str; 3] = ["regions", "functions", "lines"];

/// Source files of every gated crate that contain code.
fn sources(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut stack: Vec<PathBuf> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("unibin-"))
        })
        .map(|p| p.join("src"))
        .collect();
    stack.extend(GATED_EXTRA.iter().map(|c| root.join(c).join("src")));
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

/// Source lines that never ran, or that hold a region that never ran (a
/// `^0` marker line under them), in `llvm-cov show
/// -show-line-counts-or-regions` output.
fn uncovered(show: &str) -> Vec<String> {
    let mut file = "";
    let mut last = None;
    let mut out = Vec::new();
    for line in show.lines() {
        if !line.starts_with(' ') && line.ends_with(".rs:") {
            file = line.trim_end_matches(':');
        }
        let mut cols = line.splitn(3, '|');
        match (cols.next(), cols.next(), cols.next()) {
            (Some(n), Some(count), Some(src)) => {
                let at = format!("{file}:{}: {}", n.trim(), src.trim());
                if count.trim() == "0" {
                    out.push(at);
                } else {
                    last = Some(at);
                }
            }
            _ if line.contains("^0") => out.extend(last.take().map(|at| at + "  [region]")),
            _ => {}
        }
    }
    out
}

/// Check the `llvm-cov report` table: every gated file must be listed and
/// reach 100% in each of [`GATED_METRICS`]. Returns the failures, with reasons.
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
        let percents: Vec<&str> = cols.filter(|c| c.ends_with('%')).collect();
        let short: Vec<String> = GATED_METRICS
            .iter()
            .enumerate()
            .map(|(i, metric)| (percents.get(i).copied().unwrap_or("?"), metric))
            .filter(|(p, _)| *p != "100.00%")
            .map(|(p, metric)| format!("{p} {metric}"))
            .collect();
        if !short.is_empty() {
            problems.push((file.clone(), short.join(", ")));
        }
    }
    for file in expected.iter().filter(|f| !seen.contains(f)) {
        problems.push((file.clone(), "missing from coverage report".to_owned()));
    }
    problems
}

fn coverage() -> Result {
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
            .env("LLVM_PROFILE_FILE", profraw.join("unibin-%p-%m.profraw"))
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
    let merged = target.join("unibin.profdata");
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
        eprintln!(
            "xtask: coverage gate ok (100% regions, functions and lines on every gated file)"
        );
        return Ok(());
    }
    let mut show = llvm_cov("show");
    show.args(["-show-line-counts-or-regions", "-show-instantiations=false"])
        .args(problems.iter().map(|(file, _)| file));
    let uncovered = uncovered(&output(show).unwrap_or_default());
    let summary: Vec<String> = problems
        .iter()
        .map(|(f, why)| format!("{f}: {why}"))
        .collect();
    Err(format!(
        "coverage gate failed:\n  {}\nuncovered lines and regions:\n  {}\n\
         (a never-called closure or a `?` that never fails is a missed region even on a\n\
         line that ran. A function built into several binaries, such as the unit tests\n\
         and the shipped bin, is reported from one copy, not their union, so its misses\n\
         may not be listed above; `llvm-cov report -show-functions <file>` shows every\n\
         copy)",
        summary.join("\n  "),
        uncovered.join("\n  ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// `/x/y` is only absolute on Unix; Windows adds the current drive.
    fn abs(p: &str) -> PathBuf {
        std::path::absolute(p).unwrap()
    }

    #[test]
    fn dist_arguments() {
        let d = parse_dist(&[]).unwrap();
        assert_eq!(
            (d.package.as_str(), d.emu, d.out),
            ("unibin-pack", None, None)
        );
        let d = parse_dist(&args(&["-p", "app", "--emu", "/x/fex", "-o", "/y/app"])).unwrap();
        assert_eq!(d.package, "app");
        assert_eq!(d.emu, Some(abs("/x/fex")));
        assert_eq!(d.out, Some(abs("/y/app")));
        assert!(
            parse_dist(&args(&["-o", "rel"]))
                .unwrap()
                .out
                .unwrap()
                .is_absolute()
        );
        assert!(
            parse_dist(&args(&["-o"]))
                .unwrap_err()
                .contains("-o needs a value")
        );
        assert!(
            parse_dist(&args(&["--x"]))
                .unwrap_err()
                .contains("unknown argument")
        );
    }

    const REPORT: &str = "\
Filename          Regions    Missed Regions     Cover   Functions  Missed Functions  Executed       Lines      Missed Lines     Cover    Branches   Missed Branches     Cover
------------------------------------------------------------------------------------------------
/w/unibin-core/src/a.rs   10   0   100.00%   2   0   100.00%   8   0   100.00%   0   0   -
/w/unibin-core/src/b.rs   10   1    90.00%   2   0   100.00%   8   0   100.00%   0   0   -
/w/hwtest/probe/src/main.rs  10  0  100.00%   2   1    50.00%   8   1    87.50%   0   0   -
TOTAL                     30   1    96.67%   6   1    83.33%  24   1    95.83%   0   0   -
";

    #[test]
    fn gate_checks_regions_functions_and_lines_of_every_file() {
        let files = |f: &[&str]| f.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let expected = files(&[
            "unibin-core/src/a.rs",
            "unibin-core/src/b.rs",
            "hwtest/probe/src/main.rs",
            "unibin-pack/src/main.rs",
        ]);
        assert_eq!(
            coverage_gate(REPORT, &expected),
            [
                (expected[1].clone(), "90.00% regions".to_owned()),
                (
                    expected[2].clone(),
                    "50.00% functions, 87.50% lines".to_owned()
                ),
                (
                    expected[3].clone(),
                    "missing from coverage report".to_owned()
                ),
            ]
        );
        assert!(coverage_gate(REPORT, &expected[..1]).is_empty());
        // A row without the percentage columns fails rather than passing.
        let e = coverage_gate("/w/unibin-core/src/a.rs 10 0", &expected[..1]);
        assert_eq!(e[0].1, "? regions, ? functions, ? lines");
    }

    #[test]
    fn uncovered_lines_and_regions() {
        let show = "\
/w/unibin-core/src/a.rs:
    1|      2|fn f() -> Option<u8> {
    2|      2|    let x = g()?;
                             ^0
    3|      0|    dead();
    4|      2|    h(x)?
                    ^2
    5|       |}
";
        assert_eq!(
            uncovered(show),
            [
                "/w/unibin-core/src/a.rs:2: let x = g()?;  [region]",
                "/w/unibin-core/src/a.rs:3: dead();",
            ]
        );
    }

    #[test]
    fn gated_sources_include_the_probe() {
        let files = sources(&root());
        assert!(
            files.contains(&"unibin-core/src/lib.rs".to_owned()),
            "{files:?}"
        );
        assert!(
            files.contains(&"hwtest/probe/src/main.rs".to_owned()),
            "{files:?}"
        );
        assert!(!files.iter().any(|f| f.starts_with("xtask/")), "{files:?}");
    }

    #[test]
    fn hwkit_arguments() {
        let d = parse_hwkit(&args(&["--emu", "/x/qemu", "-o", "/kit"])).unwrap();
        assert_eq!(d.emu, Some(abs("/x/qemu")));
        assert_eq!(d.out, Some(abs("/kit")));
        assert!(
            parse_hwkit(&args(&["-p", "x"]))
                .unwrap_err()
                .contains("always packs")
        );
        // Every fallback slice is x86_64, and the kit ships every script.
        assert!(
            HWKIT_X86_64_TARGETS
                .iter()
                .all(|t| t.starts_with("x86_64-"))
        );
        for file in HWKIT_FILES {
            assert!(root().join("hwtest").join(file).is_file(), "{file}");
        }
    }

    #[test]
    fn dev_check_order() {
        let names: Vec<&str> = DEV_CHECK.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            ["check", "fmt", "clippy", "build", "test", "coverage"]
        );
        for name in names {
            assert!(TASKS.contains(name), "{name} missing from usage");
        }
    }

    #[test]
    fn artifact_paths() {
        let d = Path::new("/t");
        assert_eq!(
            artifact(d, WINDOWS_TARGET, "app"),
            Path::new("/t/x86_64-pc-windows-msvc/release/app.exe")
        );
        assert_eq!(
            artifact(d, "universal2-apple-darwin", "app"),
            Path::new("/t/universal2-apple-darwin/release/app")
        );
        assert_eq!(
            artifact(d, "aarch64-pc-windows-msvc", "app"),
            Path::new("/t/aarch64-pc-windows-msvc/release/app.exe")
        );
        assert!(DIST_TARGETS.contains(&WINDOWS_TARGET));
    }
}
