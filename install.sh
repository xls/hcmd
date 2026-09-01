#!/bin/sh
# Install hcmd.
#
#   curl -fsSL https://raw.githubusercontent.com/xls/hcmd/master/install.sh | sh
#
# Downloads the build for this platform, checks it against the published
# SHA256SUMS, and installs to ~/.local/bin. No root, and nothing outside the
# install directory is touched.
#
#   HCMD_INSTALL_DIR   where to put the binary   (default ~/.local/bin)
#   HCMD_VERSION       which release to fetch    (default the latest)
#
# POSIX sh on purpose: this has to run under dash and busybox ash as well as
# bash, and it is the first thing anyone runs, so it should not need much.
set -eu

REPO="xls/hcmd"
INSTALL_DIR="${HCMD_INSTALL_DIR:-$HOME/.local/bin}"

say()  { printf '%s\n' "$*"; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# The HTTP status of the last fetch, where one was received.
FETCH_HTTP=""

# Fetch $1 to $2.
#
#   0  fetched
#   1  the server answered, but not with the file; FETCH_HTTP has the status
#   2  the server could not be reached at all
#
# The two failures are told apart because they send you to different places.
# "No such release" is a thing to fix on the release page; "no network" is a
# thing to fix on this machine. One message covering both had people checking
# their connection when the repository was simply private, and would have
# them checking the release page when their DNS was down.
fetch() {
    FETCH_HTTP=""
    if have curl; then
        FETCH_HTTP=$(curl -sSL --proto '=https' --tlsv1.2 \
            -w '%{http_code}' -o "$2" "$1" 2>/dev/null)
        [ $? -eq 0 ] || return 2
        case "$FETCH_HTTP" in
            2*) return 0 ;;
            000 | "") return 2 ;;
            *) return 1 ;;
        esac
    elif have wget; then
        wget -qO "$2" "$1" && return 0
        # 4 is a network failure, 8 is a server error response. Anything else
        # is treated as unreachable, which is the more cautious of the two.
        case $? in
            8) return 1 ;;
            *) return 2 ;;
        esac
    else
        die "neither curl nor wget is installed"
    fi
}

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)

    case "$arch" in
        x86_64 | amd64)  arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) die "unsupported architecture: $arch" ;;
    esac

    case "$os" in
        Linux)
            # A musl build runs anywhere; a glibc build is faster to start and
            # smaller. Prefer glibc when this is demonstrably a glibc system.
            if have ldd && ldd --version 2>&1 | grep -qi musl; then
                libc=musl
            elif [ -e /lib/ld-linux-x86-64.so.2 ] || [ -e /lib/ld-linux-aarch64.so.1 ]; then
                libc=gnu
            else
                libc=musl
            fi
            printf '%s-unknown-linux-%s' "$arch" "$libc"
            ;;
        Darwin)
            printf '%s-apple-darwin' "$arch"
            ;;
        *)
            die "unsupported operating system: $os (this installs Linux and macOS builds)"
            ;;
    esac
}

latest_version() {
    tmp=$(mktemp) || die "cannot create a temporary file"
    rc=0
    fetch "https://api.github.com/repos/$REPO/releases/latest" "$tmp" || rc=$?
    case $rc in
        0) ;;
        2) die "cannot reach github.com. Check your network, or set \
HCMD_VERSION to install a known version without asking" ;;
        *) die "github.com answered ${FETCH_HTTP} asking for the latest \
release of ${REPO}. If it has no published release yet, set HCMD_VERSION" ;;
    esac
    # No jq: pull the tag out with sed rather than requiring another tool.
    v=$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\{0,1\}\([^"]*\)".*/\1/p' "$tmp" | head -1)
    rm -f "$tmp"
    [ -n "$v" ] || die "could not work out the latest version; set HCMD_VERSION"
    printf '%s' "$v"
}

verify() {
    # $1 the file, $2 the expected sha256. Skipped, loudly, if no tool is here.
    if have sha256sum; then
        actual=$(sha256sum "$1" | cut -d' ' -f1)
    elif have shasum; then
        actual=$(shasum -a 256 "$1" | cut -d' ' -f1)
    else
        say "warning: no sha256sum or shasum, cannot verify the download"
        return 0
    fi
    [ "$actual" = "$2" ] || die "checksum mismatch: expected $2, got $actual"
    say "checksum ok"
}

main() {
    target=$(detect_target)
    [ -n "$target" ] || die "could not identify this platform"
    version="${HCMD_VERSION:-$(latest_version)}"
    # `die` inside a command substitution only leaves the subshell, so a failed
    # version lookup arrives here as an empty string rather than as an exit.
    # Without this the script cheerfully went on to download "hcmd--<target>".
    [ -n "$version" ] || die "could not work out which version to install; set HCMD_VERSION"
    name="hcmd-${version}-${target}"
    base="https://github.com/$REPO/releases/download/v${version}"

    say "hcmd ${version} for ${target}"

    tmp=$(mktemp -d) || die "cannot create a temporary directory"
    # Leaves nothing behind, on success or on failure.
    trap 'rm -rf "$tmp"' EXIT INT TERM

    say "downloading..."
    rc=0
    fetch "${base}/${name}.tar.gz" "$tmp/pkg.tar.gz" || rc=$?
    case $rc in
        0) ;;
        2) die "cannot reach github.com to download ${name}.tar.gz" ;;
        *) die "no ${target} build published for v${version} (github.com \
answered ${FETCH_HTTP} for ${name}.tar.gz)" ;;
    esac

    if fetch "${base}/SHA256SUMS" "$tmp/SHA256SUMS" 2>/dev/null; then
        want=$(grep " ${name}.tar.gz\$" "$tmp/SHA256SUMS" | cut -d' ' -f1 | head -1)
        if [ -n "$want" ]; then
            verify "$tmp/pkg.tar.gz" "$want"
        else
            say "warning: SHA256SUMS does not list ${name}.tar.gz"
        fi
    else
        say "warning: no SHA256SUMS published, cannot verify the download"
    fi

    tar -xzf "$tmp/pkg.tar.gz" -C "$tmp" || die "the archive did not unpack"
    [ -f "$tmp/$name/hcmd" ] || die "no hcmd binary inside the archive"

    mkdir -p "$INSTALL_DIR" || die "cannot create $INSTALL_DIR"
    # Install to a temporary name in the same directory and rename, so a
    # running hcmd is never half-overwritten.
    install -m 0755 "$tmp/$name/hcmd" "$INSTALL_DIR/.hcmd.new" \
        || die "cannot write to $INSTALL_DIR"
    mv -f "$INSTALL_DIR/.hcmd.new" "$INSTALL_DIR/hcmd" \
        || die "cannot install into $INSTALL_DIR"

    # The 21 themes are compiled into the binary, so every one of them works
    # with no files at all. These are the editable copies: a theme is changed
    # by putting a file of the same name in the config directory, and without
    # a starting point there is nothing to copy. The tarball already carries
    # them, so keeping them costs one `cp`.
    share="${HCMD_SHARE_DIR:-$HOME/.local/share/hcmd}"
    if [ -d "$tmp/$name/themes" ]; then
        mkdir -p "$share" 2>/dev/null && cp -r "$tmp/$name/themes" "$share/" 2>/dev/null \
            && say "themes in $share/themes"
    fi
    if [ -d "$tmp/$name/examples" ]; then
        if mkdir -p "$share" 2>/dev/null && cp -r "$tmp/$name/examples" "$share/" 2>/dev/null; then
            say "examples in $share/examples (keymap, config)"
        else
            say "warning: could not write the examples to $share"
        fi
    fi

    say "installed $INSTALL_DIR/hcmd"

    # An upgrade over an existing config will not show options added since the
    # file was written. Offer to append commented examples of them; it never
    # changes an existing setting. Default yes, but only when a config already
    # exists and only on a real terminal - never modify files unprompted in a
    # pipe or CI.
    config="${XDG_CONFIG_HOME:-$HOME/.config}/holoscommander/config.toml"
    if [ -f "$config" ] && [ -t 0 ]; then
        say ""
        printf '%s' "Add examples of new config options to your existing config? [Y/n] "
        read -r reply || reply=""
        case "$reply" in
            [Nn]*) say "skipped; run 'hcmd --update-config' any time" ;;
            *) "$INSTALL_DIR/hcmd" --update-config || true ;;
        esac
    fi

    case ":$PATH:" in
        *":$INSTALL_DIR:"*) ;;
        *)
            say ""
            say "$INSTALL_DIR is not on your PATH. Add this to your shell profile:"
            say "    export PATH=\"\$PATH:$INSTALL_DIR\""
            ;;
    esac

    say ""
    "$INSTALL_DIR/hcmd" --version || true
    say "Run hcmd to start. Configuration is written to"
    say "~/.config/holoscommander/ the first time it runs."
}

main "$@"
