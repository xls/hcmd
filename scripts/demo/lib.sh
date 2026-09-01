# Shared settings for the demo recordings. Sourced, not run.
#
# Everything the demo touches lives under $DEMO_ROOT, so a recording never
# reads the recorder's own home: the config, the state (last directories,
# history) and the files on screen are all built from scratch and thrown away.

DEMO_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$DEMO_DIR/../.." && pwd)

DEMO_ROOT=${DEMO_ROOT:-/tmp/hcmd-demo}
DEMO_DATA="$DEMO_ROOT/files"
DEMO_CONFIG="$DEMO_ROOT/config"
DEMO_STATE="$DEMO_ROOT/state"
DEMO_SESSION=${DEMO_SESSION:-hcmd-demo}

# The recorded terminal's size. The tapes set Width/Height to match at the
# configured font size; change both together or the panels will not fill it.
DEMO_COLS=${DEMO_COLS:-120}
DEMO_ROWS=${DEMO_ROWS:-34}

# The binary under test. A release build if the tree has one, so a demo is
# never accidentally recorded against a stale ~/.local/bin copy.
if [ -x "$REPO_ROOT/target/release/hcmd" ]; then
    DEMO_BIN=${DEMO_BIN:-$REPO_ROOT/target/release/hcmd}
else
    DEMO_BIN=${DEMO_BIN:-$(command -v hcmd)}
fi

demo_env() {
    env XDG_CONFIG_HOME="$DEMO_CONFIG" \
        XDG_STATE_HOME="$DEMO_STATE" \
        HCMD_KEYBOARD_PROTOCOL="${HCMD_KEYBOARD_PROTOCOL:-auto}" \
        "$@"
}
