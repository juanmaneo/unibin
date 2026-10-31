//! Run the real `unibin-launch` binary with a ZIP tail appended, the way it
//! runs inside a packed file (on Windows it is the PE image; the logic is
//! the same on every host).
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use unibin_core::extract::current_host;
use unibin_core::manifest::Manifest;
use unibin_core::zip::ZipWriter;

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("launch-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("tmp")).unwrap();
    dir
}

#[test]
fn launcher_binary_runs_its_slice() {
    let dir = scratch("ok");
    let launcher = fs::read(env!("CARGO_BIN_EXE_unibin-launch")).unwrap();
    let mut zip = ZipWriter::new(launcher);
    let script = b"#!/bin/sh\necho \"slice ${0##*/}: $*\"\nexit 5\n";
    let mut manifest = Manifest::default();
    manifest
        .slices
        .push((current_host(), zip.add("slice", script).unwrap()));
    let exe = dir.join("tool");
    fs::write(&exe, zip.finish(manifest.render().as_bytes()).unwrap()).unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();

    let o = Command::new(&exe)
        .args(["one", "two"])
        .env("TMPDIR", dir.join("tmp"))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&o.stdout), "slice tool: one two\n");
    assert_eq!(
        o.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert_eq!(
        fs::read_dir(dir.join("tmp")).unwrap().count(),
        0,
        "temp dir not removed"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn bare_launcher_reports_missing_payload() {
    let o = Command::new(env!("CARGO_BIN_EXE_unibin-launch"))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(126));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        err.starts_with("unibin-launch: no ZIP end-of-central-directory record"),
        "{err}"
    );
}
