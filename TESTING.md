# Testing Riff

Riff keeps fast feedback loops separate from the full workspace gate. Run the
smallest layer that owns the behavior while developing, then run the complete
gate before committing.

The same layers are available through mise as `mise run test`, `mise run
test:core`, `mise run test:cli`, and `mise run test:composer`. This keeps CI and
local toolchain execution on the project-pinned versions.

| Layer | Command | Intended scope |
| --- | --- | --- |
| Core unit tests | `cargo test-core [filter]` | Solver, repositories, downloaders, lock and package logic |
| CLI crate tests | `cargo test-cli [filter]` | CLI unit and integration contracts across all command-specific targets |
| Composer compatibility fixtures | `cargo test-composer [filter]` | Ported Composer resolver and lock behavior |
| Full workspace | `cargo test --workspace --all-targets` | Pre-commit regression gate |

The equivalent Make targets are `make test-core`, `make test-cli`, and
`make test-composer`. `make test` runs the full workspace gate, while `make
check` additionally runs formatting, Clippy with warnings denied, and a release
build. Run one Composer case with:

```sh
cargo test-composer update_no_install
# or
make test-composer-case CASE=update_no_install
# include an ignored, not-yet-ported fixture
make test-composer-case CASE=update_installed_reference PENDING=1
```

For an ordinary CLI integration target, avoid building unrelated command test
binaries by naming the target and case directly:

```sh
cargo test -p riff --test update_command composer_update_patch_only
```

`cargo test-cli <filter>` searches every CLI test target and is useful when a
behavior may cross command boundaries. The explicit `--test <target>` form is
the fastest edit-test loop when ownership is already known.

List available compatibility cases with:

```sh
cargo test-composer -- --list
```

List only upstream fixtures that are still pending review with
`make composer-test-pending`. The inventory command also rejects duplicate or
stale `ported.txt` entries, so the passing count cannot silently drift.
Run `make composer-test-inventories` to validate the installer, direct PHP, and
functional registries together without executing their Rust tests.
`make composer-test-check` is the stricter CI gate: it also requires the pinned
Composer snapshot to match the copied installer fixtures and every inventory to
have zero pending contracts.

Parser and lockfile invariants use property tests in addition to copied Composer
examples. Generated cases should assert round trips or algebraic invariants and
stay deterministic so failures can be reproduced from the reported seed.

Composer's PHP tests are tracked at method granularity so direct Rust ports can
be run without the rest of the suite:

```sh
make composer-php-test-inventory
make composer-php-test-pending
make composer-php-test-delegated
make composer-php-test-case CASE=TransactionTest
make composer-php-test-group GROUP=InstalledVersionsTest.php
```

The mapping registry is
`crates/riff/tests/fixtures/composer/php-ported.tsv`. Each row names the exact
upstream method, Cargo package, Rust test filter, and local source file. The
inventory validates both ends against the checked-out Composer source and the
Riff tree, preventing stale or duplicate port claims. The group target runs
all mapped contracts under one upstream file or symbol prefix without invoking
the rest of the workspace.

Reviewed PHP-only contracts are recorded separately in
`php-non-portable-files.tsv`. Its first column accepts either a whole upstream
test file or one exact `File.php::testMethod` symbol, so mixed files can be
classified without hiding methods that still need a Riff contract review.

Methods whose PHP wrapper only dispatches the installer fixture corpus are
recorded in `php-delegated.tsv`. The inventory verifies that every fixture in
the delegated upstream suite exists locally and is enabled in `ported.txt`, so
delegation cannot mask an incomplete fixture port. Use
`make composer-php-test-delegated` to review these decisions.

Composer's smaller end-to-end fixture family has the same selective workflow:

```sh
make composer-functional-test-inventory
make composer-functional-test-pending
make composer-functional-test-case CASE=create-project-command.test
```

Mappings live in `functional-ported.tsv`; PHP-plugin-only cases are documented
in `functional-non-portable.tsv`. Both portable create-project fixtures are
mapped, and all three plugin-lifecycle fixtures are explicitly reviewed, so this
upstream family currently has no pending cases. Missing Riff commands and
incomplete end-to-end behavior remain pending rather than being labeled
non-portable when future fixtures are added.

## Composer fixture harness

Compatibility cases live under
`crates/riff/tests/fixtures/composer/` and retain Composer's sectioned `.test`
format. The initial fixtures are copied from Composer's MIT-licensed test suite.
Keep fixture contents unchanged when possible so upstream changes remain easy to
compare.

The harness currently supports these sections:

- `TEST`, `COMPOSER`, `RUN`, and `EXPECT`
- `LOCK` and `INSTALLED` for initial state
- `EXPECT-LOCK`, `EXPECT-INSTALLED`, and `EXPECT-EXIT-CODE`
- `EXPECT-OUTPUT` and `EXPECT-OUTPUT-OPTIMIZED` are compared semantically:
  package/platform names and diagnostic categories must agree while formatting
  and Composer-specific boilerplate may differ

Each case runs in a fresh temporary project with isolated Composer home and
Riff cache directories. Package repository entries without transport metadata
receive a dummy path transport because Riff validates installability earlier
than Composer's mocked installer tests. A no-lock `install` fixture resolves via
`update --no-install`; an install with a lock uses Riff's real install planner
in dry-run mode. Explicit dry runs use a second isolated lock projection for
transaction assertions without allowing the tested command to mutate state.

All 209 upstream installer fixtures are copied and currently pass. Future copied
fixtures remain ignored until added to `ported.txt`. Run
`make composer-test-inventory` for current counts.

To finish porting another Composer installer fixture:

1. Run the ignored generated case with `cargo test-composer <case-name> --
   --ignored --nocapture`.
2. Fix general Riff or harness behavior until the unchanged fixture passes.
3. Add its relative path to `fixtures/composer/ported.txt`.
4. If the harness reports an unsupported section or operation, extend the shared
   harness instead of adding case-specific setup.

The harness compares logical install/update/remove operations, selected lock
packages and requested metadata subsets. This keeps tests stable across harmless
output and extra lock metadata differences.
