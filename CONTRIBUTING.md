# Contributing to Riff

Thank you for helping make Riff more useful and more compatible. Changes are
most effective when they preserve the fast local feedback loops while adding a
focused contract for new behavior.

## Before you start

- Search existing issues and pull requests before beginning a large change.
- Keep changes scoped. Separate broad refactors from behavior or compatibility
  changes when practical.
- For a significant compatibility decision, explain the Composer behavior,
  Riff's intended behavior, and the test evidence in the pull request.
- Never add real repository credentials, auth files, cache contents, `vendor/`,
  or build output.

## Development requirements

- Rust 1.98.0 with `rustfmt` and Clippy
- PHP available on `PATH`; PHP 8.5 matches the CI test environment
- GNU Make for the documented convenience targets
- Git

The repository includes `rust-toolchain.toml` and a project-local `mise.toml`.
If you use [mise](https://mise.jdx.dev/), install the pinned Rust and
`actionlint` tools with:

```sh
mise install
mise ls --current
```

Mise does not currently install PHP for this project. Install PHP separately
and verify it with `php --version`.

## Set up the repository

```sh
git clone https://github.com/shyim/riff.git
cd riff
mise install                 # optional when the pinned tools already exist
cargo build --workspace
./target/debug/riff --help
./target/debug/composer --help
```

Run the fast baseline tests before editing:

```sh
cargo test-core
cargo test-cli
```

## Repository layout

| Path | Responsibility |
| --- | --- |
| `crates/riff/` | CLI parsing, rendering, commands, and CLI integration tests |
| `crates/riff-core/` | Solver, repositories, installation, autoloading, patching, configuration, and core tests |
| `crates/riff-semver/` | Composer-compatible version and constraint behavior |
| `crates/riff-spdx/` | SPDX expression support |
| `scripts/` | Composer inventory and differential compatibility tooling |
| `docs/` | User-facing guides and command documentation |
| `profiles/` | Performance profiling notes and fixtures |

`TESTING.md` remains at the root because it is a working reference for
repository development rather than an end-user guide.

## Use the smallest test loop

Choose the layer that owns the change:

```sh
# Core behavior
cargo test-core optional_filter

# CLI behavior across command test targets
cargo test-cli optional_filter

# One CLI integration target and test
cargo test -p riff --test update_command composer_update_patch_only

# Ported Composer fixtures
cargo test-composer optional_filter
```

The same common layers are exposed as `mise run test:core`,
`mise run test:cli`, and `mise run test:composer`, or through the corresponding
Make targets. [TESTING.md](TESTING.md) documents selection, inventories,
fixtures, property tests, and direct PHP contract mappings in detail.

Before handing off a change, run the complete local gate:

```sh
make check
```

It checks formatting, runs Clippy with warnings denied, runs every workspace
test target with the locked dependency graph, and builds the release workspace.

## Style and implementation guidance

- Run `cargo fmt --all` before requesting review; `make fmt` checks but does not
  rewrite files.
- Keep Clippy clean across all targets and features.
- Prefer explicit errors that name the invalid file, package, setting, or
  recovery action.
- Add tests at the lowest stable contract boundary. Add CLI integration tests
  when argument parsing, filesystem mutation, output, or exit status matters.
- Keep generated and property-based cases deterministic and reproducible.
- Avoid case-specific compatibility hacks. Extend a shared parser, harness, or
  abstraction when upstream fixtures expose a general gap.
- Update user documentation when adding commands, flags, configuration,
  environment variables, compatibility, or recovery behavior.

## Composer compatibility work

For local parity work, place an upstream Composer checkout at the Makefile's
default location:

```sh
git clone https://github.com/composer/composer.git shopware/composer
make composer-test-inventories
```

Override `COMPOSER_SRC_DIR`, `PHP_BIN`, or `RIFF_BIN` when using another
layout. The CI inventory job uses a pinned Composer commit so upstream changes
cannot silently alter expected coverage.

Useful targets include:

```sh
make composer-test-check
make composer-test-pending
make composer-php-test-pending
make composer-functional-test-pending
make parity
```

Follow the Composer compatibility guidance in [TESTING.md](TESTING.md) before
copying or classifying an upstream test. Preserve upstream fixtures unchanged
where possible, record direct ports in the appropriate registry, and classify
only genuinely PHP-runtime-specific behavior as non-portable.

## Documentation changes

Keep the root README short and task-oriented. Put durable explanations in
`docs/`, link related guides in both directions, and copy command names and
flags from the built CLI rather than memory.

When command behavior changes:

1. update the command's built-in help;
2. update the relevant task guide;
3. update `docs/commands.md` if the top-level surface or alias changed; and
4. verify all relative Markdown links.

## Continuous integration

Pull requests run these required job groups:

- workflow linting, Rust formatting, and Clippy;
- workspace tests on Linux, macOS, and Windows with PHP 8.5;
- Composer compatibility inventories against the pinned upstream snapshot; and
- a locked release build.

The final aggregate job fails when any required group fails. Reproduce the
specific failed command locally before rerunning CI where possible.

## Pull request checklist

- [ ] The change has a focused regression or contract test.
- [ ] `make check` passes locally, or the pull request explains what could not
      be run and why.
- [ ] Composer inventories or differential suites were run when compatibility
      behavior changed.
- [ ] User-facing command, configuration, and compatibility docs are current.
- [ ] No secrets, caches, dependency installs, reference checkouts, or build
      artifacts are included.
- [ ] The pull request describes observable behavior and noteworthy tradeoffs,
      not only implementation details.
