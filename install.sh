#!/bin/sh
# Foremerge installer: downloads the right release binary from GitHub,
# verifies its SHA-256 checksum, and installs it to ~/.local/bin.
#
#   curl -fsSL https://naw103.github.io/foremerge/install.sh | sh
#
# Options (environment variables):
#   FOREMERGE_INSTALL_DIR   install directory (default: ~/.local/bin)
#   FOREMERGE_VERSION       tag to install, e.g. v0.2.0 (default: latest)
set -eu

REPO="naw103/foremerge"
INSTALL_DIR="${FOREMERGE_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*" >&2; }
fail() { say "foremerge install: $*"; exit 1; }

command -v curl >/dev/null || fail "curl is required"
command -v tar >/dev/null || fail "tar is required"

os=$(uname -s)
arch=$(uname -m)
case "$os" in
  Darwin)
    case "$arch" in
      arm64) target="aarch64-apple-darwin" ;;
      x86_64) target="x86_64-apple-darwin" ;;
      *) fail "unsupported macOS architecture: $arch" ;;
    esac
    ;;
  Linux)
    case "$arch" in
      x86_64) target="x86_64-unknown-linux-gnu" ;;
      aarch64 | arm64) target="aarch64-unknown-linux-gnu" ;;
      *) fail "unsupported Linux architecture: $arch" ;;
    esac
    ;;
  *)
    fail "unsupported platform: $os. On Windows, download the zip from https://github.com/$REPO/releases or use: cargo install --locked foremerge"
    ;;
esac

version="${FOREMERGE_VERSION:-}"
if [ -z "$version" ]; then
  version=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" | sed 's#.*/tag/##')
  [ -n "$version" ] || fail "could not resolve the latest release tag"
fi

archive="foremerge-$version-$target.tar.gz"
url="https://github.com/$REPO/releases/download/$version/$archive"

workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT

say "Downloading foremerge $version for $target..."
curl -fsSL -o "$workdir/$archive" "$url" || fail "download failed: $url"
curl -fsSL -o "$workdir/$archive.sha256" "$url.sha256" || fail "checksum download failed"

cd "$workdir"
expected=$(awk '{print $1}' "$archive.sha256")
if command -v shasum >/dev/null; then
  actual=$(shasum -a 256 "$archive" | awk '{print $1}')
else
  actual=$(sha256sum "$archive" | awk '{print $1}')
fi
[ "$expected" = "$actual" ] || fail "checksum mismatch for $archive"

tar -xzf "$archive"
mkdir -p "$INSTALL_DIR"
install -m 755 foremerge "$INSTALL_DIR/foremerge"

say "Installed $("$INSTALL_DIR/foremerge" --version) to $INSTALL_DIR/foremerge"

# `fmg` is the same program under a short name. Releases before it existed have
# no such file, so this stays quiet rather than failing on an older --version.
if [ -f fmg ]; then
  if [ -e "$INSTALL_DIR/fmg" ] || ! command -v fmg >/dev/null 2>&1; then
    install -m 755 fmg "$INSTALL_DIR/fmg"
    say "Installed the short name fmg to $INSTALL_DIR/fmg"
  else
    # Something else already answers to fmg. Leave it alone and say so, rather
    # than shadowing a program the user installed on purpose.
    say "Skipped the short name fmg: $(command -v fmg) already exists."
    say "  Foremerge is still installed as foremerge."
  fi
fi
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) say "Note: $INSTALL_DIR is not on your PATH. Add it, for example:"
     say "  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
say ""
say "Next, inside a repository you want to coordinate:"
say "  foremerge init && foremerge setup all"
