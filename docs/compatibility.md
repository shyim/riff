# Composer compatibility

Riff is designed around Composer's project model and common command-line
workflow, not around embedding Composer's PHP runtime. Compatibility is
therefore broad for declarative package-management behavior and deliberately
limited for arbitrary executable plugins.

## Compatible project data

Riff reads and writes the files expected by Composer projects:

- `composer.json` requirements, repositories, scripts, autoload settings,
  package metadata, stability, and supported configuration;
- `composer.lock` dependency graphs and metadata;
- Composer-compatible global configuration and authentication;
- installed-package metadata and generated autoload files below `vendor/`;
- standard repository metadata and package archives.

The `composer` binary built from this repository is an executable-name shim for
Riff. It does not invoke a separately installed PHP Composer binary.

Compatibility is continuously checked with focused Rust tests, copied Composer
fixtures, direct PHP contract ports, and differential scripts. See
[Testing](../TESTING.md) for the current testing model; do not infer support for
an untested plugin or edge case only because a command name exists.

## PHP use

Riff does not embed PHP. It launches the selected PHP executable when it needs
runtime platform facts or when a project script uses `@php`. PHP 7.2.5 or newer
is required for those operations.

This separation means dependency solving and most repository, validation,
configuration, and inspection work happens in native Rust. It also means Riff
cannot load arbitrary Composer classes to extend its own process.

Rust embedding applications can provide a local or remote target platform
without launching PHP. See [Using Riff as a Rust library](library.md).

## Composer plugins

Riff provides native adapters for these packages:

- `bamarni/composer-bin-plugin`
- `cweagans/composer-patches` 1.x and 2.x
- `php-http/discovery`
- `phpstan/extension-installer`
- `symfony/flex` 2.x
- `symfony/runtime`

The adapter implements the relevant behavior in Rust; the package's PHP plugin
code is not executed. The Composer Patches declarative formats are available
even when `cweagans/composer-patches` is not installed.

Other enabled Composer plugins fail with an actionable error before package
installation. This is intentional: silently ignoring a plugin could produce an
incorrect vendor tree or generated configuration. Projects that depend on an
unsupported plugin should continue using Composer until a native adapter or
first-class Riff feature covers the required behavior.

`--no-plugins` disables plugin integrations for an invocation. It is useful
only when the project is valid without their behavior; it does not make an
unsupported plugin compatible.

### Symfony Flex

The native Flex 2.x adapter supports official, contrib, custom index, and
legacy recipe endpoints; aliases and special Symfony version names; Symfony
split-package filtering and pack unpacking; recipe install/uninstall and
`symfony.lock`; all Flex configurator types; UX package/importmap
synchronization; object-valued `auto-scripts`; and `dump-env`.

The Composer command names and their `symfony:` aliases are available as
`recipes`, `recipes:install`, `recipes:update`, and `dump-env`.
`recipes:update` requires Git, requires a clean index, and applies updates as a
three-way merge so application edits are retained. Conflicts are left with
standard conflict markers for manual resolution. Community recipes remain
opt-in through `extra.symfony.allow-contrib`, `SYMFONY_ALLOW_CONTRIB`, or the
install command's `--yes` flag. Docker recipe sections are enabled through
`extra.symfony.docker` or `SYMFONY_DOCKER` in non-interactive workflows.

Flex 1.x is rejected explicitly; use Flex 2.x with Riff.

## Scripts

Riff executes supported Composer lifecycle and custom project scripts as
external processes. `@php` uses the PHP executable selected by `--php`,
`RIFF_PHP`, `PHP_BINARY`, or `PATH`. Script stdout, stderr, arguments, and exit
status remain part of the project's command contract.

Use `--no-scripts` for controlled workflows that intentionally skip scripts,
and test the resulting install before relying on it.

## Patches

Native patching and the supported Composer Patches formats are documented in
[Package patching](patching.md). Riff intentionally accepts UTF-8 unified
text diffs rather than delegating to an external Git or `patch` executable.

## Adoption checklist

Before replacing Composer for a project:

1. Run `riff validate --strict`.
2. Inventory packages of type `composer-plugin` and compare them with the
   adapter list above.
3. Run `riff install` from a clean checkout using the committed lock file.
4. Run `riff check-platform-reqs --lock` and `riff audit --locked`.
5. Run the project's full build and test suite.
6. Compare generated artifacts or deployment output with the existing Composer
   workflow.

Report a compatibility gap with a minimal `composer.json`, lock information if
relevant, the Riff version, platform details, exact command, and complete
error output.
