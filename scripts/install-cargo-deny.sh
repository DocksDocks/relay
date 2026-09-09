#!/usr/bin/env sh
# Install the pinned prebuilt cargo-deny release into the cargo bin directory.
# The gate (scripts/gate.mjs) refuses any other version. CI and local setup both
# call this script, so the version and checksums below are the single pin.
set -eu

VERSION="0.20.2"
case "$(uname -m)" in
  x86_64) ARCH="x86_64"; SHA256="9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f" ;;
  aarch64 | arm64) ARCH="aarch64"; SHA256="995c82be0defc7a025cae49a2aa2644ce8245c9a3318fc4103907c6a285e8c7d" ;;
  *) echo "install-cargo-deny: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

BIN_DIR="${CARGO_HOME:-$HOME/.cargo}/bin"
if [ -x "$BIN_DIR/cargo-deny" ] && [ "$("$BIN_DIR/cargo-deny" --version)" = "cargo-deny $VERSION" ]; then
  echo "cargo-deny $VERSION already installed in $BIN_DIR"
  exit 0
fi

ASSET="cargo-deny-$VERSION-$ARCH-unknown-linux-musl"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
curl -sSfL -o "$WORK/$ASSET.tar.gz" \
  "https://github.com/EmbarkStudios/cargo-deny/releases/download/$VERSION/$ASSET.tar.gz"
echo "$SHA256  $WORK/$ASSET.tar.gz" | sha256sum -c - >/dev/null
tar -xzf "$WORK/$ASSET.tar.gz" -C "$WORK"
mkdir -p "$BIN_DIR"
install -m 755 "$WORK/$ASSET/cargo-deny" "$BIN_DIR/cargo-deny"
"$BIN_DIR/cargo-deny" --version
