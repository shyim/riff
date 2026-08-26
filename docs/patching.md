# Package patching

Package patching is a first-class Riff workflow. The Rust patch engine can
author and apply native Riff patches and can consume declarative
`cweagans/composer-patches` 1.x and 2.x configuration without installing or
executing that PHP plugin.

## Author a patch

Start from an installed dependency:

```sh
riff patch vendor/package
```

Riff prints the path of a writable `user` directory next to an immutable
source tree. Edit files only in the writable directory, test the change, then
commit the patch using the exact printed path:

```sh
riff patch-commit /path/to/the/printed/user
```

`patch-commit`:

1. creates a unified diff below `patches/` by default;
2. records the exact installed package version in `composer.json`;
3. refreshes `riff-patches.lock.json`;
4. reinstalls pristine package contents; and
5. applies the complete current patch set.

Choose a different project-relative output directory with `--patches-dir`.
Preview either operation with `--dry-run`.

Commit the manifest, generated `.patch` file, and patch lock together.

## Native declaration format

Native patches live under `extra.riff.patched-dependencies`. Selectors include
the exact package version so a dependency upgrade cannot silently apply a patch
to different source:

```json
{
  "extra": {
    "riff": {
      "patched-dependencies": {
        "vendor/package@1.2.3": "patches/vendor+package@1.2.3.patch"
      }
    }
  }
}
```

Use the authoring commands instead of editing this object manually whenever
possible.

## Maintain native patches

```sh
# Validate declarations, files, lock data, and installed state
riff patches-doctor

# Reinstall one or every patched package from pristine contents
riff patches-repatch vendor/package
riff patches-repatch

# Regenerate patch locks after intentionally editing declarations or files
riff patches-relock

# Remove one native declaration or all native declarations
riff patch-remove vendor/package@1.2.3
riff patch-remove
```

All mutating patch maintenance commands support `--dry-run`. Aliases are
available for `patches-doctor` (`pd`), `patches-relock` (`prl`), and
`patches-repatch` (`prp`).

A normal `install` or `update` compares patch fingerprints and cleanly
reinstalls a package whenever its patch set changes. Applied fingerprints are
stored in `vendor/composer/riff-patches.json`.

## Composer Patches configuration

Existing compact declarations are supported:

```json
{
  "extra": {
    "patches": {
      "vendor/package": {
        "Describe the fix": "patches/fix-package.patch"
      }
    }
  }
}
```

Riff also supports Composer Patches 2.x expanded definitions, SHA-256
checksums, depth configuration, dependency-provided definitions, ignored
dependency patches, and external files selected by
`extra.composer-patches.patches-file`. The standard resolver, downloader, and
patcher disable lists are understood for the built-in implementations; custom
PHP implementation classes cannot execute in Riff.

For 2.x declarations, `patches.lock.json` is authoritative until you explicitly
run:

```sh
riff patches-relock
```

Legacy 1.x compact root, dependency, `patches-file`, `patchLevel`, failure, and
reporting configuration is also recognized. This compatibility remains active
with `--no-plugins` and does not require the plugin package to be installed.

## Remote and untrusted patches

Remote patch bodies are downloaded through Riff's network layer and cached
under `RIFF_CACHE_DIR`. Checksums in expanded definitions are verified when
provided. For reproducible builds, prefer project-local patch files or pin
remote content with SHA-256.

Patch paths cannot escape the project or target package, and package symlinks
are rejected. Treat every patch as source code: review it and commit or checksum
it before applying it in production.

## Intentional format limits

The Rust engine supports UTF-8 unified text diffs. It rejects binary patches,
renames, copies, symlink changes, executable-mode changes, and changes that only
create or remove an empty file. Convert unsupported changes to an explicit
source distribution or another reviewed project-level mechanism.

When patching fails, run `riff patches-doctor` first. The
[Troubleshooting guide](troubleshooting.md#patch-state-is-invalid) covers the
next recovery steps.
