#!/bin/sh
# install.sh — install the shunt server-side binary without cargo.
#
#   curl -fsSL https://raw.githubusercontent.com/ShinoharaHaruna/shunt/main/install.sh | sh
#
# Commands:
#   install (default)   fetch the latest release, verify, install
#   uninstall           remove the installed binary
#   upgrade             placeholder — upgrades are `shunt upgrade`
#
# Install options:
#   --musl              static build (default; runs on any libc —
#                       the same asset `upgrade -c` ships to boxes)
#   --gnu, --glibc      dynamic-linked build for glibc systems
#   --dir DIR           install location (default: ~/.local/bin,
#                       or $SHUNT_INSTALL_DIR)
#
# Non-goals: linux-x86_64 only (the only targets releases publish);
# no shell completion install (run `shunt completions <shell>` and
# redirect it yourself); no $SHUNT_HOME setup. Uninstall removes
# only the binary — registry, keys, state, and queues are user data
# and are listed, never deleted.

set -eu

REPO="ShinoharaHaruna/shunt"
TARGET="x86_64-unknown-linux-musl"
DIR="${SHUNT_INSTALL_DIR:-$HOME/.local/bin}"
ACTION="install"

die() {
    echo "install.sh: $*" >&2
    exit 1
}

need() {
    command -v "$1" >/dev/null 2>&1 || die "missing '$1' — install it and re-run"
}

usage() {
    cat <<'EOF'
Usage: install.sh [install|uninstall|upgrade] [--musl|--gnu] [--dir DIR]

  install (default)  Install the latest shunt release (musl static).
  uninstall          Remove the installed binary only.
  upgrade            Upgrades are `shunt upgrade` — this prints why.

  --musl             Static build (default — runs on any libc).
  --gnu, --glibc     Dynamic-linked glibc build.
  --dir DIR          Install dir (default: ~/.local/bin or $SHUNT_INSTALL_DIR).
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        install | uninstall | upgrade)
            ACTION="$1"
            ;;
        --musl)
            TARGET="x86_64-unknown-linux-musl"
            ;;
        --gnu | --glibc)
            TARGET="x86_64-unknown-linux-gnu"
            ;;
        --dir)
            [ $# -ge 2 ] || die "--dir needs a value"
            DIR="$2"
            shift
            ;;
        -h | --help | help)
            usage
            exit 0
            ;;
        *)
            usage >&2
            die "unknown argument: $1"
            ;;
    esac
    shift
done

# The tag is embedded in the asset filename; `releases/latest`
# 302s to `releases/tag/<TAG>` — one HEAD call, no API, and it
# always lands on the latest stable (never a prerelease).
resolve_tag() {
    url=$(curl -fsSI -o /dev/null -w '%{redirect_url}' "https://github.com/$REPO/releases/latest") || die "could not reach GitHub releases"
    tag="${url##*/}"
    [ -n "$tag" ] && [ "$tag" != "$url" ] || die "could not resolve the latest release tag"
    echo "$tag"
}

do_install() {
    need curl
    need tar
    need sha256sum
    tag=$(resolve_tag)
    asset="shunt-${tag}-${TARGET}.tar.gz"
    base="https://github.com/$REPO/releases/download/$tag"
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT INT TERM

    [ -x "$DIR/shunt" ] && echo "current: $("$DIR/shunt" --version 2>/dev/null || echo unknown)"
    echo "installing shunt $tag ($TARGET) -> $DIR"

    curl -fsSL "$base/$asset" -o "$tmp/$asset" || die "download failed: $base/$asset"
    curl -fsSL "$base/SHA256SUMS-$TARGET" -o "$tmp/SHA256SUMS" || die "download failed: $base/SHA256SUMS-$TARGET"
    (cd "$tmp" && sha256sum --check SHA256SUMS) || die "checksum verification failed — refusing to install"

    tar -xzf "$tmp/$asset" -C "$tmp"
    bin="$tmp/shunt-${tag}-${TARGET}/shunt"
    [ -f "$bin" ] || die "archive layout unexpected: no shunt binary inside"

    # Same discipline as `shunt upgrade` (design.md §16): the
    # extracted binary must self-report the tag's version.
    ver=$("$bin" --version 2>&1) || die "the downloaded binary does not run on this system"
    case "$ver" in
        *"${tag#v}"*) : ;;
        *) die "version mismatch: tag is $tag but the binary reports '$ver'" ;;
    esac

    mkdir -p "$DIR"
    install -m 755 "$bin" "$DIR/shunt"
    echo "installed: $("$DIR/shunt" --version)"

    case ":$PATH:" in
        *":$DIR:"*) : ;;
        *) echo "note: $DIR is not on your PATH" ;;
    esac
    echo "shell completions: $DIR/shunt completions zsh > <your-completion-dir>/_shunt"
}

do_uninstall() {
    bin="$DIR/shunt"
    if [ ! -e "$bin" ]; then
        echo "no shunt binary at $bin — nothing to do"
        exit 0
    fi
    # Deleting under a live daemon is inode-safe on linux, but the
    # user should know what's still running before we remove things.
    if pgrep -x shunt >/dev/null 2>&1; then
        echo "warning: shunt processes are running — stop them first:" >&2
        echo "  shunt stop        # queue daemon" >&2
        echo "  shunt serve down  # dashboard" >&2
    fi
    rm -f "$bin"
    echo "removed $bin"
    cat <<'EOF'
Left in place — user data, remove explicitly if you want it gone:
  ~/.config/shunt       registry, keys, spool, rooms
  ~/.local/state/shunt  daemon state, qroot registry, serve pidfile
  ~/.cache/shunt        caches, if any
  your shell completion file, if you installed one
EOF
}

case "$ACTION" in
    install)
        do_install
        ;;
    uninstall)
        do_uninstall
        ;;
    upgrade)
        # Deliberately a pointer, not a second implementation —
        # upgrade's real semantics (checksum + version verify,
        # daemon handoff afterwards) live in `shunt upgrade`.
        echo "Upgrades are handled by shunt itself:"
        echo "  shunt upgrade --check   # report latest vs current"
        echo "  shunt upgrade           # verify + install the latest release"
        ;;
esac
