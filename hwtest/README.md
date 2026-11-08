# unibin hardware test kit

These checks need real machines: macOS, FreeBSD, Windows on Arm and arm64
Linux, none of which CI or the dev container can stand in for. The kit holds
two packed copies of `hwprobe`, a tiny program that prints which slice ran,
what the OS says about the machine, and the arguments and stdin it got.

| File               | Slices                                                     |
|--------------------|------------------------------------------------------------|
| `probe.com`        | every target, as `cargo xtask dist` packs them             |
| `probe-x86_64.com` | x86_64 only (thin x86_64 Mach-O) + the `--emu` emulator, if the kit was built with it |
| `run.sh`           | checks for Linux, FreeBSD and macOS                        |
| `run.ps1`          | checks for Windows 11                                      |

## Build the kit

In the dev container (it needs cargo-zigbuild, cargo-xwin and the pinned
nightly; the first FreeBSD arm64 build fetches std's crates):

```sh
podman run --rm -v "$PWD":/src:Z -v unibin-target:/src/target unibin-dev \
    sh -c 'cargo xtask hwkit --emu "$UNIBIN_EMU" -o /src/hwkit'
```

The kit lands in `hwkit/` (git-ignored; the default, `target/hwkit`, would
end up in the container's volume). It is about 17 MB, mostly the emulator.
Copy the whole directory to each machine (scp, a USB stick, or
`tar czf hwkit.tgz hwkit`; Windows 11 has `tar` built in).

## Run it

| Machine                     | Command                                               |
|-----------------------------|-------------------------------------------------------|
| Linux, FreeBSD, macOS       | `sh run.sh`                                           |
| Windows 11 x64 / arm64      | `powershell -ExecutionPolicy Bypass -File .\run.ps1`   |

Each run prints `PASS`/`FAIL`/`SKIP` lines and writes the full transcript
(every command, exit code, stdout and stderr, plus OS details) to
`hwtest-<OS>-<arch>.txt`. **Send that file back**, failures included.

Suggested order, so the signed-file check also runs on Unix:

1. Windows (x64 or arm64): `run.ps1` writes `probe-signed.com` into the kit.
2. Copy `probe-signed.com` next to `run.sh` on the Unix machines.
3. Run `run.sh` everywhere else.

## What each host checks

| Host            | `probe.com` should run          | `probe-x86_64.com` should run              |
|-----------------|---------------------------------|--------------------------------------------|
| Linux x86_64    | `Linux-x86_64`                  | `Linux-x86_64`                             |
| Linux arm64     | `Linux-aarch64`                 | `Linux-x86_64` under the emulator, `translated=1` (without `--emu`: exit 126, "no slice") |
| FreeBSD 14 x86_64 | `FreeBSD-x86_64` (proves the `EI_OSABI` = 9 brand) | `FreeBSD-x86_64`              |
| FreeBSD 14 arm64 | `FreeBSD-aarch64` (`-Zbuild-std` slice) | exit 126, "no slice for FreeBSD-aarch64" |
| macOS Intel     | `Darwin-x86_64` from the fat slice | `Darwin-x86_64`                         |
| macOS Apple Silicon | `Darwin-aarch64` from the fat slice | `Darwin-x86_64` under Rosetta, `mac.proc_translated=1` |
| Windows x64     | `Windows-x86_64`                | `Windows-x86_64`                           |
| Windows arm64   | `Windows-aarch64`: the x86_64 launcher's `IsWow64Process2` saw ARM64 | `Windows-x86_64` under Prism, `win.native_machine=0xAA64` |

`run.sh` also checks:

- every way of starting the file: `sh ./probe.com`, `sh probe.com`, direct
  exec from each installed shell (sh, bash, dash, zsh, ksh, mksh, yash, fish)
  and lookup through `$PATH`;
- exit codes up to 255, stdin, and paths and `$TMPDIR` with spaces;
- that the extraction directory is removed afterwards.

`run.ps1` also checks:

- arguments that need quoting (spaces, empty, `"`, trailing `\`, non-ASCII);
- exit codes 1000 and `0xC000013A`, stdin, and paths with spaces and
  non-ASCII;
- that `%TEMP%` is cleaned up;
- Authenticode: it signs a copy with a self-signed certificate (created in
  `Cert:\CurrentUser\My` and deleted again; `-NoSign` skips this), checks
  the signature, and runs the signed file.

## Manual checks

These need a person at the keyboard:

- **Windows, Ctrl-C:** run `.\probe.com --stdin` in a console and press
  Ctrl-C. It should exit promptly, and `%TEMP%` should hold no `unibin.*`
  directory afterwards.
- **Windows, real certificate:** if you have one, run
  `signtool sign /fd SHA256 /f cert.pfx probe-signed.com` and run
  `run.ps1 -NoSign` plus `sh run.sh` (with that file) again.
- **macOS, downloaded file:** download the kit through a browser so it
  carries `com.apple.quarantine`, then run `sh run.sh`. The slice is
  written fresh by `sh`, so Gatekeeper should not get involved.

## Reading failures

- `exit 137` on macOS: the kernel killed an arm64 slice with a missing or
  invalid code signature.
- `ELF binary type "0" not known` on FreeBSD: the slice lost its FreeBSD
  brand (`EI_OSABI`).
- `Bad CPU type in executable` on Apple Silicon: Rosetta is not installed
  (`softwareupdate --install-rosetta`).
- `SKIP direct-exec` on Linux: wine registered a binfmt_misc handler for
  `MZ` files; that is a host setting, not a unibin bug.
- `Smart App Control: ON` in a Windows report: it may block the unsigned
  slice the launcher extracts to `%TEMP%`.
