#!/bin/sh
# Build an .rpm from an already-compiled release binary.
set -eu

cd "$(dirname "$0")/.."
version=$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
bin=target/release/hcmd

[ -x "$bin" ] || { echo "$bin is not there; run cargo build --release first" >&2; exit 1; }

top=$(mktemp -d)
trap 'rm -rf "$top"' EXIT
mkdir -p "$top/BUILD" "$top/RPMS" "$top/SOURCES" "$top/SPECS" "$top/BUILDROOT"

install -D -m 0755 "$bin" "$top/BUILD/usr/bin/hcmd"
install -D -m 0644 README.md "$top/BUILD/usr/share/doc/hcmd/README.md"
install -D -m 0644 FEATURES.md "$top/BUILD/usr/share/doc/hcmd/FEATURES.md"
mkdir -p "$top/BUILD/usr/share/hcmd/examples" "$top/BUILD/usr/share/hcmd/themes"
cp -r examples/. "$top/BUILD/usr/share/hcmd/examples/"
cp -r themes/. "$top/BUILD/usr/share/hcmd/themes/"

sed "s/@VERSION@/$version/" packaging/hcmd.spec > "$top/SPECS/hcmd.spec"

rpmbuild --define "_topdir $top" \
         --define "_builddir $top/BUILD" \
         --buildroot "$top/BUILD" \
         -bb "$top/SPECS/hcmd.spec"

mkdir -p dist
find "$top/RPMS" -name '*.rpm' -exec cp {} dist/ \;
ls dist/*.rpm
