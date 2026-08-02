#!/usr/bin/env bash
set -euo pipefail

if (( $# > 1 )); then
  echo "usage: $0 [vSEMVER]" >&2
  exit 2
fi

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

version=$(awk -F '"' '/^version = "/ { print $2; exit }' Cargo.toml)
if [[ ! $version =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$ ]]; then
  echo "Cargo.toml package version is not a release version: $version" >&2
  exit 1
fi

if (( $# == 1 )) && [[ $1 != "v$version" ]]; then
  echo "tag $1 does not match Cargo.toml version $version (expected v$version)" >&2
  exit 1
fi

cargo metadata --locked --no-deps --format-version 1 >/dev/null
cargo package --locked

package="target/package/packet-tide-$version.crate"
if [[ ! -f $package ]]; then
  echo "cargo package did not create $package" >&2
  exit 1
fi

listing=$(tar -tzf "$package")
prefix="packet-tide-$version"
for required in Cargo.toml Cargo.lock README.md DESIGN.md LICENSE release/README.md src/main.rs; do
  if ! grep -Fxq "$prefix/$required" <<<"$listing"; then
    echo "source package is missing $required" >&2
    exit 1
  fi
done
if grep -Eq "/(lab|results|target|\.github)/" <<<"$listing"; then
  echo "source package contains development-only files" >&2
  exit 1
fi

temporary=$(mktemp -d "${TMPDIR:-/tmp}/packet-tide-release.XXXXXX")
trap 'rm -rf "$temporary"' EXIT
tar -xzf "$package" -C "$temporary"
cargo install --locked --path "$temporary/$prefix" --root "$temporary/install"

binary="$temporary/install/bin/packet-tide"
actual_version=$($binary --version)
if [[ $actual_version != "packet-tide $version" ]]; then
  echo "installed binary reported unexpected version: $actual_version" >&2
  exit 1
fi
$binary --help >/dev/null 2>&1
$binary keygen --out "$temporary/test.key"
if [[ $(wc -c <"$temporary/test.key") -ne 32 ]]; then
  echo "installed binary generated a key with the wrong length" >&2
  exit 1
fi
if $binary keygen --out "$temporary/test.key" >/dev/null 2>&1; then
  echo "installed binary overwrote an existing key" >&2
  exit 1
fi

echo "release source package and install smoke test passed for v$version"
