//! Hardware-test payload. `cargo xtask hwkit` packs it for every slice
//! target, and `hwtest/run.sh` / `hwtest/run.ps1` run the packed file on
//! real machines and check what it reports.
//!
//! usage: `hwprobe [--exit N] [--stdin] [args...]`
//!
//! Prints one `key=value` line per fact, then exits with N (default 0):
//!
//! ```text
//! slice=Linux-x86_64          OS-arch this slice was built for
//! native=Linux-aarch64        the machine's own OS-arch as the OS reports it, or `unknown`
//! translated=1                slice arch != native arch (Rosetta, Prism, --emu), or `unknown`
//! win.native_machine=0xAA64   platform details (win.*, mac.*, linux.*, freebsd.*)
//! exe=/tmp/unibin.x/probe.com
//! argc=2
//! arg1=a b
//! arg2=c
//! stdin=hello                 first line of stdin, with --stdin
//! ```
//!
//! CR and LF inside values are printed as `\r` and `\n`, so every fact
//! stays on one line.

use std::io::{BufRead, Write};
use unibin_core::extract::current_host;

/// What the OS says about the machine, as opposed to what this slice was built for.
#[derive(Debug, Default)]
struct Platform {
    native: Option<String>,
    details: Vec<(&'static str, String)>,
}

fn clean(s: &str) -> String {
    s.replace('\r', "\\r").replace('\n', "\\n")
}

/// Build the report for `args` (without argv\[0\]); returns it and the exit code.
fn report(
    args: &[String],
    slice: &str,
    platform: &Platform,
    exe: &str,
    stdin: &mut dyn BufRead,
) -> (String, i32) {
    let (mut code, mut read_stdin, mut rest) = (0, false, args);
    let mut out = String::new();
    loop {
        match rest {
            [flag, n, tail @ ..] if flag == "--exit" => {
                code = n.parse().unwrap_or_else(|_| {
                    out.push_str(&format!("error=bad --exit value {n:?}\n"));
                    2
                });
                rest = tail;
            }
            [flag, tail @ ..] if flag == "--stdin" => {
                read_stdin = true;
                rest = tail;
            }
            _ => break,
        }
    }
    let arch = |key: &str| key.split_once('-').map(|(_, a)| a.to_owned());
    let translated = match &platform.native {
        Some(n) => u8::from(arch(n) != arch(slice)).to_string(),
        None => "unknown".to_owned(),
    };
    out.push_str(&format!("slice={slice}\n"));
    let native = platform.native.as_deref().unwrap_or("unknown");
    out.push_str(&format!("native={native}\ntranslated={translated}\n"));
    for (k, v) in &platform.details {
        out.push_str(&format!("{k}={}\n", clean(v)));
    }
    out.push_str(&format!("exe={}\nargc={}\n", clean(exe), rest.len()));
    for (i, a) in rest.iter().enumerate() {
        out.push_str(&format!("arg{}={}\n", i + 1, clean(a)));
    }
    if read_stdin {
        let mut line = String::new();
        let line = match stdin.read_line(&mut line) {
            Ok(_) => line.trim_end_matches(['\r', '\n']).to_owned(),
            Err(e) => format!("? ({e})"),
        };
        out.push_str(&format!("stdin={}\n", clean(&line)));
    }
    (out, code)
}

fn main() {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let exe = std::env::current_exe().unwrap_or_default();
    let exe = exe.display().to_string();
    let (text, code) = report(
        &args,
        &current_host(),
        &platform(),
        &exe,
        &mut std::io::stdin().lock(),
    );
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
    std::process::exit(code);
}

/// Linux: qemu-user fakes `uname(2)` for the guest, but not this file
/// (Linux 5.1+), so it still names the real machine under `--emu`.
#[cfg(target_os = "linux")]
fn platform() -> Platform {
    let arch = std::fs::read_to_string("/proc/sys/kernel/arch").ok();
    let arch = arch.as_deref().map(str::trim).unwrap_or_default();
    let native = (!arch.is_empty()).then(|| unibin_core::extract::host_key("linux", arch));
    Platform {
        native,
        details: vec![("linux.kernel_arch", arch.to_owned())],
    }
}

#[cfg(any(target_os = "macos", target_os = "freebsd"))]
mod sysctl {
    use std::ffi::{CStr, c_char, c_int, c_void};

    unsafe extern "C" {
        fn sysctlbyname(
            name: *const c_char,
            oldp: *mut c_void,
            oldlenp: *mut usize,
            newp: *mut c_void,
            newlen: usize,
        ) -> c_int;
    }

    /// Read a sysctl into `buf`; returns the length written.
    fn raw(name: &CStr, buf: &mut [u8]) -> Option<usize> {
        let mut len = buf.len();
        // SAFETY: valid NUL-terminated name, buffer and length; no new value.
        let r = unsafe {
            sysctlbyname(
                name.as_ptr(),
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (r == 0).then_some(len)
    }

    #[cfg(target_os = "macos")]
    pub fn int(name: &CStr) -> Option<i32> {
        let mut buf = [0u8; 4];
        raw(name, &mut buf).filter(|&n| n == 4)?;
        Some(i32::from_ne_bytes(buf))
    }

    pub fn string(name: &CStr) -> Option<String> {
        let mut buf = [0u8; 256];
        let n = raw(name, &mut buf)?;
        let s = String::from_utf8_lossy(&buf[..n]);
        Some(s.trim_end_matches('\0').to_owned())
    }
}

/// macOS: `hw.optional.arm64` is 1 on Apple Silicon even inside Rosetta;
/// `sysctl.proc_translated` is 1 for a translated process (absent on Intel).
#[cfg(target_os = "macos")]
fn platform() -> Platform {
    let show = |v: Option<i32>| v.map_or("absent".to_owned(), |v| v.to_string());
    let arm64 = sysctl::int(c"hw.optional.arm64");
    let translated = sysctl::int(c"sysctl.proc_translated");
    let arch = if arm64 == Some(1) {
        "aarch64"
    } else {
        "x86_64"
    };
    let release = sysctl::string(c"kern.osproductversion").unwrap_or_default();
    Platform {
        native: Some(unibin_core::extract::host_key("macos", arch)),
        details: vec![
            ("mac.hw.optional.arm64", show(arm64)),
            ("mac.proc_translated", show(translated)),
            ("mac.os_version", release),
        ],
    }
}

/// FreeBSD has no translator; report the kernel's view and the ABI version
/// so a wrong `EI_OSABI` or libc floor shows up as a failed run, not a guess.
#[cfg(target_os = "freebsd")]
fn platform() -> Platform {
    let arch = sysctl::string(c"hw.machine_arch").unwrap_or_default();
    let release = sysctl::string(c"kern.osrelease").unwrap_or_default();
    let native = (!arch.is_empty()).then(|| unibin_core::extract::host_key("freebsd", &arch));
    Platform {
        native,
        details: vec![
            ("freebsd.machine_arch", arch),
            ("freebsd.osrelease", release),
        ],
    }
}

/// Windows: the same `IsWow64Process2` query the launcher uses to pick a
/// slice. Under Prism an x86_64 process should still see `0xAA64` as the
/// native machine; this is what the hardware test verifies.
#[cfg(windows)]
fn platform() -> Platform {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn IsWow64Process2(
            process: *mut c_void,
            process_machine: *mut u16,
            native: *mut u16,
        ) -> i32;
    }
    let (mut process, mut native) = (0u16, 0u16);
    // SAFETY: the current-process pseudo-handle and two valid out-pointers.
    let ok = unsafe { IsWow64Process2(GetCurrentProcess(), &mut process, &mut native) };
    if ok == 0 {
        let err = std::io::Error::last_os_error().to_string();
        return Platform {
            native: None,
            details: vec![("win.IsWow64Process2", format!("failed: {err}"))],
        };
    }
    Platform {
        native: Some(unibin_core::extract::windows_host(native)),
        details: vec![
            ("win.process_machine", format!("0x{process:04X}")),
            ("win.native_machine", format!("0x{native:04X}")),
        ],
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    windows
)))]
fn platform() -> Platform {
    Platform::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn run(args: &[&str], native: Option<&str>, stdin: &str) -> (String, i32) {
        let p = Platform {
            native: native.map(str::to_owned),
            details: vec![("win.native_machine", "0xAA64".to_owned())],
        };
        report(
            &strings(args),
            "Windows-x86_64",
            &p,
            "C:\\t\\probe.com",
            &mut stdin.as_bytes(),
        )
    }

    #[test]
    fn reports_slice_args_and_exit_code() {
        let (out, code) = run(
            &["--exit", "42", "--stdin", "a b", "", "x\ny"],
            Some("Windows-aarch64"),
            "hello\r\nmore\n",
        );
        assert_eq!(code, 42);
        assert_eq!(
            out,
            "slice=Windows-x86_64\nnative=Windows-aarch64\ntranslated=1\n\
             win.native_machine=0xAA64\nexe=C:\\t\\probe.com\nargc=3\n\
             arg1=a b\narg2=\narg3=x\\ny\nstdin=hello\n"
        );
    }

    #[test]
    fn options_only_count_before_the_first_argument() {
        let (out, code) = run(&["x", "--exit", "3"], None, "");
        assert_eq!(code, 0);
        assert!(
            out.contains("native=unknown\ntranslated=unknown\n"),
            "{out}"
        );
        assert!(
            out.ends_with("argc=3\narg1=x\narg2=--exit\narg3=3\n"),
            "{out}"
        );
        assert!(!out.contains("stdin="));
    }

    #[test]
    fn native_slice_is_not_translated() {
        let (out, _) = run(&[], Some("Windows-x86_64"), "");
        assert!(out.contains("translated=0\n"), "{out}");
    }

    #[test]
    fn bad_exit_value() {
        let (out, code) = run(&["--exit", "x"], None, "");
        assert_eq!(code, 2);
        assert!(out.starts_with("error=bad --exit value \"x\"\n"), "{out}");
    }

    #[test]
    fn unreadable_stdin_is_reported() {
        let p = Platform::default();
        let (out, _) = report(&strings(&["--stdin"]), "s", &p, "e", &mut &b"\xFF\n"[..]);
        assert!(
            out.ends_with("stdin=? (stream did not contain valid UTF-8)\n"),
            "{out}"
        );
    }

    #[test]
    fn platform_names_this_machine() {
        let Platform { native, details } = platform();
        assert!(native.is_some_and(|n| n.contains('-')), "{details:?}");
    }
}
