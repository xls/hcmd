#!/usr/bin/env bash
# Build the tree the demo recordings browse.
#
# Deterministic on purpose: fixed content, fixed timestamps, fixed sizes. Two
# recordings of the same tape differ only where the program differs, which is
# what makes a regenerated GIF worth looking at.
set -euo pipefail
. "$(dirname -- "${BASH_SOURCE[0]}")/lib.sh"

STAMP="2026-03-14 09:26:53"

rm -rf "$DEMO_DATA"
mkdir -p "$DEMO_DATA"/{project,archives,media,logs,backup}
cd "$DEMO_DATA"

# --- a small source tree: syntax highlighting, the git column, Alt+D diffs ---
mkdir -p project/src project/docs
cat > project/src/main.rs <<'EOF'
use std::io::{self, Write};

use crate::engine::{Engine, Frame};

/// Entry point. Everything below runs in this process: no helper binaries,
/// no shelling out, no surprises about which `rg` happens to be on PATH.
fn main() -> io::Result<()> {
    let mut engine = Engine::new(Config::load()?);

    for frame in engine.frames() {
        match frame {
            Frame::Ready(buf) => io::stdout().write_all(&buf)?,
            Frame::Skipped { reason } => eprintln!("skipped: {reason}"),
        }
    }

    engine.flush()
}
EOF
cat > project/src/engine.rs <<'EOF'
//! The decode loop.

use std::collections::VecDeque;
use std::time::Duration;

const QUEUE_DEPTH: usize = 64;
const TIMEOUT: Duration = Duration::from_millis(250);

pub struct Engine {
    queue: VecDeque<Frame>,
    depth: usize,
}

impl Engine {
    pub fn new(config: Config) -> Self {
        Self { queue: VecDeque::with_capacity(QUEUE_DEPTH), depth: config.depth }
    }

    /// Frames come out in submission order even though they decode out of it.
    pub fn frames(&mut self) -> impl Iterator<Item = Frame> + '_ {
        std::iter::from_fn(move || self.queue.pop_front())
    }
}
EOF
cat > project/src/config.rs <<'EOF'
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub depth: usize,
    pub verify: bool,
    pub theme: String,
}

impl Default for Config {
    fn default() -> Self {
        Self { depth: 8, verify: true, theme: "blue".into() }
    }
}
EOF
cat > project/Cargo.toml <<'EOF'
[package]
name = "vellum"
version = "0.4.2"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
EOF
cat > project/docs/README.md <<'EOF'
# Vellum

A streaming decoder that does its own work in process.

## Why

Shelling out to a helper binary means the helper's version, the helper's
error messages and the helper's idea of a path. Vellum has none of those.

## Building

    cargo build --release

| Target        | Status |
| ------------- | ------ |
| x86_64-linux  | tier 1 |
| aarch64-linux | tier 1 |
| x86_64-darwin | tier 2 |
EOF
cat > project/docs/api.json <<'EOF'
{
  "name": "vellum",
  "version": "0.4.2",
  "endpoints": [
    { "path": "/frames", "method": "GET", "streaming": true },
    { "path": "/frames/{id}", "method": "GET", "streaming": false },
    { "path": "/health", "method": "GET", "streaming": false }
  ],
  "limits": { "queue_depth": 64, "timeout_ms": 250 }
}
EOF

# A repository, with one file committed and then changed, so the git column
# has something to say and Alt+D has a diff to show.
(
    cd project
    git init -q -b main
    git -c user.email=demo@example.com -c user.name=Demo add -A
    GIT_AUTHOR_DATE="$STAMP" GIT_COMMITTER_DATE="$STAMP" \
        git -c user.email=demo@example.com -c user.name=Demo \
            commit -qm "Frames leave the queue in the order they entered it"
    # The working copy now differs from HEAD in one file.
    sed -i 's/const QUEUE_DEPTH: usize = 64;/const QUEUE_DEPTH: usize = 256;/' src/engine.rs
    printf '\n[profile.release]\nlto = true\n' >> Cargo.toml
    printf 'target/\n' > .gitignore
)

# --- archives, which the panels step into like directories ---
tar -czf archives/vellum-0.4.2.tar.gz -C "$DEMO_DATA" project
(cd project && zip -qr "$DEMO_DATA/archives/docs.zip" docs)
printf 'placeholder payload, compressed on its own\n' > /tmp/hcmd-demo-single.txt
gzip -c /tmp/hcmd-demo-single.txt > archives/notes.txt.gz
rm -f /tmp/hcmd-demo-single.txt
# An archive nested inside an archive.
tar -czf archives/nested.tar.gz -C "$DEMO_DATA" archives/docs.zip 2>/dev/null || true

# --- a PNG, for Shift+F9 reading the header through a built-in template ---
python3 - "$DEMO_DATA/media/render.png" <<'PY'
import struct, sys, zlib
w, h = 640, 400
rows = b"".join(
    b"\x00" + bytes(v for x in range(w) for v in ((x * 255) // w, (y * 255) // h, 160, 255))
    for y in range(h)
)
def chunk(tag, data):
    c = tag + data
    return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c))
png = (b"\x89PNG\r\n\x1a\n"
       + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
       + chunk(b"IDAT", zlib.compress(rows, 9))
       + chunk(b"IEND", b""))
open(sys.argv[1], "wb").write(png)
PY
cp "$DEMO_BIN" media/hcmd.bin 2>/dev/null || cp /bin/ls media/hcmd.bin

# --- a log worth streaming: the viewer opens it as fast as a small file ---
python3 - "$DEMO_DATA/logs/service.log" <<'PY'
import sys
levels = ["INFO ", "INFO ", "INFO ", "WARN ", "ERROR"]
msgs = ["frame %d queued", "frame %d decoded in %dms", "queue depth %d",
        "peer %d timed out, retrying", "checksum mismatch on frame %d"]
with open(sys.argv[1], "w") as f:
    for i in range(240000):
        lvl = levels[i % 5]
        m = msgs[i % 5] % ((i,) if m_args := msgs[i % 5].count("%d") == 1 else (i, i % 97))
        f.write(f"2026-03-14T09:26:{i % 60:02d}.{i % 1000:03d}Z {lvl} vellum::engine  {m}\n")
PY
gzip -kf logs/service.log && mv logs/service.log.gz logs/service-2026-03-13.log.gz

# --- a destination panel with something already in it, so F5 has a conflict
#     to talk about and the panel is not an empty box on screen ---
cp project/docs/README.md backup/README.md
printf 'checked 2026-03-14, all frames accounted for\n' > backup/audit.txt

find "$DEMO_DATA" -depth \( -type f -o -type d \) ! -path "*/.git/*" \
    -exec touch -d "$STAMP" {} +

printf 'demo tree ready: %s\n' "$DEMO_DATA"
du -sh "$DEMO_DATA"
