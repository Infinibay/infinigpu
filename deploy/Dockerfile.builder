# infinigpu container-native builder.
#
# Produces the two artifacts the infinigpu render path needs — a QEMU with the
# upstream vfio-user-pci client, and the infinigpu-device server — built against
# the SAME base as the backend container (node:20-bookworm → Debian 12, glibc
# 2.36). A host build is the wrong ABI: the host QEMU needs glibc ≥2.38 and the
# host-cargo device binary needs ≥2.39, so neither runs inside the backend
# container. This builder is the fix for that. Mirrors docker/infiniservice.Dockerfile.
#
# Build context = the infinigpu repo root; `run` builds the device binary from the
# mounted /src and publishes both artifacts (plus QEMU's non-glibc shared-lib
# closure, rpath-patched) into the shared /out volume the backend mounts read-only.
FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive \
    QEMU_VER=10.1.5 \
    QEMU_PREFIX=/opt/qemu-vfio-user \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

# QEMU build deps (same set as scripts/build-qemu-vfio-user.sh) + Rust build deps +
# patchelf (to make QEMU find its bundled libs relative to itself, no LD_LIBRARY_PATH).
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates wget xz-utils git curl patchelf \
      build-essential ninja-build meson pkg-config python3 python3-venv flex bison \
      libglib2.0-dev libpixman-1-dev zlib1g-dev libslirp-dev \
 && rm -rf /var/lib/apt/lists/*

# Build QEMU with the vfio-user-pci client (landed upstream in QEMU 10.1) into a
# private prefix — baked into the IMAGE so a `run` is just the fast device build +
# copy-out. configure bootstraps its own meson into a venv, so bookworm's system
# meson version is irrelevant.
RUN set -eux; \
    cd /tmp; \
    wget -q "https://download.qemu.org/qemu-${QEMU_VER}.tar.xz"; \
    tar xf "qemu-${QEMU_VER}.tar.xz"; \
    cd "qemu-${QEMU_VER}"; \
    ./configure --prefix="${QEMU_PREFIX}" --target-list=x86_64-softmmu --enable-slirp; \
    make -j"$(nproc)"; \
    make install; \
    "${QEMU_PREFIX}/bin/qemu-system-x86_64" -device vfio-user-pci,help >/dev/null; \
    cd /; rm -rf /tmp/qemu-*

# Rust toolchain for infinigpu-device (built at run time from the mounted source).
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --default-toolchain stable

COPY deploy/build-into.sh /usr/local/bin/build-into.sh
RUN chmod +x /usr/local/bin/build-into.sh
ENTRYPOINT ["/usr/local/bin/build-into.sh"]
