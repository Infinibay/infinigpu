#!/usr/bin/env bash
# Regenerate the C view of the wire ABI from infinigpu-abi (cbindgen) and verify
# the generated layout matches via the C conformance static-asserts.
#   needs: cargo install cbindgen
set -euo pipefail
cd "$(dirname "$0")/.."

command -v cbindgen >/dev/null || { echo "!! cbindgen missing: cargo install cbindgen"; exit 1; }

# cbindgen prints benign "Conflicting name for constant" notes for names that exist
# in two modules; we emit structs only, so filter them (it still exits 0).
cbindgen --config crates/infinigpu-abi/cbindgen.toml \
    --crate infinigpu-abi \
    --output guest/include/infinigpu_abi.h \
    2> >(grep -vE 'Conflicting name for constant|^WARN' >&2)

cc -std=c11 -Wall -Werror -I guest/include guest/include/abi_conformance.c -o /tmp/infinigpu-abiconf
/tmp/infinigpu-abiconf && echo "OK: C header layout matches the Rust ABI."
