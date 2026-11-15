#!/bin/sh
# Build a release tarball plus checksum in dist/.
# Usage: scripts/build-release.sh   (requires cargo and a Rust toolchain)
set -eu

cd "$(dirname "$0")/.."

version=$(awk -F'"' '/^version = / { print $2; exit }' Cargo.toml)
target=$(rustc -vV | sed -n 's/^host: //p')
name="syslens-${version}-${target}"

cargo build --locked --release

rm -rf "dist/$name" "dist/$name.tar.gz" "dist/$name.tar.gz.sha256"
mkdir -p "dist/$name"
cp target/release/syslens README.md LICENSE CHANGELOG.md "dist/$name"/
cp -r systemd docs "dist/$name"/
tar -C dist -czf "dist/$name.tar.gz" "$name"
(cd dist && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")

echo "built dist/$name.tar.gz"
echo "sha256: $(cat "dist/$name.tar.gz.sha256")"
