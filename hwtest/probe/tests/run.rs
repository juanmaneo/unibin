//! Run the real binary: argv, stdin, stdout and the exit code all go
//! through `main`.

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn binary_reports_and_exits() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_hwprobe"))
        .args(["--exit", "3", "--stdin", "a b"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"hi\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(3), "{text}");
    assert!(text.contains("\nexe="), "{text}");
    assert!(text.ends_with("argc=1\narg1=a b\nstdin=hi\n"), "{text}");
}
