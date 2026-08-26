# Using Riff as a Rust library

Riff separates package-management behavior from PHP platform discovery.
Library callers provide platform facts explicitly; only the standalone
`riff` and `composer` binaries inspect the environment and probe PHP.

## Supply platform information

`PlatformSnapshot` represents PHP runtime facts. Its fields are serializable,
so an embedding application can construct them directly, load them from its own
inventory, or obtain them from a remote build target:

```rust
use std::collections::BTreeMap;
use riff_core::{Platform, PlatformSnapshot};

let snapshot = PlatformSnapshot {
    php_version: "8.3.12".to_owned(),
    php_version_id: 80312,
    int_size: 8,
    zts: false,
    debug: false,
    ipv6: true,
    extensions: BTreeMap::from([
        ("json".to_owned(), "8.3.12".to_owned()),
        ("openssl".to_owned(), "8.3.12".to_owned()),
    ]),
    libraries: BTreeMap::from([
        ("openssl".to_owned(), "3.2.2".to_owned()),
    ]),
};

let platform = Platform::from_snapshot(snapshot)
    .with_package("ext-company-runtime", "1.4.0");
```

Riff derives PHP capability packages such as `php-64bit`, `php-ipv6`, and
`ext-*` from the snapshot. Extra packages override derived facts. The
project's `config.platform` values remain the final override layer.

Riff supplies overridable versions for `composer`, `composer-runtime-api`,
and `composer-plugin-api` because those describe Riff's own compatibility.

## Construct the core package manager

Every `RiffBuilder` must receive an explicit platform:

```rust
use std::path::PathBuf;
use riff_core::config::Config;
use riff_core::{Platform, Riff, RiffManifest};

fn build(
    directory: PathBuf,
    config: Config,
    manifest: RiffManifest,
    platform: Platform,
) -> anyhow::Result<Riff> {
    Riff::builder(directory)
        .with_config(config)
        .with_manifest(manifest)
        .with_platform(platform)
        .build()
}
```

Omitting `with_platform` is an error instead of silently resolving without PHP
or extensions. For code that deliberately does not perform platform-dependent
resolution, pass `Platform::empty()`. An empty platform still exposes Riff's
Composer API capability packages.

`Platform::from_packages` accepts an existing list of virtual packages when a
typed PHP snapshot is not available. The older
`RiffBuilder::with_platform_packages` method remains as a deprecated
compatibility adapter.

## Execute CLI-like commands

The `riff` crate exposes an async runner that accepts arguments without the
executable name:

```rust
use std::path::PathBuf;
use riff::{run_with_args, CommandContext};
use riff_core::{Platform, RuntimeContext};

async fn show_target_platform(platform: Platform) -> anyhow::Result<i32> {
    let runtime = RuntimeContext::new(
        PathBuf::from("/opt/php/bin/php"),
        PathBuf::from("riff"),
    );
    let context = CommandContext::new(runtime, platform);

    run_with_args(
        ["show", "--platform", "--working-dir", "/srv/project"],
        context,
    )
    .await
}
```

`run_with_args` does not:

- inspect `RIFF_PHP`, `PHP_BINARY`, or `PATH` to discover PHP;
- execute PHP to detect versions or extensions;
- enforce the standalone detector's PHP 7.2.5 minimum;
- create a Tokio runtime; or
- initialize a logger.

The embedding application owns the async runtime and logging setup. A supplied
`--php <path>` argument changes the executable used by project scripts but
does not replace or refresh the supplied platform facts.

## PHP scripts remain explicit execution

Detaching platform detection does not disable project scripts. Commands that
dispatch an `@php` script still execute
`CommandContext::runtime().php_binary`. Use `--no-scripts` where supported or
provide a runtime appropriate for the host application.

Command output continues through Riff's process-wide renderer. Do not execute
concurrent embedded commands with different output settings.
