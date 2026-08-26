# Everyday usage

Riff follows Composer's project model: requirements live in `composer.json`,
the resolved dependency graph lives in `composer.lock`, and installed packages
live under the configured vendor directory.

## Install a locked project

```sh
riff install
```

Useful installation modes include:

```sh
riff install --no-dev
riff install --prefer-dist
riff install --no-scripts --no-plugins
riff install --download-only
```

Use `--no-scripts` only when skipping project lifecycle scripts is safe. Use
`--no-plugins` to disable supported plugin integration for that invocation; it
does not make an unsupported required Composer plugin executable.

## Add, update, and remove dependencies

Add a runtime or development dependency:

```sh
riff require psr/log:^3
riff require --dev phpunit/phpunit:^12
```

Update the complete graph, one package, or a package with its dependencies:

```sh
riff update
riff update symfony/console
riff update symfony/console --with-dependencies
riff update symfony/console --with-all-dependencies
```

Remove a dependency or unused direct requirements:

```sh
riff remove vendor/package
riff remove --unused
```

For a clean reinstall without changing constraints, use:

```sh
riff reinstall vendor/package
riff reinstall --type library
```

## Preview mutations

`--dry-run` resolves the requested operation and prints the plan without
changing project files. It skips manifest and lock writes, vendor and patch
changes, autoload generation, lifecycle scripts, and the automatic audit.

```sh
riff require vendor/package:^2 --dry-run
riff update --dry-run
riff remove vendor/package --dry-run
riff bump --dry-run
riff dump-autoload --dry-run
riff config --dry-run optimize-autoloader true
riff patches-repatch --dry-run
```

Repository metadata may still be downloaded or refreshed in Riff's cache so
the preview is accurate. A dry run guarantees that the project is not mutated,
not that the invocation is offline.

## Generate autoload files

Installation normally generates autoload files. Regenerate them explicitly
after changing application code or autoload configuration:

```sh
riff dump-autoload
riff dump-autoload --optimize
riff dump-autoload --classmap-authoritative
riff dump-autoload --strict-psr
```

## Run project scripts and binaries

List and run scripts from `composer.json`:

```sh
riff run --list
riff run test
riff test
```

The last form works because an unknown top-level command matching a project
script is dispatched as that script. Arguments after the script name are
forwarded to it.

List or execute dependency binaries:

```sh
riff exec --list
riff exec phpunit -- --filter SomeTest
```

Project processes retain their own stdout and stderr. Riff's lifecycle
messages continue through its shared renderer.

## Inspect dependencies

```sh
riff show
riff show vendor/package
riff show --tree vendor/package
riff outdated --direct
riff why vendor/package
riff why-not vendor/package 2.0.0
riff licenses
riff suggests
riff fund
```

Use `--locked` on supported inspection commands to read from `composer.lock`
instead of installed packages.

## Validate and audit

```sh
riff validate --strict
riff audit --locked
riff check-platform-reqs --lock
riff status
```

`status` reports local modifications to installed packages. Use
`patches-doctor` for patch declarations, locks, and installed patch state.

## Human and machine output

Text output is the default. Global output flags can be combined with any
command:

```sh
riff --quiet install
riff --no-progress update
riff --no-ansi validate
riff --output json audit
```

`--output json` emits newline-delimited JSON events with a `level` and
`message`; it is a stream, not one enclosing JSON document. JSON and
non-terminal output automatically disable interactive progress.

Command-specific structured formats, such as `audit --format=json` or
`show --format=json`, describe the command result. The global `--output json`
format describes Riff's event stream. Choose the one that matches the
consumer.

## CI recommendations

A reproducible CI install usually starts from a committed `composer.lock`:

```sh
riff install --no-interaction --no-progress --no-ansi
riff validate --strict
riff check-platform-reqs --lock
riff audit --locked
```

Set `RIFF_CACHE_DIR` to a path managed by the CI cache mechanism. Cache the
directory between jobs, but treat `composer.lock`—not cached metadata—as the
source of dependency truth.

For commands and options not covered here, see the
[Command reference](commands.md) and `riff <command> --help`.
