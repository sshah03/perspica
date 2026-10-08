#!/bin/sh
# Prints the Homebrew formula for a release, with checksums from its SHA256SUMS.
# After a release, update the tap (github.com/sshah03/homebrew-perspica) with:
#
#   scripts/homebrew-formula.sh 0.1.9 > ../homebrew-perspica/Formula/perspica.rb
set -eu

version="${1:?usage: scripts/homebrew-formula.sh <version>}"
version="${version#v}"
base="https://github.com/sshah03/perspica/releases/download/v$version"
sums="$(curl -fsSL "$base/SHA256SUMS")"

sha() {
  s="$(printf '%s\n' "$sums" | grep " perspica-$1.tar.gz\$" | cut -d' ' -f1)"
  [ -n "$s" ] || { echo "perspica-$1.tar.gz is not in SHA256SUMS for v$version" >&2; exit 1; }
  echo "$s"
}

cat <<EOF
class Perspica < Formula
  desc "Review code changes by what they do, not line by line"
  homepage "https://github.com/sshah03/perspica"
  version "$version"
  license "MIT"

  on_macos do
    on_arm do
      url "$base/perspica-aarch64-apple-darwin.tar.gz"
      sha256 "$(sha aarch64-apple-darwin)"
    end
    on_intel do
      url "$base/perspica-x86_64-apple-darwin.tar.gz"
      sha256 "$(sha x86_64-apple-darwin)"
    end
  end

  on_linux do
    on_arm do
      url "$base/perspica-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "$(sha aarch64-unknown-linux-gnu)"
    end
    on_intel do
      url "$base/perspica-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "$(sha x86_64-unknown-linux-gnu)"
    end
  end

  def install
    bin.install "perspica"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/perspica --version")
  end
end
EOF
