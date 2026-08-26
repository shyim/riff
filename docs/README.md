# Riff documentation

The README gets Riff onto your machine. These guides explain how to use it
safely in real projects and how its Composer compatibility boundary works.

## Start here

1. [Getting started](getting-started.md) installs Riff, introduces the two
   executable names, and walks through a first project.
2. [Everyday usage](usage.md) covers dependency changes, scripts, inspection,
   dry runs, machine-readable output, and CI usage.
3. [Configuration](configuration.md) explains how project, global, environment,
   PHP, authentication, and cache settings interact.

Embedding Riff in another Rust application starts with
[Using Riff as a Rust library](library.md).

## Reference and advanced workflows

- [Command reference](commands.md) lists every top-level command and alias.
- [Package patching](patching.md) covers Riff-native patches and
  `cweagans/composer-patches` declarations.
- [Compatibility](compatibility.md) documents supported Composer behavior,
  native plugin adapters, and intentional limits.
- [Troubleshooting](troubleshooting.md) starts with the shortest diagnostic and
  recovery steps.

## Develop Riff

- [Contributing](../CONTRIBUTING.md) covers repository setup and change quality.
- [Testing](../TESTING.md) describes focused Rust, CLI, and Composer contract
  test layers.
- [Releasing](../RELEASING.md) documents version tags, binary packaging, and
  draft release recovery.

The CLI remains the exhaustive option reference:

```sh
riff --help
riff <command> --help
```
