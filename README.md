# composer-rs

`composer-rs` is a standalone Composer-compatible package manager. Dependency
resolution, repository access, installs, autoload generation, validation, and
inspection run in Rust. A small PHP script is executed only when runtime facts
or an `@php` project script are needed.

The project intentionally targets the common package-management workflow. It
does not embed PHP and does not attempt to execute arbitrary PHP Composer
plugins. Native adapters are included for `bamarni/composer-bin-plugin`,
`phpstan/extension-installer`, and `symfony/runtime`; other enabled plugins fail
with an actionable error before package installation.

## Development

Enter the pinned Nix development environment and build the two binary names:

```bash
nix develop path:.
cargo build --workspace
./target/debug/composer-rs --help
./target/debug/composer --help
```

The flake supports `x86_64-linux`, `aarch64-linux`, and `aarch64-darwin`. It
provides Rust, PHP CLI, Git, archive tools, `jq`, and native build dependencies.
The checked-in `flake.lock` pins nixpkgs.

Useful gates:

```bash
make fmt
make test
make release
make flake-check
make parity
```

Parity tests use the Composer reference at `/workspace/composer` by default.
Override `COMPOSER_SRC_DIR`, `PHP_BIN`, or `COMPOSER_RS_BIN` for another layout.

## PHP Selection

The PHP executable is selected in this order:

1. `--php <PATH>`
2. `COMPOSER_RS_PHP`
3. `PHP_BINARY`
4. `php` from `PATH`

PHP 7.2.5 or newer is required for platform-dependent operations. Commands
such as `validate`, `config`, and `status` do not launch PHP unless a configured
project script explicitly uses `@php`.

```bash
composer-rs --php /opt/php/bin/php install
COMPOSER_RS_PHP=/opt/php/bin/php composer-rs check-platform-reqs
composer-rs show --platform
```

`config.platform` string values replace or add virtual packages, while `false`
disables an extension or library package. Disabling `php` itself is rejected.
Platform snapshots are cached under `$XDG_CACHE_HOME/composer-rs` (or
`$HOME/.cache/composer-rs`) and invalidated when PHP, its ini files, scan
directories, or relevant environment variables change. Set
`COMPOSER_RS_NO_PLATFORM_CACHE=1` to force a fresh probe.

## Commands

The supported top-level commands are `install`, `update`, `require` (`add`),
`remove`, `dump-autoload`, `run` (`run-script`), `show` (`info`), `why`
(`depends`), `why-not` (`prohibits`), `outdated`, `audit`, `validate`, `config`,
`status`, `check-platform-reqs`, and `completion`.
