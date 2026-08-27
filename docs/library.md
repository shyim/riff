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
use riff_core::{Output, Platform, Riff, RiffManifest};

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
        // Optional: the default discards Riff-generated output.
        .with_output(Output::silent())
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

## Receive structured output

Library use is silent by default. To observe messages, implement the
thread-safe `OutputSink` interface and attach it to either `RiffBuilder` or
`CommandContext`:

```rust
use std::sync::{Arc, Mutex};
use riff_core::{Output, OutputEvent, OutputSink};

#[derive(Default)]
struct Events(Mutex<Vec<OutputEvent>>);

impl OutputSink for Events {
    fn emit(&self, event: OutputEvent) {
        self.0.lock().unwrap().push(event);
    }
}

let events = Arc::new(Events::default());
let output = Output::from_sink(events.clone());

let context = CommandContext::new(runtime, platform)
    .with_output(output.clone());
let riff = Riff::builder(directory)
    .with_config(config)
    .with_manifest(manifest)
    .with_platform(Platform::empty())
    .with_output(output)
    .build()?;
```

Events are serializable and carry a severity, stdout/stderr intent, plain text
without ANSI escape sequences, and whether the message ends a line. A sink can
be called from installation worker threads and therefore must be `Send + Sync`.
Interactive progress bars belong only to the standalone process renderer and
are never sent to custom sinks.

## PHP scripts remain explicit execution

Detaching platform detection does not disable project scripts. Commands that
dispatch an `@php` script still execute
`CommandContext::runtime().php_binary`. Use `--no-scripts` where supported or
provide a runtime appropriate for the host application.

Child processes continue to inherit stdin, stdout, and stderr. This preserves
normal script behavior; `OutputSink` covers messages generated by Riff itself.
