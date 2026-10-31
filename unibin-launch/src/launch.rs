use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use unibin_core::extract::{Archive, Plan, plan, temp_dir};
use unibin_core::{Error, Result};

/// Exit code when the launcher itself fails (mirrors the shell's 126).
pub const LAUNCH_FAILED: i32 = 126;

/// Map a child's status to our exit code; signals become `128 + n` like sh.
fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    if let Some(sig) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return 128 + sig;
    }
    status.code().unwrap_or(LAUNCH_FAILED)
}

fn execute(
    archive: &Archive,
    plan: &Plan,
    dir: &Path,
    name: &str,
    args: Vec<OsString>,
) -> Result<i32> {
    let argv = archive.materialize(plan, dir, name)?;
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .args(args)
        .status()
        .map_err(|e| Error::new(format!("cannot run {}: {e}", argv[0].display())))?;
    Ok(exit_code(status))
}

fn run(exe: io::Result<PathBuf>, args: Vec<OsString>, host: &str, tmp: &Path) -> Result<i32> {
    let exe = exe.map_err(|e| Error::new(format!("cannot locate own executable: {e}")))?;
    let data = std::fs::read(&exe)
        .map_err(|e| Error::new(format!("cannot read {}: {e}", exe.display())))?;
    let archive = Archive::open(&data)?;
    let plan = plan(&archive.manifest, host)?;
    let name = exe
        .file_name()
        .map_or("unibin".into(), |n| n.to_string_lossy().into_owned());
    let dir = temp_dir(tmp)?;
    let result = execute(&archive, &plan, &dir, &name, args);
    // The child has exited, so its image is no longer locked (Windows) and can go.
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// Entry point with injectable environment; returns the process exit code.
pub fn run_main_with(
    exe: io::Result<PathBuf>,
    args: Vec<OsString>,
    host: &str,
    tmp: &Path,
    err: &mut dyn Write,
) -> i32 {
    run(exe, args, host, tmp).unwrap_or_else(|e| {
        let _ = writeln!(err, "unibin-launch: {e}");
        LAUNCH_FAILED
    })
}

/// Ctrl-C is delivered to every process on the console. Swallow it here so
/// the launcher outlives the child (which decides how to react) and can
/// still clean up and forward the exit code. A handler, unlike
/// `SetConsoleCtrlHandler(NULL, TRUE)`, is not inherited by the child.
#[cfg(windows)]
pub fn forward_ctrl_c_to_child() {
    unsafe extern "system" fn swallow(_ctrl_type: u32) -> i32 {
        1
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }
    // SAFETY: registers a static function pointer; no other invariants.
    unsafe {
        SetConsoleCtrlHandler(Some(swallow), 1);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::{Mutex, MutexGuard, PoisonError};
    use unibin_core::manifest::Manifest;
    use unibin_core::zip::ZipWriter;

    /// The launcher is single-threaded; the test harness is not. A fork in one
    /// test thread can inherit another thread's still-open write fd to a fresh
    /// slice and make its exec fail with ETXTBSY, so tests run one at a time.
    static SERIAL: Mutex<()> = Mutex::new(());

    struct Fixture {
        dir: PathBuf,
        _serial: MutexGuard<'static, ()>,
    }

    impl Fixture {
        fn new() -> Self {
            let serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
            Fixture {
                dir: temp_dir(&std::env::temp_dir()).unwrap(),
                _serial: serial,
            }
        }

        /// Write a fake packed file whose `Linux-x86_64` slice is `script`.
        fn packed(&self, name: &str, script: &str, emu: Option<&str>) -> PathBuf {
            let mut z = ZipWriter::new(b"not really a launcher\n".to_vec());
            let mut m = Manifest::default();
            m.slices.push((
                "Linux-x86_64".into(),
                z.add("Linux-x86_64", script.as_bytes()).unwrap(),
            ));
            if let Some(emu) = emu {
                m.emu = Some((
                    "Linux-aarch64".into(),
                    z.add("emu", emu.as_bytes()).unwrap(),
                ));
            }
            let path = self.dir.join(name);
            fs::write(&path, z.finish(m.render().as_bytes()).unwrap()).unwrap();
            path
        }

        fn tmp(&self) -> PathBuf {
            let t = self.dir.join("tmp");
            fs::create_dir_all(&t).unwrap();
            t
        }

        fn run(&self, exe: &Path, args: &[&str], host: &str) -> (i32, String) {
            let mut err = Vec::new();
            let args = args.iter().map(OsString::from).collect();
            let code = run_main_with(Ok(exe.to_owned()), args, host, &self.tmp(), &mut err);
            (code, String::from_utf8(err).unwrap())
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn runs_native_slice_and_forwards_exit_code() {
        let f = Fixture::new();
        let out = f.dir.join("out.txt");
        let script = format!("#!/bin/sh\necho \"$0|$*\" > {}\nexit 7\n", out.display());
        let exe = f.packed("tool", &script, None);
        let (code, err) = f.run(&exe, &["a b", "c"], "Linux-x86_64");
        assert_eq!((code, err.as_str()), (7, ""));
        let line = fs::read_to_string(&out).unwrap();
        assert!(line.ends_with("/tool|a b c\n"), "{line}");
        // The extraction directory is removed after the child exits.
        assert_eq!(fs::read_dir(f.tmp()).unwrap().count(), 0);
    }

    #[test]
    fn runs_emulated_slice() {
        let f = Fixture::new();
        let out = f.dir.join("emu.txt");
        let emu = format!(
            "#!/bin/sh\necho \"emu:${{1##*/}}:$2\" > {}\n",
            out.display()
        );
        let exe = f.packed("prog", "#!/bin/sh\nexit 1\n", Some(&emu));
        assert_eq!(f.run(&exe, &["x"], "Linux-aarch64").0, 0);
        assert_eq!(fs::read_to_string(&out).unwrap(), "emu:prog:x\n");
    }

    #[test]
    fn signals_map_to_128_plus_n() {
        let f = Fixture::new();
        let exe = f.packed("sig", "#!/bin/sh\nkill -TERM $$\n", None);
        assert_eq!(f.run(&exe, &[], "Linux-x86_64").0, 128 + 15);
    }

    #[test]
    fn failures_exit_126_with_message() {
        let f = Fixture::new();
        let exe = f.packed("t", "#!/bin/sh\n", None);
        let (code, err) = f.run(&exe, &[], "FreeBSD-x86_64");
        assert_eq!(code, LAUNCH_FAILED);
        assert!(
            err.starts_with("unibin-launch: no slice for FreeBSD-x86_64"),
            "{err}"
        );

        let (code, err) = f.run(&f.dir.join("missing"), &[], "Linux-x86_64");
        assert_eq!(code, LAUNCH_FAILED);
        assert!(err.contains("cannot read"), "{err}");

        let plain = f.dir.join("plain");
        fs::write(&plain, b"no zip here at all, just text").unwrap();
        assert!(
            f.run(&plain, &[], "Linux-x86_64")
                .1
                .contains("end-of-central-directory")
        );

        // Slice that is not executable by the OS.
        let exe = f.packed("junk", "\0\0\0\0 not a program", None);
        let (code, err) = f.run(&exe, &[], "Linux-x86_64");
        assert_eq!(code, LAUNCH_FAILED);
        assert!(err.contains("cannot run"), "{err}");

        let mut err = Vec::new();
        let code = run_main_with(
            Err(io::ErrorKind::NotFound.into()),
            vec![],
            "x",
            &f.tmp(),
            &mut err,
        );
        assert_eq!(code, LAUNCH_FAILED);
        assert!(
            String::from_utf8(err)
                .unwrap()
                .contains("cannot locate own executable")
        );

        // Unwritable temp directory.
        let mut err = Vec::new();
        let tmp = f.dir.join("no/such/dir");
        assert_eq!(
            run_main_with(Ok(exe), vec![], "Linux-x86_64", &tmp, &mut err),
            LAUNCH_FAILED
        );
    }
}
