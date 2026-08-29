#!/bin/sh
# Install nchctop.
#
#   curl -LsSf https://raw.githubusercontent.com/tanchihpin0517/nchctop/main/install.sh | sh
#
# Options, which a piped run passes after `sh -s --`:
#
#   --version <v>   a release to install, without the leading v (default: latest)
#   --dir <path>    where to put the binary (default: ~/.local/bin)
#   --help
#
# The equivalent environment variables are NCHCTOP_VERSION and
# NCHCTOP_INSTALL_DIR; an option wins over the variable.

set -eu

REPO="tanchihpin0517/nchctop"
ASSET="nchctop-x86_64-linux"

VERSION="${NCHCTOP_VERSION:-}"
INSTALL_DIR="${NCHCTOP_INSTALL_DIR:-}"

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
Install nchctop.

    curl -LsSf https://raw.githubusercontent.com/tanchihpin0517/nchctop/main/install.sh | sh

Options, which a piped run passes after `sh -s --`:

    --version <v>   a release to install, without the leading v (default: latest)
    --dir <path>    where to put the binary (default: ~/.local/bin)
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
        -h | --help)
            usage
            exit 0
            ;;
        *)
            err "unknown option: $1 (try --help)"
            ;;
    esac
done

# Only one build is released, so anything else is a clear error rather than a
# download that turns out not to run.
os="$(uname -s)"
arch="$(uname -m)"

[ "$os" = "Linux" ] || err "no build for $os; install from source with: cargo install --git https://github.com/$REPO"

case "$arch" in
    x86_64 | amd64) ;;
    *) err "no build for $arch; install from source with: cargo install --git https://github.com/$REPO" ;;
esac

if [ -n "$VERSION" ]; then
    # A tag people are as likely to write with the v as without it.
    tag="v${VERSION#v}"
    base="https://github.com/$REPO/releases/download/$tag"
else
    base="https://github.com/$REPO/releases/latest/download"
fi

: "${INSTALL_DIR:=$HOME/.local/bin}"

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -LsSf --proto '=https' --tlsv1.2 "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q "$1" -O "$2"; }
else
    err "neither curl nor wget is available"
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

say "downloading $ASSET from $base"
fetch "$base/$ASSET" "$tmp/nchctop" || err "could not download $base/$ASSET"

# A missing checksum is not fatal — older releases may not carry one — but a
# checksum that disagrees is.
if fetch "$base/$ASSET.sha256" "$tmp/sum" 2>/dev/null; then
    want="$(cut -d' ' -f1 <"$tmp/sum")"
    if got="$(sha256 "$tmp/nchctop")"; then
        [ "$want" = "$got" ] || err "checksum mismatch: expected $want, got $got"
        say "checksum ok"
    else
        say "no sha256 tool available, skipping the checksum"
    fi
else
    say "no published checksum, skipping the check"
fi

chmod +x "$tmp/nchctop"
mkdir -p "$INSTALL_DIR"
# Replace by rename, so a running nchctop keeps the file it started from.
mv -f "$tmp/nchctop" "$INSTALL_DIR/nchctop"

say "installed $("$INSTALL_DIR/nchctop" --version) to $INSTALL_DIR/nchctop"

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        say ""
        say "$INSTALL_DIR is not on your PATH. To add it:"
        say ""
        say "    echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.bashrc"
        say "    export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac
