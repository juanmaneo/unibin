//! End-to-end: pack real and fake slices with the `poly-pack` binary and run
//! the result through the POSIX shell prologue.
#![cfg(target_os = "linux")]

use poly_core::extract::Archive;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const PACK: &str = env!("CARGO_BIN_EXE_poly-pack");
const MARKER: &str = "hello from poly";

fn scratch(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("e2e-{name}-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("tmp")).unwrap();
    dir
}

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// Minimal PE32+ launcher: one .text section at RVA 0x1000.
fn fake_pe(machine: u16) -> Vec<u8> {
    let mut b = vec![0u8; 0x400];
    b[..2].copy_from_slice(b"MZ");
    put32(&mut b, 0x3C, 0x80);
    b[0x80..0x84].copy_from_slice(b"PE\0\0");
    put16(&mut b, 0x84, machine);
    put16(&mut b, 0x86, 1);
    put16(&mut b, 0x94, 0xF0);
    let opt = 0x98;
    put16(&mut b, opt, 0x20B);
    put32(&mut b, opt + 32, 0x1000);
    put32(&mut b, opt + 36, 0x200);
    put32(&mut b, opt + 60, 0x200);
    put32(&mut b, opt + 108, 16);
    let s = opt + 0xF0;
    b[s..s + 5].copy_from_slice(b".text");
    put32(&mut b, s + 12, 0x1000);
    put32(&mut b, s + 16, 0x200);
    put32(&mut b, s + 20, 0x200);
    b
}

fn fake_elf(osabi: u8, machine: u16) -> Vec<u8> {
    let mut b = vec![0u8; 256];
    b[..8].copy_from_slice(&[0x7F, b'E', b'L', b'F', 2, 1, 1, osabi]);
    put16(&mut b, 18, machine);
    b
}

fn fake_macho_x86_64() -> Vec<u8> {
    let mut b = vec![0u8; 256];
    b[..8].copy_from_slice(&[0xCF, 0xFA, 0xED, 0xFE, 7, 0, 0, 1]);
    b
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> String {
    let p = dir.join(name);
    fs::write(&p, bytes).unwrap();
    p.to_string_lossy().into_owned()
}

/// A real static Linux program: prints the marker and its arguments,
/// exits with the argument count.
fn hello(dir: &Path) -> String {
    let src = dir.join("hello.c");
    fs::write(
        &src,
        "#include <stdio.h>\nint main(int c, char **v) { printf(\"hello from poly\");\n\
         for (int i = 1; i < c; i++) printf(\" [%s]\", v[i]); printf(\"\\n\"); return c - 1; }\n",
    )
    .unwrap();
    let out = dir.join("hello");
    let st = Command::new("cc")
        .args(["-static", "-O1", "-o"])
        .arg(&out)
        .arg(&src)
        .status();
    assert!(
        st.expect("a C compiler (cc) is required").success(),
        "cc -static failed"
    );
    out.to_string_lossy().into_owned()
}

fn poly_pack(args: &[&str]) -> Output {
    Command::new(PACK).args(args).output().unwrap()
}

fn pack_ok(args: &[&str]) {
    let o = poly_pack(args);
    assert!(
        o.status.success(),
        "poly-pack failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(String::from_utf8_lossy(&o.stdout).starts_with("poly-pack: wrote "));
}

fn sh(dir: &Path, program: &str, script: &str, args: &[&str]) -> Output {
    Command::new(program)
        .arg("-c")
        .arg(script)
        .args(args)
        .env("TMPDIR", dir.join("tmp"))
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// The shell's watcher removes the extraction directory once the program exits.
fn wait_for_cleanup(dir: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while fs::read_dir(dir.join("tmp")).unwrap().count() > 0 {
        assert!(Instant::now() < deadline, "temp dir not cleaned up");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn real_static_binary_runs_through_shells() {
    let dir = scratch("native");
    let launcher = write(&dir, "launch.exe", &fake_pe(0x8664));
    let hello = hello(&dir);
    let freebsd = write(&dir, "fbsd", &fake_elf(9, 62));
    let out = dir.join("app");
    let out_s = out.to_string_lossy();
    pack_ok(&[
        "--launcher",
        &launcher,
        "--slice",
        &hello,
        "--slice",
        &freebsd,
        "-o",
        &out_s,
    ]);

    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&out).unwrap().permissions().mode() & 0o777,
        0o755
    );

    // Explicit interpreter.
    let o = Command::new("/bin/sh")
        .arg(&out)
        .args(["a b", "c"])
        .env("TMPDIR", dir.join("tmp"))
        .output()
        .unwrap();
    assert_eq!(
        stdout(&o),
        format!("{MARKER} [a b] [c]\n"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert_eq!(o.status.code(), Some(2));
    wait_for_cleanup(&dir);

    // execve() fails with ENOEXEC; the shell then runs the file as a script.
    let mut shells = vec!["bash", "sh"];
    if Command::new("zsh")
        .arg("-c")
        .arg("true")
        .status()
        .is_ok_and(|s| s.success())
    {
        shells.push("zsh");
    }
    let direct_exec = !mz_binfmt_handler(&dir);
    if !direct_exec {
        eprintln!("note: a binfmt_misc handler (e.g. wine) claims MZ files; skipping direct exec");
        shells.clear();
    }
    for shell in shells {
        let o = sh(&dir, shell, "\"$0\" \"$@\"", &[&out_s, "x"]);
        assert_eq!(
            stdout(&o),
            format!("{MARKER} [x]\n"),
            "{shell}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        assert_eq!(o.status.code(), Some(1), "{shell}");
    }

    // Found through $PATH: the prologue resolves $0 with `command -v`. Without
    // direct exec, bash (unlike dash) also searches $PATH for its script
    // operand and leaves $0 slash-less, so the same branch is exercised.
    let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap());
    let by_name = if direct_exec { "app" } else { "bash app" };
    let o = Command::new("sh")
        .args(["-c", by_name])
        .env("PATH", path)
        .env("TMPDIR", dir.join("tmp"))
        .output()
        .unwrap();
    assert_eq!(
        stdout(&o),
        format!("{MARKER}\n"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    wait_for_cleanup(&dir);

    // The file is also a valid ZIP for standard tools.
    if let Ok(o) = Command::new("unzip")
        .arg("-p")
        .arg(&out)
        .arg("Linux-x86_64")
        .output()
    {
        assert_eq!(o.stdout, fs::read(&hello).unwrap());
    }

    // Data appended after packing (signtool's certificate table) does not
    // move the EOCD offset baked into the prologue.
    let mut signed = fs::read(&out).unwrap();
    signed.extend_from_slice(&[0xAB; 24]);
    fs::write(&out, signed).unwrap();
    let o = Command::new("/bin/sh")
        .arg(&out)
        .env("TMPDIR", dir.join("tmp"))
        .output()
        .unwrap();
    assert_eq!(
        stdout(&o),
        format!("{MARKER}\n"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    wait_for_cleanup(&dir);
    let _ = fs::remove_dir_all(&dir);
}

/// True if the kernel hands `MZ` files to an interpreter (wine's binfmt_misc
/// registration), in which case `execve` never reaches the shell fallback.
fn mz_binfmt_handler(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let probe = dir.join("mz-probe");
    fs::write(&probe, "MZ=1\necho plain-sh\n").unwrap();
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).unwrap();
    let o = Command::new("sh")
        .arg("-c")
        .arg("\"$0\"")
        .arg(&probe)
        .output()
        .unwrap();
    stdout(&o) != "plain-sh\n"
}

/// Overwrite a slice in the packed file with a shell script (same length),
/// so fake foreign slices can "run" on this host.
fn patch(file: &Path, key: &str, script: &str) {
    let mut data = fs::read(file).unwrap();
    let a = Archive::open(&data).unwrap();
    let span = match key {
        "emu" => a.manifest.emu.clone().unwrap().1,
        _ => a.manifest.get(key).unwrap(),
    };
    let mut body = script.as_bytes().to_vec();
    assert!(body.len() <= span.len as usize);
    body.resize(span.len as usize, b'\n');
    data[span.off as usize..(span.off + span.len) as usize].copy_from_slice(&body);
    fs::write(file, data).unwrap();
}

#[test]
fn host_selection_and_fallbacks() {
    let dir = scratch("hosts");
    let launcher = write(&dir, "launch.exe", &fake_pe(0x8664));
    let slices = [
        write(&dir, "linux", &fake_elf(0, 62)),
        write(&dir, "fbsd", &fake_elf(9, 62)),
        write(&dir, "mac", &fake_macho_x86_64()),
    ];
    let emu = write(&dir, "fex", &fake_elf(0, 183));
    let out = dir.join("multi");
    let out_s = out.to_string_lossy();
    pack_ok(&[
        "--launcher",
        &launcher,
        "--slice",
        &slices[0],
        "--slice",
        &format!("{}=FreeBSD-x86_64", slices[1]),
        "--slice",
        &slices[2],
        "--emu",
        &emu,
        "-o",
        &out_s,
    ]);
    patch(
        &out,
        "Linux-x86_64",
        "#!/bin/sh\necho linux \"${0##*/}\" \"$@\"\n",
    );
    patch(&out, "FreeBSD-x86_64", "#!/bin/sh\necho freebsd \"$@\"\n");
    patch(
        &out,
        "Darwin-x86_64",
        "#!/bin/sh\necho darwin-x86_64 \"$@\"\n",
    );
    patch(&out, "emu", "#!/bin/sh\necho fex \"${1##*/}\" \"$2\"\n");

    let fake = dir.join("fakebin");
    fs::create_dir(&fake).unwrap();
    let uname = write(
        &fake,
        "uname",
        b"#!/bin/sh\ncase $1 in -s) echo \"$FAKE_S\";; -m) echo \"$FAKE_M\";; esac\n",
    );
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(uname, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", fake.display(), std::env::var("PATH").unwrap());
    let run = |s: &str, m: &str| {
        let o = Command::new("/bin/sh")
            .arg(&out)
            .arg("arg1")
            .env("PATH", &path)
            .env("FAKE_S", s)
            .env("FAKE_M", m)
            .env("TMPDIR", dir.join("tmp"))
            .output()
            .unwrap();
        (
            o.status.code(),
            stdout(&o),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        )
    };

    assert_eq!(run("Linux", "x86_64").1, "linux multi arg1\n");
    assert_eq!(run("FreeBSD", "amd64").1, "freebsd arg1\n");
    assert_eq!(run("Darwin", "x86_64").1, "darwin-x86_64 arg1\n");
    // Apple silicon without an arm64 slice: Rosetta 2 runs the x86_64 one.
    assert_eq!(run("Darwin", "arm64").1, "darwin-x86_64 arg1\n");
    // Linux/aarch64 without a native slice: the bundled emulator runs x86_64.
    assert_eq!(run("Linux", "aarch64").1, "fex multi arg1\n");
    let (code, out_txt, err) = run("FreeBSD", "arm64");
    assert_eq!((code, out_txt.as_str()), (Some(126), ""));
    assert!(err.contains("has no slice for FreeBSD-aarch64"), "{err}");
    wait_for_cleanup(&dir);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn damaged_archive_is_reported() {
    let dir = scratch("damaged");
    let launcher = write(&dir, "launch.exe", &fake_pe(0x8664));
    let linux = write(&dir, "linux", &fake_elf(0, 62));
    let out = dir.join("broken");
    pack_ok(&[
        "--launcher",
        &launcher,
        "--slice",
        &linux,
        "-o",
        &out.to_string_lossy(),
    ]);
    let mut data = fs::read(&out).unwrap();
    let eocd = poly_core::zip::find_eocd(&data).unwrap().offset as usize;
    data[eocd] = b'X';
    fs::write(&out, data).unwrap();
    let o = Command::new("/bin/sh")
        .arg(&out)
        .env("TMPDIR", dir.join("tmp"))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(126));
    assert!(String::from_utf8_lossy(&o.stderr).contains("damaged archive"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cli_rejects_bad_input() {
    let dir = scratch("cli");
    let launcher = write(&dir, "launch.exe", &fake_pe(0x8664));
    let arm_launcher = write(&dir, "arm.exe", &fake_pe(0xAA64));
    let linux = write(&dir, "linux", &fake_elf(0, 62));
    let out = dir.join("o").to_string_lossy().into_owned();
    let err = |args: &[&str]| {
        let o = poly_pack(args);
        assert_eq!(o.status.code(), Some(1), "{args:?}");
        String::from_utf8_lossy(&o.stderr).into_owned()
    };
    let wrong_os = format!("{linux}=FreeBSD-x86_64");
    assert!(
        err(&["--launcher", &launcher, "--slice", &wrong_os, "-o", &out])
            .contains("but its content is Linux-x86_64")
    );
    assert!(
        err(&[
            "--launcher",
            &launcher,
            "--slice",
            &linux,
            "--slice",
            &linux,
            "-o",
            &out
        ])
        .contains("duplicate slice")
    );
    assert!(
        err(&["--launcher", &arm_launcher, "--slice", &linux, "-o", &out])
            .contains("must be x86_64")
    );
    assert!(
        err(&[
            "--launcher",
            &launcher,
            "--slice",
            "/nonexistent",
            "-o",
            &out
        ])
        .contains("cannot read /nonexistent")
    );
    assert!(
        err(&[
            "--launcher",
            &launcher,
            "--emu",
            "/nonexistent",
            "--slice",
            &linux,
            "-o",
            &out
        ])
        .contains("cannot read")
    );
    assert!(
        err(&[
            "--launcher",
            &launcher,
            "--slice",
            &launcher,
            "--slice",
            &linux,
            "-o",
            "/nonexistent/o"
        ])
        .contains("cannot write")
    );
    assert!(err(&["--launcher", &launcher, "--slice", &out, "-o", &out]).contains("cannot read"));
    let help = poly_pack(&["--help"]);
    assert!(help.status.success() && stdout(&help).contains("usage: poly-pack"));
    let _ = fs::remove_dir_all(&dir);
}
