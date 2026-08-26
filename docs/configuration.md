# Configuration

Riff reads Composer project and global configuration so an existing PHP
project does not need a second package-manager manifest. Riff-specific
runtime settings use the `RIFF_` environment variables documented below.

## Inspect configuration

List merged values and include their origins:

```sh
riff config --list
riff config --list --source
riff config vendor-dir
```

Write or remove project settings:

```sh
riff config optimize-autoloader true
riff config --unset optimize-autoloader
riff config --json preferred-install '{"*":"dist"}'
```

Preview a write without touching the selected file:

```sh
riff config --dry-run optimize-autoloader true
```

Use `--global` for Composer's global configuration, `--file <path>` for a
specific manifest/config file, and `--auth` with `--editor` to select auth data.
Run `riff config --help` for merge, append, editor, and absolute-path modes.

## Project, global, and alternate files

- Project configuration is read from `composer.json` in the working directory.
- Global configuration and authentication are read from Composer's home
  directory. Set `COMPOSER_HOME` to choose it explicitly.
- Set `COMPOSER` to select an alternate project manifest filename or path.
- Use `riff global <command> ...` to run a command with Composer's global home
  as its working directory.

The `repository` command provides safer structured mutations than manually
editing repository arrays:

```sh
riff repository list
riff repository add private composer https://packages.example.com
riff repository set-url private https://mirror.example.com
riff repository disable private
```

Use `--global`, `--file`, `--append`, `--before`, or `--after` when the target or
repository order matters.

## Authentication

Riff reads Composer-compatible authentication from global and project auth
files and from the `COMPOSER_AUTH` JSON environment variable. Precedence is
`COMPOSER_AUTH`, project `auth.json`, then global `auth.json`. Prefer an auth
file with appropriately restricted permissions for local development and a
secret environment variable in CI.

```sh
export COMPOSER_AUTH='{"http-basic":{"packages.example.com":{"username":"user","password":"token"}}}'
riff install
```

Never commit credentials to `composer.json`, examples, fixtures, or shell
history. If authentication fails, `riff config --list --source` can verify the
active configuration layer, but its output may include credential values.
Always redact that output before sharing it.

## PHP and platform detection

PHP selection has a fixed precedence:

1. the global `--php <PATH>` option;
2. `RIFF_PHP`;
3. `PHP_BINARY`;
4. `php` found on `PATH`.

PHP 7.2.5 or newer is required for platform-dependent operations. Static
commands such as `validate`, `config`, and `status` do not launch PHP unless a
configured project script explicitly invokes `@php`.

The Composer `config.platform` map can pin virtual platform packages. String
values replace or add a platform version; `false` disables an extension or
library package. Disabling the `php` package itself is rejected.

Riff caches platform snapshots and invalidates them when the PHP binary, ini
files, scanned ini directories, or relevant environment changes. Set
`RIFF_NO_PLATFORM_CACHE=1` when diagnosing a platform probe and you need a
fresh result.

## Riff's runtime cache

Riff stores package archives, repository metadata, audit responses, platform
snapshots, and temporary remote patch downloads in one dedicated cache root.
Set it explicitly with:

```sh
export RIFF_CACHE_DIR=/path/to/riff-cache
```

Without an override, Riff uses the operating system's application cache
directory for `riff`—typically `$XDG_CACHE_HOME/riff` or
`$HOME/.cache/riff` on Linux. If no platform cache directory can be resolved,
it falls back to `.riff/cache`.

Composer's `cache-dir` and `COMPOSER_CACHE_DIR` are parsed and displayed for
compatibility but do not redirect Riff's runtime cache. This prevents Riff
and Composer from mixing differently structured cache entries.

Manage the runtime cache with:

```sh
riff clear-cache
riff clear-cache --gc
```

## Useful environment variables

| Variable | Purpose |
| --- | --- |
| `RIFF_CACHE_DIR` | Override Riff's entire runtime cache root. |
| `RIFF_PHP` | Select PHP unless `--php` is supplied. |
| `RIFF_NO_PLATFORM_CACHE` | Force a fresh PHP platform probe when set. |
| `COMPOSER` | Select an alternate project manifest. |
| `COMPOSER_HOME` | Select Composer-compatible global config and auth storage. |
| `COMPOSER_AUTH` | Provide Composer-compatible authentication as JSON. |
| `COMPOSER_PROCESS_TIMEOUT` | Set the timeout used for project processes. |
| `COMPOSER_SKIP_SCRIPTS` | Provide a comma-separated set of project scripts to skip. |
| `COMPOSER_ROOT_VERSION` | Override root package version detection. |

Composer-compatible commands may honor additional `COMPOSER_` variables. Use
command help and `riff config --list --source` to confirm behavior rather than
assuming every variable supported by a particular Composer release is
implemented.
