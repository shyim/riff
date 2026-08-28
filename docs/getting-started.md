# Getting started

## Requirements

Building Riff requires Rust 1.98.0. PHP 7.2.5 or newer must be available for
commands that inspect the PHP platform, install dependencies, or run `@php`
scripts. Git is also recommended because some Composer packages use source
distributions.

Riff itself is a native executable. Static commands such as `validate`,
`config`, and `status` can operate without launching PHP unless a configured
project script invokes `@php`.

## Install with Homebrew

Homebrew core's `riff` formula belongs to an unrelated project. Use the fully
qualified Riff formula so Homebrew selects the Composer-compatible package
manager:

```sh
brew install shyim/tap/php-riff
```

It installs the command as `riff`, declares PHP as a runtime dependency, and
generates shell completions during installation.

## Install a release binary

Download the archive for your operating system and architecture from
[GitHub Releases](https://github.com/shyim/riff/releases). Every release
provides Linux GNU, Linux musl, macOS, and Windows builds for x86-64 and ARM64,
plus a `SHA256SUMS` manifest.

Extract the archive, verify that `riff --version` reports the downloaded
version, and move the executable to a directory on your `PATH`. Release
archives contain only `riff`; use the source installation below when you
also need the `composer` compatibility executable.

## Install from source

Clone the repository and install the CLI with Cargo:

```sh
git clone https://github.com/shyim/riff.git
cd riff
cargo install --path crates/riff --locked
```

The source package provides two executables with identical behavior:

- `riff` is the preferred name for direct use.
- `composer` is a compatibility shim for tools and scripts that hard-code the
  Composer executable name.

Confirm the executable you intend to use is the one on your `PATH`:

```sh
riff --version
command -v riff
command -v composer
```

If installing the `composer` shim would conflict with an existing Composer
binary, build instead and copy only Riff:

```sh
cargo build --release --locked
cp target/release/riff /a/directory/on/your/PATH/riff
```

Replace the destination with a directory you own; do not overwrite an existing
Composer installation unless that is intentional.

## Use an existing project

From a directory containing `composer.json`:

```sh
riff validate
riff install
riff check-platform-reqs
```

`install` uses `composer.lock` when it exists. Commit the resulting lock file
and generated patch locks, but do not commit `vendor/` unless your project
already does so.

Before replacing Composer in CI, read [Compatibility](compatibility.md), run the
project's full test suite, and verify any Composer plugins used by the project
have native Riff support.

## Create a project

Create a project from a package:

```sh
riff create-project vendor/skeleton my-project
cd my-project
```

Or initialize a manifest interactively in an empty directory:

```sh
mkdir my-project
cd my-project
riff init
riff require psr/log:^3
```

Pass `--no-interaction` to commands intended for unattended environments. Use
`-d <directory>` or `--working-dir <directory>` on commands that expose it when
you do not want to change the current shell directory.

## Configure PHP

Riff selects PHP in this order:

1. `--php <PATH>`
2. `RIFF_PHP`
3. `PHP_BINARY`
4. `php` from `PATH`

For example:

```sh
riff --php /opt/php/bin/php install
RIFF_PHP=/opt/php/bin/php riff show --platform
```

See [Configuration](configuration.md#php-and-platform-detection) for platform
overrides and caching details.

## Enable shell completion

Generate completions from the installed binary so suggestions stay in sync
with your Riff version:

```sh
# Bash
source <(riff completion bash)

# Zsh
source <(riff completion zsh)

# Fish
riff completion fish | source

# PowerShell
Invoke-Expression (& riff completion powershell)
```

Put the appropriate command or generated script in your shell startup file for
persistent completion. Local package, script, repository, and patch suggestions
are derived from project files and do not perform network requests.

## Next steps

- Follow the common workflows in [Everyday usage](usage.md).
- Review every available command in the [Command reference](commands.md).
- Learn Riff's first-class [Package patching](patching.md) workflow.
