#!/bin/sh
# Installs the perspica binary from the latest GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/sshah03/perspica/main/install.sh | sh
#
# PERSPICA_VERSION picks a release, like 0.1.8. PERSPICA_INSTALL_DIR picks where the
# binary goes. The default is ~/.local/bin.
set -eu

repo="sshah03/perspica"
dir="${PERSPICA_INSTALL_DIR:-$HOME/.local/bin}"

fail() { echo "perspica install: $*" >&2; exit 1; }
command -v curl >/dev/null || fail "curl is needed"

case "$(uname -s)" in
  Darwin) os="apple-darwin" ;;
  Linux) os="unknown-linux-gnu" ;;
  *) fail "no binary for $(uname -s). On Windows, download the zip from https://github.com/$repo/releases" ;;
esac

case "$(uname -m)" in
  arm64 | aarch64) arch="aarch64" ;;
  x86_64 | amd64) arch="x86_64" ;;
  *) fail "no binary for $(uname -m). Try cargo install perspica" ;;
esac
# A shell running under Rosetta reports x86_64 on an Apple silicon Mac. Use the native build.
if [ "$os" = "apple-darwin" ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = "1" ]; then
  arch="aarch64"
fi

name="perspica-$arch-$os"
if [ -n "${PERSPICA_VERSION:-}" ]; then
  base="https://github.com/$repo/releases/download/v${PERSPICA_VERSION#v}"
else
  base="https://github.com/$repo/releases/latest/download"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading $name"
curl -fsSL "$base/$name.tar.gz" -o "$tmp/$name.tar.gz" || fail "download failed from $base/$name.tar.gz"
curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" || fail "download failed from $base/SHA256SUMS"

expected="$(grep " $name.tar.gz\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)"
[ -n "$expected" ] || fail "$name.tar.gz is not in SHA256SUMS"
if command -v sha256sum >/dev/null; then
  actual="$(sha256sum "$tmp/$name.tar.gz" | cut -d' ' -f1)"
else
  actual="$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d' ' -f1)"
fi
[ "$actual" = "$expected" ] || fail "checksum mismatch for $name.tar.gz"

tar xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$dir"
install -m 755 "$tmp/$name/perspica" "$dir/perspica"
echo "Installed $("$dir/perspica" --version) to $dir/perspica"

case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "$dir isn't on your PATH. Add this to your shell profile: export PATH=\"$dir:\$PATH\"" ;;
esac
