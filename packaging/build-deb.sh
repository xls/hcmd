#!/bin/sh
# Build a .deb from an already-compiled release binary.
#
# Hand-rolled rather than cargo-deb: it is thirty lines of dpkg-deb, it does not
# add a build dependency, and it keeps the control file where a packager can
# read it.
set -eu

cd "$(dirname "$0")/.."
version=$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
arch=$(dpkg --print-architecture 2>/dev/null || echo amd64)
bin=target/release/hcmd

[ -x "$bin" ] || { echo "$bin is not there; run cargo build --release first" >&2; exit 1; }

root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT

mkdir -p "$root/DEBIAN" \
         "$root/usr/bin" \
         "$root/usr/share/doc/hcmd" \
         "$root/usr/share/hcmd/examples"

install -m 0755 "$bin" "$root/usr/bin/hcmd"
install -m 0644 README.md FEATURES.md "$root/usr/share/doc/hcmd/"
cp -r examples/. "$root/usr/share/hcmd/examples/"
mkdir -p "$root/usr/share/hcmd/themes"
cp -r themes/. "$root/usr/share/hcmd/themes/"
[ -f LICENSE ] && install -m 0644 LICENSE "$root/usr/share/doc/hcmd/copyright"

# The binary links libc and libstdc++ and nothing else; the shipped xz is
# static. Versions are left to the resolver rather than pinned to the builder's.
cat > "$root/DEBIAN/control" <<EOF
Package: hcmd
Version: $version
Section: utils
Priority: optional
Architecture: $arch
Depends: libc6, libstdc++6
Maintainer: holoscommander
Description: A Total Commander alternative for the terminal, for fingers that learned F5 in 1998
 Two panels, the classic function keys, and a viewer that opens a very large
 file as fast as a small one. Archives, read-only disk images, SFTP and FTP all
 browse as directories. Search runs in process; nothing is shelled out to.
EOF

mkdir -p dist
dpkg-deb --build --root-owner-group "$root" "dist/hcmd_${version}_${arch}.deb"
echo "dist/hcmd_${version}_${arch}.deb"
