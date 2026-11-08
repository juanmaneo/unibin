# Build/test environment for the unibin workspace.
#
# Works with Docker, Podman and Buildah (plain Dockerfile syntax only: no
# BuildKit-specific features, fully-qualified image names):
#
#   docker build -t unibin-dev .            podman build -t unibin-dev .
#   buildah bud -t unibin-dev .
#
#   podman run --rm -v "$PWD":/src:Z -v unibin-target:/src/target unibin-dev
#   podman run --rm -v "$PWD":/src:Z -v unibin-target:/src/target unibin-dev \
#       cargo xtask dist -o /src/unibin-pack.com     # release polyglot
#   ... cargo xtask dist --emu "$UNIBIN_EMU" -o ...  # + x86_64 emulator for arm64 Linux
#
# The default command is `cargo xtask dev-check`. The named `unibin-target` volume
# keeps the container's build artifacts apart from the host's `target/`.
# The `:Z` suffix relabels the bind mount for SELinux hosts; drop it if your
# engine rejects it.
#
# Note: binfmt_misc is kernel-wide, so a host that hands MZ files to wine
# does so inside the container too (see README).

FROM docker.io/library/rust:1-bookworm

ARG ZIG_VERSION=0.16.0
ARG ZIG_SHA256_X86_64=70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00
ARG ZIG_SHA256_AARCH64=ea4b09bfb22ec6f6c6ceac57ab63efb6b46e17ab08d21f69f3a48b38e1534f17

# cc + static glibc (integration test), zsh (shell matrix), unzip (ZIP interop),
# clang/lld/llvm (cargo-xwin), xz (zig tarball).
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates clang lld llvm unzip xz-utils zsh \
 && rm -rf /var/lib/apt/lists/*

RUN set -eu; \
    arch="$(uname -m)"; \
    case "$arch" in \
      x86_64) sum="$ZIG_SHA256_X86_64" ;; \
      aarch64) sum="$ZIG_SHA256_AARCH64" ;; \
      *) echo "unsupported build host $arch" >&2; exit 1 ;; \
    esac; \
    tarball="zig-$arch-linux-$ZIG_VERSION.tar.xz"; \
    curl -fsSLo "/tmp/$tarball" "https://ziglang.org/download/$ZIG_VERSION/$tarball"; \
    echo "$sum  /tmp/$tarball" | sha256sum -c -; \
    mkdir -p /opt/zig; \
    tar -xJf "/tmp/$tarball" -C /opt/zig --strip-components=1; \
    rm "/tmp/$tarball"; \
    ln -s /opt/zig/zig /usr/local/bin/zig; \
    zig version

# Slice targets. aarch64-unknown-freebsd has no prebuilt std: `dist` builds
# it with -Zbuild-std on this pinned nightly (keep in sync with
# BUILD_STD_TOOLCHAIN in xtask/src/main.rs).
ARG BUILD_STD_TOOLCHAIN=nightly-2026-11-08
RUN rustup component add clippy rustfmt llvm-tools \
 && rustup target add \
      x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
      x86_64-apple-darwin aarch64-apple-darwin \
      x86_64-unknown-freebsd \
      x86_64-pc-windows-msvc aarch64-pc-windows-msvc \
 && rustup toolchain install "$BUILD_STD_TOOLCHAIN" --profile minimal --component rust-src

# x86_64 emulator for Linux/arm64 hosts (`--emu`): the static-pie
# qemu-x86_64 from Debian's qemu-user arm64 package (GPL-2.0), pinned by
# snapshot.debian.org hash. FEX is faster, but it has no static build and
# needs a FEXServer next to it, so it cannot be a single --emu file.
ARG QEMU_DEB=qemu-user_10.0.13+ds-0+deb13u1_arm64.deb
ARG QEMU_DEB_SHA1=02b553a4532192bc999d15408d1e56d7b254906f
ARG QEMU_DEB_SHA256=c73711af02b97cd5e2667e735d9c06890c57165517110ca4e646c95a7083158d
ARG QEMU_X86_64_SHA256=9af86352fd463ad9ec25bdd6a4b891ac6b6a7f3d46a7db3da6f23e2c09094a59
RUN set -eu; \
    curl -fsSLo /tmp/qemu.deb "https://snapshot.debian.org/file/$QEMU_DEB_SHA1/$QEMU_DEB"; \
    echo "$QEMU_DEB_SHA256  /tmp/qemu.deb" | sha256sum -c -; \
    mkdir -p /opt/emu; \
    dpkg-deb --fsys-tarfile /tmp/qemu.deb | tar -xO ./usr/bin/qemu-x86_64 > /opt/emu/qemu-x86_64; \
    echo "$QEMU_X86_64_SHA256  /opt/emu/qemu-x86_64" | sha256sum -c -; \
    chmod 755 /opt/emu/qemu-x86_64; \
    rm /tmp/qemu.deb
ENV UNIBIN_EMU=/opt/emu/qemu-x86_64

RUN cargo install --locked cargo-zigbuild cargo-xwin \
 && rm -rf "$CARGO_HOME/registry" "$CARGO_HOME/git"

# cargo-xwin downloads the MSVC CRT/SDK on first use; this accepts its license.
# Both download caches live in the (volume-backed) target dir so `--rm`
# containers don't refetch them.
ENV XWIN_ACCEPT_LICENSE=1 \
    XWIN_CACHE_DIR=/src/target/xwin-cache \
    ZIG_GLOBAL_CACHE_DIR=/src/target/zig-cache

WORKDIR /src
CMD ["cargo", "xtask", "dev-check"]
