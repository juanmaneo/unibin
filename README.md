[![CI](https://github.com/juanmaneo/unibin/actions/workflows/ci.yml/badge.svg)](https://github.com/juanmaneo/unibin/actions/workflows/ci.yml) [![License: ISC](https://img.shields.io/badge/license-ISC-blue.svg)](LICENSE)

# Unibin - One executable file for Linux, FreeBSD, macOS and Windows

`unibin-pack` combines native executables ("slices") into **one file**. That
file runs on Linux, FreeBSD, macOS and Windows 11+, on x86_64 and arm64,
with nothing to install: no binfmt_misc registration, no admin rights and no
runtime.

It borrows the *container* trick from Cosmopolitan's APE (a file that is a
shell script, a PE image and a ZIP archive at once). It is **not**
APE-compatible and does not use a portable libc. Each slice is an ordinary
binary built by the normal Rust toolchain for its own target.

| Host                | What runs                                                     |
|---------------------|---------------------------------------------------------------|
| Linux x86_64/arm64  | `Linux-<arch>` static musl slice, via the `sh` prologue       |
| Linux arm64, no slice | `Linux-x86_64` slice under the embedded FEX emulator (`--emu`) |
| FreeBSD x86_64      | `FreeBSD-x86_64` slice, via the `sh` prologue                 |
| macOS x86_64/arm64  | fat Mach-O slice; arm64 falls back to x86_64 under Rosetta    |
| Windows x64/arm64   | PE launcher stub → `Windows-x86_64` slice (Prism on arm64)    |

## Workspace

| Crate         | Role |
|---------------|------|
| `unibin-core`   | Format library with **zero external dependencies**: stored ZIP writer/reader, manifest, `sh` prologue, PE relocation, slice sniffing, packing, extraction plans |
| `unibin-pack`   | CLI that writes the polyglot |
| `unibin-launch` | The Windows launcher stub embedded as the PE image (pure Rust, std only) |
| `xtask`       | `dev-check` pre-push gate (check, fmt, clippy, build, test, coverage with a 100% line gate) and `dist` |

## File layout

```
0x000  MZqFpD='unibin' #<spaces>   sh line 1; "MZ" is the PE e_magic
0x038  \n                          first newline: below 0x3C, no NUL before it
0x039  #  <e_lfanew LE32>\n        sh comment hiding e_lfanew at 0x3C
0x041  sh script … exit 1\n        picks, extracts and execs a slice
       \n padding to FileAlignment
P      PE launcher image           relocated copy of unibin-launch.exe
S…     slice bytes                 stored (uncompressed) ZIP entries
       ZIP central directory
       EOCD + comment              the comment is the text manifest
```

Why each piece works:

- **Shells.** `execve` returns `ENOEXEC` for a file with no shebang and no
  known magic. bash, dash, FreeBSD `sh`, zsh ≥ 5.9 and fish ≥ 3.3 then run it
  as a script, provided the first line contains no NUL (POSIX.1-2024). So the
  DOS header is padded with **spaces**, and the first `\n` sits at 0x38.
- **Windows** reads only `MZ` at offset 0 and `e_lfanew` at 0x3C. Everything
  else in the DOS header and stub is ignored, so it can be shell text. The
  launcher is *relocated*, not just appended: section file offsets, the
  symbol table, the debug directory and `SizeOfHeaders` are shifted by the
  prologue length, rounded up to `FileAlignment`, and `CheckSum` is zeroed.
- **ZIP** readers scan backwards from EOF for `PK\5\6`. ELF, Mach-O and PE
  loaders never look there. Central-directory offsets are absolute, so
  `unzip -l app` works, and so does the shell, which addresses slices by
  absolute offset.

The EOCD comment holds the manifest:

```
unibin=1
emu=Linux-aarch64
emu_payload=2101248,9437184
Linux-x86_64=1536,354384
Darwin-x86_64=917855,662656
Darwin-aarch64=917855,662656
…
```

Each value is `offset,length` into the file. Keys are `$(uname -s)-$(uname -m)`
after normalising `amd64`/`x64` → `x86_64` and `arm64` → `aarch64`. Several
keys can share one span; a fat Mach-O serves both Darwin keys.

### The shell prologue

The prologue is plain POSIX `sh` and uses only `tail`, `head`, `od`, `sed`,
`mktemp`, `uname`, `chmod` and `kill`. It does this:

1. Resolves `$0`, using `command -v` when the program was found through `$PATH`.
2. Checks for the `PK\5\6` signature at the EOCD offset. That offset is
   **baked into the script** at pack time (`e=…`) instead of being read from
   the last 22 bytes, so the file still works after Authenticode signing
   appends data.
3. Reads the manifest. Binary data is only ever *piped*. Command substitution
   would silently drop NUL bytes.
4. Extracts the chosen slice into `mktemp -d "${TMPDIR:-/tmp}/unibin.XXXXXX"`
   and `exec`s it. A small background watcher deletes the directory once the
   shell's PID (now the slice's) exits. The running file is never rewritten
   in place (that would hit `ETXTBSY`).

Exit status is 126 if the file is damaged or there is no slice for the host,
and 127 if `$0` cannot be found.

## Usage

```sh
unibin-pack --launcher unibin-launch.exe \
          --slice app-linux-x86_64 \
          --slice app-linux-aarch64 \
          --slice app-macos-universal \
          --slice app-freebsd-x86_64 \
          --slice app.exe \
          [--emu FEXInterpreter-static] \
          -o app
```

The key of each slice is detected from its content: ELF `EI_OSABI` and
FreeBSD ABI note, Mach-O magic and CPU type, PE machine. An explicit
`--slice file=Darwin-aarch64` must agree with the content. Use it to
register only part of a fat binary. `unibin-pack` rejects duplicate keys, 32-bit
or foreign-arch binaries, and an `--emu` that is not a static Linux-aarch64
ELF or has no `Linux-x86_64` slice to run. The output is written with mode
0755 and is byte-for-byte reproducible.

`unibin-launch` must be the **unsigned x86_64** build. On Windows it opens its
own image and reads the manifest. It then extracts `Windows-x86_64` to
`%TEMP%`, runs it with the same arguments, waits, and returns its exit code.
Ctrl-C is left to the child. The temp directory is removed afterwards.

## Building slices (from one Linux host)

The [`Dockerfile`](Dockerfile) sets up everything below: zig, cargo-zigbuild,
cargo-xwin and the rustup targets. It works with Docker, Podman and Buildah:

```sh
podman build -t unibin-dev .            # or: docker build / buildah bud
podman run --rm -v "$PWD":/src:Z -v unibin-target:/src/target unibin-dev   # cargo xtask dev-check
podman run --rm -v "$PWD":/src:Z -v unibin-target:/src/target unibin-dev \
    cargo xtask dist -o /src/unibin-pack.com
```

`cargo xtask dist [-p <package>] [--emu <FEX-static>] [-o <out>]` does the
whole release: it builds the `unibin-launch` stub and the package's
same-named binary for every target below, then runs `unibin-pack` on the
results. The default package is `unibin-pack` and the default output is
`target/dist/<package>.com`. Relative paths are resolved from where you run
it. The zig and MSVC SDK download caches live under `target/`, so the
`unibin-target` volume keeps them between `--rm` runs.

The equivalent manual steps (all run in that image):

```sh
# Linux: static musl, no glibc version floor
cargo zigbuild --release --target x86_64-unknown-linux-musl
cargo zigbuild --release --target aarch64-unknown-linux-musl
# macOS: one fat Mach-O (cargo-zigbuild merges both arches)
cargo zigbuild --release --target universal2-apple-darwin
# FreeBSD: zig's libc floor is FreeBSD 14
cargo zigbuild --release --target x86_64-unknown-freebsd
# Windows: MSVC ABI, CRT/SDK fetched by cargo-xwin
cargo xwin build --release --target x86_64-pc-windows-msvc
cargo xwin build --release -p unibin-launch --target x86_64-pc-windows-msvc
```

Notes:

- zig/lld brand FreeBSD executables only with a `FreeBSD` ABI-tag note and
  leave `EI_OSABI` at 0. `unibin-pack` detects the note and sets byte 7 to
  `ELFOSABI_FREEBSD` (9), as `brandelf -t FreeBSD` would. FreeBSD is the only
  UNIX that checks it.
- Without an SDK, zigbuild warns that `xcrun` is missing. Pure-Rust binaries
  link fine anyway. Set `SDKROOT` if you link Apple frameworks.
- To merge separately built thin Mach-Os yourself, use the pure-Rust
  [`fat-macho`](https://crates.io/crates/fat-macho) crate (cargo-zigbuild uses
  it internally).
- rustc's macOS floors are **10.12 on x86_64 and 11.0 on arm64**.
- `aarch64-unknown-freebsd` has no prebuilt std, so there is no
  `FreeBSD-aarch64` slice without `-Zbuild-std`.
- **Emulator:** pass a static FEX build (`FEXInterpreter`, MIT) as `--emu`.
  FEX needs a 4 KiB-page kernel; on 16 KiB-page Asahi Linux run it inside
  [muvm](https://github.com/AsahiLinux/muvm). macOS arm64 uses Rosetta, and
  Windows on Arm uses Prism, so neither needs `--emu`.

## Signing: always *after* packing

Appending the ZIP would invalidate any signature, so sign last:

- **Windows:** `signtool sign … app` on the packed file works. The certificate
  table is appended after the ZIP. Because the packed length is a multiple of
  8, signtool adds no padding before it. The shell still finds the archive
  through the baked `e=` offset, and `unibin-core` ignores a trailing
  certificate table (`pe::logical_end`).
- **Slices:** sign macOS (`codesign`) and Windows slices **before** packing.
  They are stored byte-for-byte and extracted unchanged, so their signatures
  survive. `codesign` cannot sign the polyglot itself, because it is not a
  Mach-O.
- Any other change to the file after packing makes the baked `e=` offset
  stale. Re-pack instead.

## Caveats

- **binfmt_misc and wine.** If wine registered a binfmt_misc handler for `MZ`
  files, `./app` on Linux starts wine instead of the shell fallback. Use
  `sh ./app`, or disable the handler. The registration is kernel-wide, so it
  applies inside containers too. APE has the same problem.
- **noexec `/tmp`.** Slices are extracted under `$TMPDIR` (default `/tmp`).
  If that is mounted `noexec`, point `TMPDIR` somewhere executable.
- The temp directory is removed by a watcher loop that polls once per
  second, so it lingers briefly after the program exits. `SIGKILL` of the
  whole process group also leaves it behind.

## Development

```sh
cargo xtask dev-check   # pre-push gate: check, fmt, clippy, build, test, coverage
cargo xtask coverage    # just the coverage gate
cargo xtask dist        # cross-build and pack a release polyglot (needs zigbuild/xwin)
```

Each `dev-check` step is also a task of its own (`cargo xtask check`, `fmt`,
`clippy`, `build`, `test`). The steps run in that order and stop at the
first failure. `fmt` only checks formatting, and clippy runs with
`-D warnings`. `all` and `cov` remain as aliases for `dev-check` and
`coverage`.

GitHub Actions ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs
`cargo xtask dev-check` on `ubuntu-latest` for every push and pull request.

`coverage` builds with `-C instrument-coverage` into `target/cov`, merges the
profiles with rustup's `llvm-tools` (`rustup component add llvm-tools`), and
fails unless `llvm-cov report` shows **100% line coverage** for every
`unibin-*/src` file. A successful run ends with:

```
xtask: coverage gate ok (100% lines on unibin-* sources)
```

When the gate fails it prints the uncovered lines. Watch for these
`llvm-cov` quirks:

- A closure that is never called (`.map_err(|_| …)` on a path that never
  fails) counts as an uncovered line, even when the surrounding line runs.
- A `?` that rustfmt moves onto its own line is a separate region. Compute
  the value into a local first.
- `if let` without `else` can leave an uncovered closing brace. Move the body
  into a helper function.

The end-to-end tests:

- pack a real `cc -static` binary and run it through `sh`, `bash` and `zsh`,
  directly and via `$PATH`;
- check temp-dir cleanup and `unzip` interoperability;
- use a fake `uname` to drive every host branch of the prologue (FreeBSD,
  Darwin with the Rosetta fallback, Linux aarch64 with the emulator, no
  slice);
- run the real `unibin-launch` binary against an appended archive.

## Known limitations

- The launcher always picks `Windows-x86_64`. Choosing a native
  `Windows-aarch64` slice under Prism would need `IsWow64Process2`.
- No `FreeBSD-aarch64` slice; see above.
- FEX is C++ and stays behind an exec boundary. It would be replaced only by
  a pure-Rust x86→arm64 JIT that matches it on a differential benchmark
  corpus.
- `rusty_alloc` is not wired in; `unibin-core` deliberately has no
  dependencies.

## License

[ISC](LICENSE), the license Cosmopolitan libc uses. No Cosmopolitan code is
included; unibin reimplements the ideas in Rust from scratch. A FEX binary
passed with `--emu` stays under its own license (MIT) and is not part of
this repository.

## Acknowledgements

This project would not exist without
[Cosmopolitan libc](https://github.com/jart/cosmopolitan) and its
[Actually Portable Executable](https://justine.lol/ape.html) format, created
by Justine Tunney and developed by the Cosmopolitan contributors.

APE showed that one file can be a shell script, a Windows PE image and a
ZIP archive at the same time, and that this works on real systems. unibin's
core ideas come from that work:

- the `MZqFpD='…'` header that is both a DOS `MZ` header and a line of
  shell;
- relying on the shell's `ENOEXEC` fallback instead of a binfmt_misc
  registration;
- hiding executables in a ZIP read from the end of the file.

The POSIX.1-2024 rule that lets shells run such a file at all came out of
the Cosmopolitan project's work with the POSIX standards group.

unibin takes only the container idea. Each slice is an ordinary binary built
by the normal Rust toolchain for its own OS, where Cosmopolitan builds one
binary for all of them. Everything here stands on Cosmopolitan's
groundwork. Thank you.
