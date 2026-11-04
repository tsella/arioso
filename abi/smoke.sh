#!/bin/sh
# Build the C ABI, compile abi/smoke.c against the static library, and run it.
#
# Two things are checked here that the Rust tests cannot: that the generated header compiles
# as C, and that the archive carries the symbols with the signatures the header declares. The
# program is linked against libarioso.a, not the shared library, because the store build is a
# static link into an app bundle and that is the shape that has to work.
#
# CI runs this on Linux and macOS. Windows is not covered: `cc` is not guaranteed on the
# runner and the smoke program uses mkdtemp, which the Windows C runtime does not have. The
# crate itself still builds on Windows with the rest of the workspace.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
: "${CARGO_TARGET_DIR:=$root/target}"
archive="$CARGO_TARGET_DIR/c-abi/libarioso.a"
onnx="$root/vendor/onnxruntime/lib"
binary="$CARGO_TARGET_DIR/c-abi/arioso-smoke"

cargo build -p arioso-abi --profile c-abi --manifest-path "$root/Cargo.toml"

# The C++ runtime is for the sherpa-onnx engine, which is C++ compiled to a static library
# and carried inside the archive. The frameworks are what rustls and getrandom reach for on
# macOS; a Linux build has no equivalent.
case "$(uname -s)" in
    Linux) system="-lstdc++ -lpthread -ldl -lm" ;;
    Darwin) system="-lc++ -framework Security -framework CoreFoundation" ;;
    *)
        echo "abi/smoke.sh: no link line for $(uname -s)" >&2
        exit 1
        ;;
esac

# ONNX Runtime stays a shared library: it is a native dependency of the speech engine, not
# part of the Rust archive. The rpath is absolute because the binary lives in the cargo
# target directory, which is not beside the vendor tree.
cc -std=c11 -Wall -Wextra -I "$root/include" -o "$binary" "$root/abi/smoke.c" \
    "$archive" -L "$onnx" -lonnxruntime -Wl,-rpath,"$onnx" $system

LD_LIBRARY_PATH="$onnx${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
DYLD_LIBRARY_PATH="$onnx${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}" \
    "$binary"
