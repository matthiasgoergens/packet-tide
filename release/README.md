# Release packaging

The crate requires Rust 1.88 or newer and can be installed from a checkout with:

```sh
cargo install --locked --path .
```

`check-release.sh` verifies the manifest/tag version relationship, builds the
clean Cargo source package, installs from that package in a temporary prefix, and
smoke-tests the installed command:

```sh
release/check-release.sh
release/check-release.sh v0.1.0
```

The optional tag must be exactly `vMAJOR.MINOR.PATCH`. The source package contains
only the Rust source, locked dependency graph, license, and user/design documents;
the lab and retained benchmark observations remain available in Git but are not
part of an installable crate.

## Published artifacts

A matching tag makes the GitHub Actions release workflow build natively on x86-64
and ARM64 Linux. It publishes:

- `tsunami-udp-VERSION-x86_64-unknown-linux-musl.tar.gz`
- `tsunami-udp-VERSION-aarch64-unknown-linux-musl.tar.gz`
- `tsunami-udp-VERSION.crate`
- `SHA256SUMS`

The musl archives are statically linked and each contains `tsunami-udp`,
`README.md`, and `LICENSE`. Verify an artifact before installing it:

```sh
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf tsunami-udp-VERSION-TARGET.tar.gz
sudo install -m 0755 tsunami-udp-VERSION-TARGET/tsunami-udp /usr/local/bin/
```

To install from the clean source package instead:

```sh
mkdir tsunami-source
tar -xzf tsunami-udp-VERSION.crate -C tsunami-source
cargo install --locked --path tsunami-source/tsunami-udp-VERSION
```

## Maintainer sequence

The repository currently has no public Git remote. Choose and configure that
remote before the first release; do not add a guessed repository URL to Cargo
metadata or documentation.

1. Ensure CI passes on the intended release commit.
2. Set the same stable version in `Cargo.toml` and regenerate `Cargo.lock`.
3. Run `release/check-release.sh vVERSION` from a clean checkout.
4. Create an annotated or signed `vVERSION` tag at that commit.
5. Push the commit and tag to the chosen GitHub remote.
6. Verify the tag-triggered workflow and its checksums before announcing it.

The workflow refuses a tag that does not exactly match the Cargo package version.
It uses GitHub's automatically supplied token only to create that repository's
release; it requires no project secret.
