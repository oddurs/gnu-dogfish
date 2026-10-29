# Verifying a GNU/Dogfish release

Every release on the [Releases page](https://github.com/oddurs/gnu-dogfish/releases)
has these files:

| File | What it is |
| --- | --- |
| `gnu-dogfish-linux-x86_64.tar.gz`, `gnu-dogfish-macos-arm64.tar.gz` | The `dogfish` binary with README, licenses and changelog, and `share/`: manual pages and bash, zsh and fish completions. |
| `<archive>.sha256` | The archive's SHA-256, in `sha256sum` format. |
| `gnu-dogfish-linux-x86_64.cdx.json`, `gnu-dogfish-macos-arm64.cdx.json` | A [CycloneDX](https://cyclonedx.org/) SBOM per archive: every crate compiled into (or used to build) that binary, with version, license and package URL. |
| `SHA256SUMS` | SHA-256 of every archive and SBOM. |

The release workflow (`.github/workflows/release.yml`) also publishes
[GitHub artifact attestations](https://docs.github.com/en/actions/security-for-github-actions/using-artifact-attestations)
for these files. An attestation is a [Sigstore](https://www.sigstore.dev/)-signed
statement, recorded in a public transparency log, that says which repository,
workflow, commit and run built a file with a given digest
([SLSA build provenance](https://slsa.dev/spec/v1.0/provenance)). Each archive
also carries an attestation of its SBOM.

## 1. Check the checksum

Proves the download is complete and matches what the release lists.

```bash
sha256sum -c --ignore-missing SHA256SUMS          # Linux
shasum -a 256 -c gnu-dogfish-macos-arm64.tar.gz.sha256   # macOS
```

## 2. Check the provenance

Proves the archive was built by this repository's release workflow, and not
uploaded by hand or built elsewhere. Needs the [GitHub CLI](https://cli.github.com/)
(`gh` 2.49 or later):

```bash
gh attestation verify gnu-dogfish-linux-x86_64.tar.gz --repo oddurs/gnu-dogfish
```

To also require the release workflow specifically (rather than any workflow in
the repository):

```bash
gh attestation verify gnu-dogfish-linux-x86_64.tar.gz --repo oddurs/gnu-dogfish \
  --signer-workflow oddurs/gnu-dogfish/.github/workflows/release.yml
```

The output names the commit the archive was built from. The same command works
for the SBOMs and for `SHA256SUMS`.

## 3. Check the SBOM

The SBOM attestation binds an archive to its SBOM:

```bash
gh attestation verify gnu-dogfish-linux-x86_64.tar.gz --repo oddurs/gnu-dogfish \
  --predicate-type https://cyclonedx.org/bom
```

Feed the `.cdx.json` file to any CycloneDX tool (for example
[OSV-Scanner](https://google.github.io/osv-scanner/) or Dependency-Track) to
check the dependencies for known vulnerabilities. In this repository CI does
the same on every pull request and weekly with
[cargo-deny](https://github.com/EmbarkStudios/cargo-deny) (`deny.toml`).

## 4. Rebuild it yourself

Release builds are reproducible: the same source, toolchain and build
environment give a bit-identical `dogfish` binary, whatever directory it is
built in and whenever. CI checks this on every push to `main`
(`cargo xtask repro`: two builds from two directories must match).

```bash
git clone https://github.com/oddurs/gnu-dogfish.git
cd gnu-dogfish
git checkout v0.2.0                      # the release tag
grep RELEASE_TOOLCHAIN: .github/workflows/release.yml   # e.g. 1.99.1
rustup toolchain install 1.99.1 --target x86_64-unknown-linux-gnu
cargo +1.99.1 xtask dist --target x86_64-unknown-linux-gnu
```

`cargo xtask dist` writes `target/dist/out/<archive>` and its `.sha256`, and
leaves the binary in `target/dist/<target>/release/dogfish`. Compare the
binary with the one in the release archive:

```bash
mkdir release && tar -xzf gnu-dogfish-linux-x86_64.tar.gz -C release
sha256sum release/dogfish target/dist/x86_64-unknown-linux-gnu/release/dogfish
```

What has to match:

- **Toolchain**: the `RELEASE_TOOLCHAIN` of that tag.
- **Build environment**: the C compiler that builds `ring`'s C and assembly
  code, and the C runtime objects the linker adds, come from the system.
  Release binaries are built on GitHub's
  `ubuntu-latest` (Ubuntu 24.04) and `macos-latest` runners; for a
  bit-identical Linux binary, build in the same distribution (for example a
  `ubuntu:24.04` container with `build-essential`).
- **Nothing else**: the build maps the source and `$CARGO_HOME` paths to
  fixed names (`--remap-path-prefix`), strips symbols, and takes its only
  timestamp from the commit (`SOURCE_DATE_EPOCH`).

The archive itself is reproducible too (sorted entries, owner 0:0, fixed
modes, the commit time as mtime, `gzip -n`), but its compressed bytes depend
on the `gzip` version as well, so compare the binary first.
