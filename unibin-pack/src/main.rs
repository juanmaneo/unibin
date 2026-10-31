//! `unibin-pack --launcher <pe> --slice <file>[=OS-arch]... [--emu <fex>] -o <out>`

use std::io::Write;
use std::process::ExitCode;
use unibin_core::manifest::check_key;
use unibin_core::pack::{Input, Slice, pack};
use unibin_core::{Error, Result};

const USAGE: &str = "\
usage: unibin-pack --launcher <unibin-launch.exe> --slice <file>[=OS-arch]... [--emu <FEX-static>] -o <out>

  --launcher <pe>       unsigned x86_64 Windows launcher stub (unibin-launch.exe)
  --slice <file>[=KEY]  native executable; KEY (e.g. Linux-x86_64) is detected from
                        the file and, if given, must match it. Repeatable.
  --emu <file>          static Linux-aarch64 x86_64 emulator (FEX), used on
                        Linux/aarch64 hosts when there is no Linux-aarch64 slice
  -o, --output <out>    output file (written with mode 0755)

Sign the output (signtool) only after packing.";

fn main() -> ExitCode {
    let args = std::env::args().skip(1).collect();
    ExitCode::from(run_main_with(
        args,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    ))
}

#[derive(Debug, Default, PartialEq)]
struct Args {
    launcher: String,
    slices: Vec<String>,
    emu: Option<String>,
    out: String,
}

/// `Ok(None)` means `--help`.
fn parse_args(args: Vec<String>) -> Result<Option<Args>> {
    let (mut launcher, mut out, mut parsed) = (None, None, Args::default());
    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        if flag == "-h" || flag == "--help" {
            return Ok(None);
        }
        let value = it
            .next()
            .ok_or_else(|| Error::new(format!("{flag} needs a value")));
        match flag.as_str() {
            "--launcher" => launcher = Some(value?),
            "--slice" => parsed.slices.push(value?),
            "--emu" => parsed.emu = Some(value?),
            "-o" | "--output" => out = Some(value?),
            _ => return Err(Error::new(format!("unknown argument {flag:?}\n{USAGE}"))),
        }
    }
    parsed.launcher =
        launcher.ok_or_else(|| Error::new(format!("--launcher is required\n{USAGE}")))?;
    parsed.out = out.ok_or_else(|| Error::new(format!("-o is required\n{USAGE}")))?;
    Ok(Some(parsed))
}

/// Split `file[=KEY]`; a suffix only counts as a key if it looks like one,
/// so file names containing `=` still work.
fn split_slice_arg(arg: &str) -> (&str, Option<&str>) {
    match arg.rsplit_once('=') {
        Some((file, key)) if check_key(key).is_ok() => (file, Some(key)),
        _ => (arg, None),
    }
}

fn read(path: &str) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| Error::new(format!("cannot read {path}: {e}")))
}

fn write_executable(path: &str, bytes: &[u8]) -> Result<()> {
    let fail = |e: std::io::Error| Error::new(format!("cannot write {path}: {e}"));
    std::fs::write(path, bytes).map_err(fail)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(fail)?;
    }
    Ok(())
}

fn run(args: Args, out: &mut dyn Write) -> Result<()> {
    let mut input = Input {
        launcher: read(&args.launcher)?,
        ..Input::default()
    };
    for arg in &args.slices {
        let (file, key) = split_slice_arg(arg);
        let slice = Slice::new(read(file)?, key).map_err(|e| Error::new(format!("{file}: {e}")))?;
        input.slices.push(slice);
    }
    input.emu = args.emu.as_deref().map(read).transpose()?;
    let bytes = pack(&input)?;
    write_executable(&args.out, &bytes)?;
    let keys: Vec<&str> = input
        .slices
        .iter()
        .flat_map(|s| &s.keys)
        .map(String::as_str)
        .collect();
    let emu = if input.emu.is_some() {
        " (+ Linux-aarch64 emulator)"
    } else {
        ""
    };
    let (path, size, keys) = (&args.out, bytes.len(), keys.join(", "));
    writeln!(out, "unibin-pack: wrote {path} ({size} bytes): {keys}{emu}")?;
    Ok(())
}

/// Entry point with injectable I/O; returns the process exit code.
fn run_main_with(args: Vec<String>, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let result = match parse_args(args) {
        Ok(None) => writeln!(out, "{USAGE}").map_err(Error::from),
        Ok(Some(args)) => run(args, out),
        Err(e) => Err(e),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(err, "unibin-pack: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn run_capture(a: &[&str]) -> (u8, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_main_with(strings(a), &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn parses_arguments() {
        let a = parse_args(strings(&[
            "--launcher",
            "l.exe",
            "--slice",
            "a",
            "--slice",
            "b=Linux-x86_64",
            "--emu",
            "fex",
            "--output",
            "o",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            a,
            Args {
                launcher: "l.exe".into(),
                slices: strings(&["a", "b=Linux-x86_64"]),
                emu: Some("fex".into()),
                out: "o".into(),
            }
        );
        assert_eq!(parse_args(strings(&["--slice", "x", "-h"])).unwrap(), None);
    }

    #[test]
    fn argument_errors() {
        let e = |a: &[&str]| parse_args(strings(a)).unwrap_err().0;
        assert!(e(&["--slice"]).contains("--slice needs a value"));
        assert!(e(&["--bogus", "x"]).contains("unknown argument \"--bogus\""));
        assert!(e(&["-o", "x"]).contains("--launcher is required"));
        assert!(e(&["--launcher", "x"]).contains("-o is required"));
    }

    #[test]
    fn slice_argument_split() {
        assert_eq!(
            split_slice_arg("bin/app=Linux-x86_64"),
            ("bin/app", Some("Linux-x86_64"))
        );
        assert_eq!(split_slice_arg("bin/app"), ("bin/app", None));
        assert_eq!(split_slice_arg("dir=x/app"), ("dir=x/app", None));
    }

    #[test]
    fn help_and_errors_through_entry_point() {
        let (code, out, _) = run_capture(&["--help"]);
        assert_eq!(code, 0);
        assert!(out.starts_with("usage: unibin-pack"));
        let (code, _, err) = run_capture(&["--nope"]);
        assert_eq!(code, 1);
        assert!(err.starts_with("unibin-pack: unknown argument"));
    }

    #[test]
    fn missing_files() {
        let (code, _, err) = run_capture(&["--launcher", "/nonexistent/l.exe", "-o", "/tmp/x"]);
        assert_eq!(code, 1);
        assert!(err.contains("cannot read /nonexistent/l.exe"), "{err}");
    }

    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn help_to_closed_stdout_fails() {
        let mut err = Vec::new();
        assert_eq!(run_main_with(strings(&["-h"]), &mut Broken, &mut err), 1);
        assert_eq!(Broken.flush().ok(), Some(()));
    }

    #[test]
    fn write_errors() {
        let e = write_executable("/nonexistent-dir/out", b"x").unwrap_err();
        assert!(e.0.contains("cannot write /nonexistent-dir/out"), "{e}");
    }
}
