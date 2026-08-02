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
release/check-release.sh v0.2.0-alpha.1
```

The optional tag must be exactly `vSEMVER`, including any prerelease suffix. The
source package contains only the Rust source, locked dependency graph, license, and user/design documents;
the lab and retained benchmark observations remain available in Git but are not
part of an installable crate.

## Published artifacts

A matching tag makes the GitHub Actions release workflow build natively on x86-64
and ARM64 Linux. It publishes:

- `packet-tide-VERSION-x86_64-unknown-linux-musl.tar.gz`
- `packet-tide-VERSION-aarch64-unknown-linux-musl.tar.gz`
- `packet-tide-VERSION.crate`
- `SHA256SUMS`

The musl archives are statically linked and each contains `packet-tide`,
`README.md`, and `LICENSE`. The historical v0.1.0 release predates the rename and
keeps its original `tsunami-udp` names. Verify a current artifact before
installing it:

```sh
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf packet-tide-VERSION-TARGET.tar.gz
sudo install -m 0755 packet-tide-VERSION-TARGET/packet-tide /usr/local/bin/
```

To install from the clean source package instead:

```sh
mkdir packet-tide-source
tar -xzf packet-tide-VERSION.crate -C packet-tide-source
cargo install --locked --path packet-tide-source/packet-tide-VERSION
```

## Maintainer sequence

The public repository is
[matthiasgoergens/packet-tide](https://github.com/matthiasgoergens/packet-tide).

1. Ensure CI passes on the intended release commit.
2. Run the independent-host matrix and require
   `lab/two-host/evaluate-release.py RESULT_DIR` to pass.
3. Set the same stable version in `Cargo.toml` and regenerate `Cargo.lock`.
4. Run `release/check-release.sh vVERSION` from a clean checkout.
5. Create an annotated or signed `vVERSION` tag at that commit.
6. Push the commit and tag to the chosen GitHub remote.
7. Verify the tag-triggered workflow and its checksums before announcing it.

The workflow refuses a tag that does not exactly match the Cargo package version.
It uses GitHub's automatically supplied token only to create that repository's
release; it requires no project secret.
