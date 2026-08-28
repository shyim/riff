# Releasing Riff

Riff releases are created from version tags and contain prebuilt binaries for
Linux, macOS, and Windows. The release workflow builds a draft first and only
publishes it after every target and checksum succeeds.

## Prepare the version

1. Update `workspace.package.version` in `Cargo.toml`.
2. Run `cargo check --workspace` once without `--locked` so Cargo updates the
   Riff package versions recorded in `Cargo.lock`.
3. Review the lockfile diff and run the complete release gate:

   ```sh
   make check
   ```

4. Commit the version and lockfile changes, merge them to `main`, and confirm
   the normal CI workflow is green.

The release tag must be exactly `v` followed by the workspace version. For
example, workspace version `0.2.0` requires tag `v0.2.0`. SemVer prereleases
such as `0.2.0-rc.1` are supported and become GitHub prereleases.

## Test the packaging workflow

Before the first release, or after changing release infrastructure, manually
run the **Release** workflow with a ref and a harmless label such as `dryrun`.
The workflow performs the same quality gate and eight-target build, but creates
only a temporary `riff-dryrun-release-bundle` workflow artifact. It never
creates a tag or GitHub release.

The bundle must contain eight target archives and `SHA256SUMS`. Each archive
contains exactly one executable named `riff` or `riff.exe`.

## Create the release

Create the tag only after the version commit is on `main`:

```sh
git switch main
git pull --ff-only
git tag -a v0.2.0 -m "Riff v0.2.0"
git push origin v0.2.0
```

Replace `0.2.0` with the prepared workspace version. The workflow validates
the tag, reruns formatting, Clippy, and the workspace tests, then builds:

- Linux GNU and musl for x86-64 and ARM64;
- macOS for Intel and Apple Silicon; and
- Windows MSVC for x86-64 and ARM64.

It creates a draft GitHub release with generated notes, uploads the eight
archives and checksum manifest, and publishes the release only when all jobs
succeed. Stable releases become the latest release. Prerelease versions do not.

After a stable release is published, the **Publish Homebrew formula** workflow
downloads the four macOS and Linux GNU archives, computes their checksums, and
updates `Formula/php-riff.rb` in `shyim/homebrew-tap`. Prereleases are
intentionally excluded from the stable formula.

The workflow uses Octo STS to exchange GitHub's OIDC identity for a short-lived
token with **Contents: write** access to `shyim/homebrew-tap`; it does not use a
PAT or repository secret. Install the Octo STS GitHub App with access to the tap
and copy `packaging/homebrew/riff-homebrew.sts.yaml` to
`.github/chainguard/riff-homebrew.sts.yaml` in the tap. The policy binds access
to this repository's immutable numeric IDs, the Homebrew workflow path, and
either `main` or stable version tags.

To publish an existing stable release or retry a failed tap update, run the
workflow manually from `main` with its immutable version tag.

If an automation credential creates the tag but GitHub does not dispatch its
push workflow, run **Release** manually with `ref` set to the version tag,
`label` set to the same tag, and `release` enabled. The workflow applies the
same tag/version and `main` ancestry checks before it can publish a release.

## Recover a failed release

If a build or upload fails, the GitHub release remains a draft. Fix transient
infrastructure problems and rerun the failed workflow jobs. The workflow reuses
the existing draft and replaces assets with the same names. Do not publish the
draft manually unless all eight archives and `SHA256SUMS` are present.

If the source itself is wrong, delete the draft and tag, prepare a new version,
and create a new tag. Never move a tag after users may have downloaded its
artifacts.

## Verify a downloaded archive

Download the archive and `SHA256SUMS` from the same GitHub release, then run:

```sh
grep 'riff-v0.2.0-x86_64-unknown-linux-gnu.tar.gz' SHA256SUMS \
  | sha256sum --check
```

GitHub build-provenance attestations are emitted automatically for public
repositories. Private repositories can opt in when their GitHub plan supports
attestations by setting the repository variable
`ENABLE_PRIVATE_ATTESTATIONS=true`.
