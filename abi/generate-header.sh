#!/bin/sh
# Regenerate include/arioso.h from crates/arioso-abi.
#
# The header is generated, so it is checked in: a C caller needs it, and a review of the
# surface should not require installing a tool. CI runs this and fails when the committed
# file differs, which is what keeps the two from drifting.
#
# cbindgen is pinned in CI. The version matters: a different cbindgen can reformat the same
# declarations, and the diff check would fail for a reason that has nothing to do with the
# ABI.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

cbindgen --config cbindgen.toml --crate arioso-abi --output include/arioso.h
