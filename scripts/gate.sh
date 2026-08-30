#!/usr/bin/env bash
# Gate oracles for GATES.md. Each prints a success-only token and exits 0 only
# when every assertion in it passes. Written so a gate cannot pass by accident:
# the token is echoed after the last check, never before one.
set -uo pipefail
cd "$(dirname "$0")/.."

case "${1:-}" in

macos-check)
  # Type-check the macOS build without a Mac.
  #
  # Development happens on Linux, where `cargo test` never compiles the macOS
  # side of a `#[cfg]`. Every macOS-only compile error therefore arrives as a
  # red CI run minutes after a push, which has now happened three times for
  # the same cause: a use of `trash::Error::FileSystem`, a variant that exists
  # only on the freedesktop backend.
  #
  # `cargo check` for an Apple target fails on Linux because four dependencies
  # compile C and `cc` cannot target darwin. Zig ships the macOS libc headers,
  # so it can, and `cargo-zigbuild` puts it where cc-rs looks. No Apple SDK and
  # no Mac are involved. `--all-targets` matters: the errors have all been in
  # test code, which a plain `check` does not compile.
  if command -v mise >/dev/null 2>&1 && ! command -v zig >/dev/null 2>&1; then
    z=$(mise which zig 2>/dev/null) && PATH="$(dirname "$z"):$PATH" && export PATH
  fi
  [ -d "$HOME/.cargo/bin" ] && PATH="$HOME/.cargo/bin:$PATH" && export PATH
  missing=
  command -v zig >/dev/null 2>&1 || missing="$missing zig"
  command -v cargo-zigbuild >/dev/null 2>&1 || missing="$missing cargo-zigbuild"
  if [ -n "$missing" ]; then
    echo "cannot check the macOS build; missing:$missing"
    echo "  mise use -g zig@0.15.1"
    echo "  cargo install cargo-zigbuild --locked"
    echo "  rustup target add aarch64-apple-darwin x86_64-apple-darwin"
    exit 1
  fi
  for t in aarch64-apple-darwin x86_64-apple-darwin; do
    rustup target list --installed 2>/dev/null | grep -qx "$t" || {
      echo "no std for $t; rustup target add $t"; exit 1
    }
    cargo-zigbuild check --target "$t" --all-targets --quiet || {
      echo "the macOS build does not compile: $t"; exit 1
    }
  done
  echo "MACOS-CHECK-OK both Apple targets type-check"
  ;;

gate-green)
  cargo fmt --check >/dev/null 2>&1 || { echo "fmt dirty"; exit 1; }
  n=$(cargo clippy --all-targets -- -D warnings 2>&1 | grep -cE '^error' || true)
  [ "$n" -eq 0 ] || { echo "clippy: $n errors"; exit 1; }
  # Linux compiles only one side of every #[cfg]; this compiles the other.
  "$0" macos-check >/dev/null || { "$0" macos-check; exit 1; }
  # ONE run, parsed once. Running the suite twice and comparing the answers
  # let a flaky test make the gate disagree with itself, which is a defect in
  # the oracle rather than in the tree.
  out=$(mktemp); cargo test >"$out" 2>&1; rc=$?
  fail=$(grep -cE '^test result: FAILED' "$out" || true)
  ok=$(grep -cE '^test result: ok' "$out" || true)
  if [ "$rc" -ne 0 ] || [ "$fail" -ne 0 ]; then
    grep -E '^---- |^test result: FAILED' "$out" | head -10; rm -f "$out"; exit 1
  fi
  [ "$ok" -ge 15 ] || { echo "only $ok suites ran"; rm -f "$out"; exit 1; }
  rm -f "$out"
  echo "GATE-GREEN-OK suites=$ok"
  ;;

no-dashes)
  # Every text file the repository carries, not a hand-kept list of four
  # directories. The list was `src tests examples docs` plus two filenames that
  # no longer exist, so README.md, FEATURES.md, AGENTS.md, themes/, templates/,
  # install.sh and the workflows - the files a stranger reads first - were
  # never looked at.
  files=$(
    { git ls-files; git status --short | grep '^??' | cut -c4-; } 2>/dev/null \
      | grep -vE '^(target|dist)/' \
      | grep -vE '\.(png|jpg|gif|ico|zip|gz|zst|xz|bz2|pdf|woff2?)$' \
      | grep -v '^Cargo.lock$'
  )
  [ -n "$files" ] || { echo "no files to check"; exit 1; }

  # Em dash and en dash.
  hit=$(echo "$files" | xargs -r grep -lP '[\x{2014}\x{2013}]' 2>/dev/null | head -5)
  [ -z "$hit" ] || { echo "em or en dash in:"; echo "$hit"; exit 1; }

  # Emoji. The pictographic blocks, plus the dingbats, less two glyphs that are
  # literal shell output in test fixtures rather than decoration: U+276F in a
  # command-line fixture and U+2717 in a sample prompt.
  hit=$(echo "$files" | xargs -r grep -loP '[\x{1F000}-\x{1FAFF}]' 2>/dev/null | head -5)
  [ -z "$hit" ] || { echo "emoji in:"; echo "$hit"; exit 1; }
  n=$(echo "$files" | xargs -r grep -hoP '[\x{2600}-\x{27BF}]' 2>/dev/null \
        | grep -vc '[\xe2\x9d\xaf\xe2\x9c\x97]' || true)
  n=$(echo "$files" | xargs -r grep -hoP '[\x{2600}-\x{27BF}]' 2>/dev/null \
        | grep -v '❯' | grep -vc '✗' || true)
  [ "$n" -eq 0 ] || { echo "$n dingbat glyphs beyond the two allowed"; exit 1; }

  # The house style of a machine that is trying to sound impressive. None of
  # these have ever been the clearest word for anything in this codebase, and
  # their presence is the tell that something was written rather than thought.
  phrases='delve|leverage|seamless|cutting-edge|game.chang|elevate your|unlock the|empower|vibrant|tapestry|testament to|navigate the complex|deep dive|embark|realm of|pivotal|meticulous|showcase|in conclusion|furthermore|moreover|it is worth noting|best-in-class|state-of-the-art|revolutionary|effortless|blazing|supercharge|unleash|holistic|synerg|paradigm|at the end of the day'
  # This script is excluded from its own sweep: it has to name the words in
  # order to look for them, and an oracle that fails on its own definition is
  # an oracle nobody can satisfy.
  hit=$(echo "$files" | grep -v '^scripts/gate.sh$' \
        | xargs -r grep -inE "$phrases" 2>/dev/null | head -5)
  [ -z "$hit" ] || { echo "sales copy:"; echo "$hit"; exit 1; }

  echo "NO-DASHES-OK no dashes, no emoji, no sales copy, $(echo "$files" | wc -l) files"
  ;;

no-spec-in-ui)
  # The shipped binary, not the source. The old check parsed Rust for
  # single-line string literals and missed two things that reached the user:
  # a multi-line help string that told them SPEC.md was the source of truth,
  # and the default config files, which are `include_str!`d from examples/ and
  # written into the user's own config directory. What ships is what matters,
  # so this reads the binary's string data and cannot be fooled by formatting.
  cargo build --release >/dev/null 2>&1 || { echo "release build failed"; exit 1; }
  n=$(strings target/release/hcmd | grep -cE 'SPEC\.md|SPEC\.txt' || true)
  [ "$n" -eq 0 ] || { strings target/release/hcmd | grep -E 'SPEC\.md|SPEC\.txt' | head -5; exit 1; }
  # And the templates on disk, which is where they are read from and edited.
  m=$(grep -rlE 'SPEC\.md' examples/ 2>/dev/null | wc -l)
  [ "$m" -eq 0 ] || { grep -rnE 'SPEC\.md' examples/ | head -5; exit 1; }
  echo "NO-SPEC-IN-UI-OK nothing the user reads names the spec"
  ;;

no-cell)
  # Declarations only: a comment explaining why a Mutex was chosen over a
  # RefCell is compliance being documented, not a violation.
  n=$(grep -rn '[^a-zA-Z_]\(Ref\)\?Cell<' src --include=*.rs | grep -vc ':\s*//' || true)
  [ "$n" -eq 0 ] || { grep -rn '[^a-zA-Z_]\(Ref\)\?Cell<' src --include=*.rs | grep -v ':\s*//'; exit 1; }
  grep -q '^disallowed_types = "deny"' Cargo.toml || { echo "disallowed_types is not deny"; exit 1; }
  echo "NO-CELL-OK"
  ;;

binary-size)
  cargo build --release >/dev/null 2>&1 || { echo "release build failed"; exit 1; }
  # The ceiling catches bloat arriving by accident. It is raised deliberately,
  # in a commit that says what bought the space, and never nudged up to make a
  # failing run pass.
  #
  # 24 MB held until the binary carried 109 templates, the content renderers
  # and image codecs for the resizer. Raised to 28 MB for those, which the
  # owner agreed to: a file manager that decodes eight image formats and
  # renders JSON, HTML and markdown is a bigger program than one that does
  # not, and the alternative was leaving the features out.
  b=$(stat -c %s target/release/hcmd)
  [ "$b" -lt 28000000 ] || { echo "binary is $b bytes, expected under 28000000"; exit 1; }
  echo "BINARY-SIZE-OK bytes=$b"
  ;;

no-liblzma)
  cargo build --release >/dev/null 2>&1 || { echo "release build failed"; exit 1; }
  if ldd target/release/hcmd 2>/dev/null | grep -qi lzma; then
    ldd target/release/hcmd | grep -i lzma; exit 1
  fi
  echo "NO-LIBLZMA-OK"
  ;;

decomposed)
  # A line count is dogma; size is the thing that actually hurts. A file of
  # 800 lines with one clear responsibility is fine to read and fine to hand
  # to an agent. 363 KB is not: it does not fit in a reader's head, and it
  # burns an enormous amount of any agent's context to touch at all.
  #
  # 100 KB is roughly 2000 lines of this codebase's style, which is large but
  # still navigable. Six files exceed it today and app.rs is more than three
  # times over on its own.
  big=$(python3 scripts/file_sizes.py over100k)
  [ "$big" -eq 0 ] || { python3 scripts/file_sizes.py list100k; exit 1; }
  echo "DECOMPOSED-OK no production file over 100 KB"
  ;;

# A filtered-out test set prints "test result: ok. 0 passed", which matches a
# naive expectation and certifies an empty set. Both feature gates below
# therefore require a MINIMUM number of tests to have actually run, so the
# gate fails while the feature does not exist rather than passing vacuously.
remote-search)
  out=$(HCMD_SSHD_TEST=1 cargo test --test remote_sshd -- --ignored 2>&1)
  echo "$out" | grep -qE '^test result: FAILED' && { echo "$out" | grep -E '^---- ' | head -5; exit 1; }
  n=$(echo "$out" | grep -oP '^test result: ok\. \K[0-9]+' | head -1)
  [ -n "$n" ] && [ "$n" -ge 8 ] || { echo "only ${n:-0} tests ran against a real sshd"; exit 1; }
  echo "$out" | grep -q 'search' || { echo "no search test among them"; exit 1; }
  echo "REMOTE-SEARCH-OK tests=$n"
  ;;

disk-images)
  out=$(cargo test --lib vfs::image 2>&1)
  echo "$out" | grep -qE '^test result: FAILED' && { echo "$out" | grep -E '^---- ' | head -5; exit 1; }
  n=$(echo "$out" | grep -oP '^test result: ok\. \K[0-9]+' | head -1)
  [ -n "$n" ] && [ "$n" -ge 10 ] || { echo "only ${n:-0} image tests ran; the feature is not there"; exit 1; }
  echo "DISK-IMAGES-OK tests=$n"
  ;;

# The mechanical half of the refactor gate. It cannot read a header against
# what is in the file, which is the half a human owns and which this does not
# claim; it can prove that every module the pass produced says what it owns,
# that the design note names each one, and that nothing was named for the fact
# that it was split off.
refactor-doc)
  # This used to measure the decomposition against docs/REFACTOR.md, which was
  # a working note and is no longer in the repository - so the check was
  # reading a file a fresh clone does not have. What ships is AGENTS.md, so
  # that is what is measured, and the per-module half becomes what can honestly
  # be checked against it: every top-level module is named there, and every
  # module file says at its top what it owns.
  [ -s AGENTS.md ] || { echo "AGENTS.md is missing or empty"; exit 1; }
  unnamed=""
  for d in src/*/; do
    m=$(basename "$d")
    grep -q "src/$m/" AGENTS.md || unnamed="$unnamed $m"
  done
  [ -z "$unnamed" ] || { echo "module not named in AGENTS.md:$unnamed"; exit 1; }
  miss=""
  for f in src/*.rs src/*/*.rs src/*/*/*.rs; do
    [ -e "$f" ] || continue
    case "$f" in */tests.rs) continue ;; esac
    head -1 "$f" | grep -q '^//!' || miss="$miss $f"
  done
  [ -z "$miss" ] || { echo "no module doc comment:$miss"; exit 1; }
  hits=$(grep -rnE '^[[:space:]]*(pub )?(pub\(super\) )?(pub\(crate\) )?(struct|enum|trait) [A-Za-z_]*([23]|Helper|Helpers|Utils?|Misc|Common|Base|Impl|Manager|Data|Info|Parts)\b' src --include=*.rs | grep -v '^src/vfs/archive/index.rs:' || true)
  [ -z "$hits" ] || { echo "$hits"; exit 1; }
  echo "REFACTOR-DOC-OK every module documented and named in AGENTS.md"
  ;;

# The spec is being deleted. Nothing may cite it: not the shipped binary, not
# the config templates, and not the source, whose comments would otherwise
# point at a file that is not there.
no-spec-anywhere)
  # Three separate leaks, because the first version of this gate checked only
  # the word SPEC and passed a tree holding 600 section citations.
  #
  # A section sign is not automatically a defect: RFC 3986 and ECMA-119 are
  # public standards a reader can actually look up, and src/ui/text.rs carries
  # the glyph itself in its ASCII fallback table. Everything else cites a
  # document that is not in this repository, so a reader cannot follow it.
  fail=0

  n=$(grep -rn "SPEC" src/ tests/ examples/ 2>/dev/null | wc -l)
  if [ "$n" -ne 0 ]; then
    echo "cites SPEC by name:"; grep -rn "SPEC" src/ tests/ examples/ | head -5; fail=1
  fi

  n=$(grep -rn "§" src/ tests/ examples/ 2>/dev/null \
      | grep -vE "(RFC [0-9]+|ECMA-[0-9]+|ISO [0-9]+|POSIX|IEEE [0-9]+) §" \
      | grep -v "^src/ui/text.rs:" | wc -l)
  if [ "$n" -ne 0 ]; then
    echo "$n citations to a document that is not in this repository:"
    grep -rn "§" src/ tests/ examples/ \
      | grep -vE "(RFC [0-9]+|ECMA-[0-9]+|ISO [0-9]+|POSIX|IEEE [0-9]+) §" \
      | grep -v "^src/ui/text.rs:" | head -5
    fail=1
  fi

  n=$(grep -rnE "\\bthe the design\\b|\\bthe design the\\b" src/ tests/ examples/ 2>/dev/null | wc -l)
  if [ "$n" -ne 0 ]; then
    echo "$n sentences left broken by an earlier strip:"
    grep -rnE "\\bthe the design\\b|\\bthe design the\\b" src/ tests/ examples/ | head -5; fail=1
  fi

  [ "$fail" -eq 0 ] || exit 1
  echo "NO-SPEC-ANYWHERE-OK source, tests and templates cite nothing absent"
  ;;

# A copy is not done until its bytes are on the medium. Write::flush on a
# std::fs::File does nothing and returns Ok, and dropping the handle discards
# close(2), which is where a network or quota filesystem reports its failure.
# This is the check that the commit is a real one.
durable-copy)
  grep -q "fn commit_partial" src/ops/copy/mod.rs || { echo "commit_partial is gone"; exit 1; }
  # The write handle itself, not the directory: `sync_parent` also calls
  # `sync_all`, so a bare grep for it passes with the file sync deleted, which
  # is how this check first failed its own control.
  awk '/fn commit_partial/,/^}/' src/ops/copy/mod.rs | grep -q "writer.sync_all()" \
    || { echo "commit_partial does not sync the file it was given"; exit 1; }
  # Every path that renames a written partial over a destination must have
  # committed it first. The symlink path writes no bytes and is exempt.
  bad=0
  for f in src/ops/copy/mod.rs src/ops/copy/vfs.rs; do
    r=$(grep -c "fs::rename(&tmp" "$f" || true)
    c=$(grep -cE "commit_partial\(writer" "$f" || true)
    [ "$r" -le 1 ] || [ "$c" -ge 1 ] || bad=1
  done
  [ "$bad" -eq 0 ] || { echo "a partial is renamed into place without a commit"; exit 1; }
  echo "DURABLE-COPY-OK the bytes are on the disk before the rename"
  ;;

# The published release carries every format that was promised. Named
# explicitly rather than counted, because a count passes when six tarballs
# arrive and the deb does not.
release-assets)
  have=$(gh release view v0.1.0 --json assets --jq '.assets[].name' 2>/dev/null || true)
  [ -n "$have" ] || { echo "no release v0.1.0, or its assets cannot be read"; exit 1; }
  missing=""
  for want in \
    hcmd-0.1.0-x86_64-unknown-linux-gnu.tar.gz \
    hcmd-0.1.0-aarch64-unknown-linux-gnu.tar.gz \
    hcmd-0.1.0-x86_64-unknown-linux-musl.tar.gz \
    hcmd-0.1.0-aarch64-unknown-linux-musl.tar.gz \
    hcmd-0.1.0-x86_64-apple-darwin.tar.gz \
    hcmd-0.1.0-aarch64-apple-darwin.tar.gz \
    SHA256SUMS
  do
    echo "$have" | grep -qx "$want" || missing="$missing $want"
  done
  echo "$have" | grep -q '\.deb$'          || missing="$missing a-deb"
  echo "$have" | grep -q '\.rpm$'          || missing="$missing an-rpm"
  echo "$have" | grep -q '\.pkg\.tar\.zst$' || missing="$missing an-arch-package"
  [ -z "$missing" ] || { echo "the release is missing:$missing"; exit 1; }
  echo "RELEASE-ASSETS-OK every format published"
  ;;

# The shell installer, run for real against the published release, into a
# throwaway directory. Proves the whole path: detect, download, verify, unpack,
# install, and a binary that runs.
install-sh)
  dir=$(mktemp -d) || exit 1
  out=$(HCMD_INSTALL_DIR="$dir" sh ./install.sh 2>&1) || {
    echo "$out" | tail -3; rm -rf "$dir"; exit 1; }
  echo "$out" | grep -q "checksum ok" || { echo "the download was not verified"; rm -rf "$dir"; exit 1; }
  v=$("$dir/hcmd" --version 2>&1 || true)
  rm -rf "$dir"
  case "$v" in
    "hcmd 0.1.0"*) echo "INSTALL-SH-OK installed and ran: $v" ;;
    *) echo "the installed binary did not report its version: $v"; exit 1 ;;
  esac
  ;;

# The npm installer, run the same way. Same claim, different route, so both
# are proven rather than one standing in for the other.
install-npx)
  command -v node >/dev/null || { echo "no node on this machine"; exit 1; }
  dir=$(mktemp -d) || exit 1
  out=$(HCMD_INSTALL_DIR="$dir" node packaging/npm/install.js 2>&1) || {
    echo "$out" | tail -3; rm -rf "$dir"; exit 1; }
  echo "$out" | grep -q "checksum ok" || { echo "the download was not verified"; rm -rf "$dir"; exit 1; }
  v=$("$dir/hcmd" --version 2>&1 || true)
  rm -rf "$dir"
  case "$v" in
    "hcmd 0.1.0"*) echo "INSTALL-NPX-OK installed and ran: $v" ;;
    *) echo "the installed binary did not report its version: $v"; exit 1 ;;
  esac
  ;;

# macOS is compiled by a real macOS compiler, not by a simulated cfg here.
macos-builds)
  id=$(gh run list --workflow=CI --branch=master --limit 1 --json databaseId --jq '.[0].databaseId' 2>/dev/null || true)
  [ -n "$id" ] || { echo "no CI run to read"; exit 1; }
  jobs=$(gh run view "$id" --json jobs --jq '.jobs[] | select(.name | test("apple-darwin")) | "\(.name) \(.conclusion)"' 2>/dev/null || true)
  [ -n "$jobs" ] || { echo "the CI run has no macOS job"; exit 1; }
  n=$(echo "$jobs" | wc -l)
  [ "$n" -eq 2 ] || { echo "expected two macOS jobs, found $n: $jobs"; exit 1; }
  if echo "$jobs" | grep -qv "success"; then
    echo "a macOS job has not passed: $(echo "$jobs" | tr '\n' ';')"
    exit 1
  fi
  echo "MACOS-BUILDS-OK both architectures compiled and tested on macOS"
  ;;

*) echo "unknown gate: ${1:-}"; exit 2 ;;
esac
