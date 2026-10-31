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

# Slice targets (aarch64-unknown-freebsd has no prebuilt std; see README).
RUN rustup component add clippy rustfmt llvm-tools \
 && rustup target add \
      x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
      x86_64-apple-darwin aarch64-apple-darwin \
      x86_64-unknown-freebsd \
      x86_64-pc-windows-msvc aarch64-pc-windows-msvc

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
