#!/bin/sh
# Run arioso-speaks, the terminal front end.
#
#   ./examples/speaks.sh                      the default config
#   ./examples/speaks.sh /path/to/config.json a config of your own
#
# The window opens on the voice the config names. Keys:
#
#   tab        move between the voice list, the text, the phonemes and the buttons
#   left/right change engine tab (in the list) or button (in the buttons)
#   up/down    move through the voices
#   typing     filters the list
#   +/-        next and previous speaker of a multi-speaker voice
#   ctrl-enter say it, or download the voice when it is not on disk
#   ctrl-s     save the last audio, as WAV or FLAC
#   ctrl-r     throw away an edited phoneme line and generate it again
#   ctrl-d     download the highlighted voice
#   ctrl-q     quit
#
# The log is beside the binary: arioso-speaks.log. A download that fails writes the reason
# there, and the path is on screen under the wordmark. ARIOSO_LOG=debug keeps more of it.
set -eu

config=${1:-}

# Linux needs ALSA's headers to build, because playback is in process. The other two
# platforms use their own system frameworks.
if [ "$(uname -s)" = "Linux" ] && [ -f Cargo.toml ]; then
    if ! pkg-config --exists alsa 2>/dev/null; then
        echo "building needs ALSA headers: sudo apt-get install libasound2-dev" >&2
        echo "(trying anyway: they may be installed where pkg-config cannot see them)" >&2
    fi
fi

# A source tree builds and runs the workspace binary; anything else uses the installed one.
if [ -f Cargo.toml ] && command -v cargo >/dev/null 2>&1; then
    if [ -n "$config" ]; then
        exec cargo run --quiet -p arioso-speaks -- --config "$config"
    fi
    exec cargo run --quiet -p arioso-speaks
fi

if ! command -v arioso-speaks >/dev/null 2>&1; then
    echo "arioso-speaks is not on PATH and this is not a source tree" >&2
    echo "run it from the repository, or install it with: cargo install --path crates/arioso-speaks" >&2
    exit 1
fi

if [ -n "$config" ]; then
    exec arioso-speaks --config "$config"
fi
exec arioso-speaks
