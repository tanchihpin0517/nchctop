#!/bin/sh
# Install or update nchctop.
#
#   curl -LsSf https://raw.githubusercontent.com/tanchihpin0517/nchctop/main/install.sh | sh
#
# Run again to update: the same command replaces an existing binary, and stops
# early when it is already the version being asked for.
#
# Options, which a piped run passes after `sh -s --`:
#
#   --version <v>   a release to install, without the leading v (default: latest)
#   --dir <path>    where to put the binary (default: ~/.local/bin)
#   --force         reinstall even if that version is already there
#   --help
#
# The equivalent environment variables are NCHCTOP_VERSION and
# NCHCTOP_INSTALL_DIR; an option wins over the variable.

set -eu

REPO="tanchihpin0517/nchctop"
ASSET="nchctop-x86_64-linux"

VERSION="${NCHCTOP_VERSION:-}"
INSTALL_DIR="${NCHCTOP_INSTALL_DIR:-}"
FORCE=0

say() {
    printf '%s\n' "$*"
}

err() {
    printf 'install.sh: %s\n' "$*" >&2
    exit 1
}

# Spelled out rather than read back out of $0: piped into sh, this script has
# no file to read itself from.
usage() {
    cat <<'USAGE'
Install or update nchctop.

    curl -LsSf https://raw.githubusercontent.com/tanchihpin0517/nchctop/main/install.sh | sh

Run again to update: the same command replaces an existing binary, and stops
early when it is already the version being asked for.

Options, which a piped run passes after `sh -s --`:

    --version <v>   a release to install, without the leading v (default: latest)
    --dir <path>    where to put the binary (default: ~/.local/bin)
    --force         reinstall even if that version is already there
    --help

The equivalent environment variables are NCHCTOP_VERSION and
NCHCTOP_INSTALL_DIR; an option wins over the variable.
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || err "--version needs a value"
            VERSION="$2"
            shift 2
            ;;
        --dir)
            [ $# -ge 2 ] || err "--dir needs a value"
            INSTALL_DIR="$2"
            shift 2
            ;;
        --force)
            FORCE=1
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            err "Unknown option: $1 (try --help)"
            ;;
    esac
done

# Only one build is released, so anything else is a clear error rather than a
# download that turns out not to run.
os="$(uname -s)"
arch="$(uname -m)"

[ "$os" = "Linux" ] || err "No build for $os; install from source with: cargo install --git https://github.com/$REPO"

case "$arch" in
    x86_64 | amd64) ;;
    *) err "No build for $arch; install from source with: cargo install --git https://github.com/$REPO" ;;
esac

if [ -n "$VERSION" ]; then
    # A tag people are as likely to write with the v as without it.
    tag="v${VERSION#v}"
    base="https://github.com/$REPO/releases/download/$tag"
else
    base="https://github.com/$REPO/releases/latest/download"
fi

: "${INSTALL_DIR:=$HOME/.local/bin}"
target="$INSTALL_DIR/nchctop"

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -LsSf --proto '=https' --tlsv1.2 "$1" -o "$2"; }
    # Headers alone, redirects followed: the answer is the address we end up
    # at, not anything in the body.
    redirect() { curl -sIL --proto '=https' --tlsv1.2 -o /dev/null -w '%{url_effective}' "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q "$1" -O "$2"; }
    redirect() {
        wget -qS --spider --max-redirect=0 "$1" 2>&1 |
            awk 'tolower($1) == "location:" { sub(/\r$/, "", $2); print $2 }'
    }
else
    err "Neither curl nor wget is available"
fi

# The version a binary reports, or nothing if it is missing or will not run
# here. `nchctop --version` prints `nchctop <v>`.
version_of() {
    [ -x "$1" ] || return 0
    "$1" --version 2>/dev/null | awk 'NR == 1 { print $NF }'
}

# The version of the latest release, taken from the tag that
# /releases/latest redirects to. Nothing depends on knowing it: when it cannot
# be worked out the download below settles the same question, a few megabytes
# later.
latest_version() {
    url="$(redirect "https://github.com/$REPO/releases/latest")" || return 0

    case "$url" in
        */releases/tag/v*) printf '%s\n' "${url##*/tag/v}" ;;
    esac
}

installed="$(version_of "$target")"
if [ -n "$installed" ]; then
    say "Found nchctop $installed in $INSTALL_DIR"
fi

# Which release we would install, asked for by name rather than downloaded: a
# tag costs a header, the binary costs megabytes. Worth doing on every run,
# because the update check on startup is one of these.
if [ -n "$VERSION" ]; then
    want="${VERSION#v}"
else
    want="$(latest_version)"
fi

if [ -n "$want" ] && [ "$installed" = "$want" ] && [ "$FORCE" -eq 0 ]; then
    say "Already at $installed, nothing to do (--force to reinstall)"
    exit 0
fi

if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
    sha256() { return 1; }
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading $ASSET from $base"
fetch "$base/$ASSET" "$tmp/nchctop" || err "Could not download $base/$ASSET"

# A missing checksum is not fatal — older releases may not carry one — but a
# checksum that disagrees is.
if fetch "$base/$ASSET.sha256" "$tmp/sum" 2>/dev/null; then
    want="$(cut -d' ' -f1 <"$tmp/sum")"
    if got="$(sha256 "$tmp/nchctop")"; then
        [ "$want" = "$got" ] || err "Checksum mismatch: expected $want, got $got"
        say "Checksum ok"
    else
        say "No sha256 tool available, skipping the checksum"
    fi
else
    say "No published checksum, skipping the check"
fi

chmod +x "$tmp/nchctop"

# Asking the download its version is also the check that it runs here, so a
# binary that cannot start never displaces a working one.
downloaded="$(version_of "$tmp/nchctop")"
[ -n "$downloaded" ] || err "The downloaded binary does not run on this machine; install from source with: cargo install --git https://github.com/$REPO"

if [ "$installed" = "$downloaded" ] && [ "$FORCE" -eq 0 ]; then
    say "Already at $installed, nothing to do (--force to reinstall)"
    exit 0
fi

mkdir -p "$INSTALL_DIR"
# Replace by rename, so a running nchctop keeps the file it started from.
mv -f "$tmp/nchctop" "$target"

if [ "$installed" = "$downloaded" ]; then
    say "Reinstalled nchctop $downloaded in $INSTALL_DIR"
elif [ -n "$installed" ]; then
    say "Updated nchctop $installed -> $downloaded in $INSTALL_DIR"
else
    say "Installed nchctop $downloaded to $target"
fi

case ":$PATH:" in
    *":$INSTALL_DIR:"*)
        # Installing into a directory that PATH reaches second leaves the old
        # binary answering, which otherwise looks like an update that did
        # nothing.
        found="$(command -v nchctop || true)"
        if [ -n "$found" ] && [ "$found" != "$target" ]; then
            say ""
            say "Note: $found comes first on your PATH, so it is the one that runs."
        fi
        ;;
    *)
        say ""
        say "$INSTALL_DIR is not on your PATH. To add it:"
        say ""
        say "    echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.bashrc"
        say "    export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac
