#!/bin/sh
# wavecode installer: fetches the latest release binary for this
# platform from GitHub Releases, verifies its sha256 against the
# published checksum file, and installs it into ~/.cargo/bin (or
# $WAVECODE_INSTALL_DIR). No archive tooling needed — releases carry
# bare binaries alongside the archives.
#
# Usage: curl -fsSL https://raw.githubusercontent.com/daftpunkwav/wave-code/main/scripts/install.sh | sh
# (pipe with care: review the script first, or download and run it.)
set -eu

REPO="daftpunkwav/wave-code"
BIN="wavecode"

dest="${WAVECODE_INSTALL_DIR:-$HOME/.cargo/bin}"
mkdir -p "$dest"

# Map the running platform onto one of the release targets (the same
# matrix release.yml builds).
case "$(uname -s)/$(uname -m)" in
    Linux/x86_64) target="x86_64-unknown-linux-gnu" ;;
    Darwin/arm64) target="aarch64-apple-darwin" ;;
    Darwin/x86_64) target="x86_64-apple-darwin" ;;
    *)
        echo "unsupported platform: $(uname -s)/$(uname -m)" >&2
        echo "build from source instead: cargo install --git https://github.com/$REPO" >&2
        exit 1
        ;;
esac

# The latest release moves; resolve its tag once so both downloads
# come from the same release even if one lands mid-publish.
api="https://api.github.com/repos/$REPO/releases/latest"
if command -v curl >/dev/null; then
    tag=$(curl -fsSL "$api" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
else
    echo "need curl to download releases" >&2
    exit 1
fi
if [ -z "$tag" ]; then
    echo "could not resolve the latest release tag" >&2
    exit 1
fi

base="https://github.com/$REPO/releases/download/$tag"
asset="wavecode-${tag#v}-${target}-bin"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "downloading $tag ($target)..."
curl -fsSL -o "$tmp/$BIN" "$base/$asset"
curl -fsSL -o "$tmp/$BIN.sha256" "$base/$asset.sha256"

# The checksum file is "<hex>  <filename>"; compare the downloaded
# bytes against the published hex only.
expected=$(cut -d' ' -f1 "$tmp/$BIN.sha256")
if command -v sha256sum >/dev/null; then
    actual=$(sha256sum "$tmp/$BIN" | cut -d' ' -f1)
elif command -v shasum >/dev/null; then
    actual=$(shasum -a 256 "$tmp/$BIN" | cut -d' ' -f1)
else
    echo "need sha256sum or shasum to verify the download" >&2
    exit 1
fi
if [ "$expected" != "$actual" ]; then
    echo "checksum mismatch: expected $expected, got $actual" >&2
    exit 1
fi

chmod 755 "$tmp/$BIN"
mv "$tmp/$BIN" "$dest/$BIN"

echo "installed $tag to $dest/$BIN"
case ":$PATH:" in
    *":$dest:"*) ;;
    *) echo "note: $dest is not on your PATH" ;;
esac
"$dest/$BIN" --version
