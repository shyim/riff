# Troubleshooting

Start with Riff's built-in checks:

```sh
riff diagnose
```

The command checks platform settings, project files, and network connectivity.
When diagnosing only local configuration or working offline, use:

```sh
riff diagnose --no-network
```

## Confirm the executable and project

```sh
command -v riff
riff --version
riff validate
riff config --list --source
```

`config --list` may include authentication values. Redact its output before
putting it in logs, chat, or an issue.

If a tool invokes `composer`, also run `command -v composer` and
`composer --version`; a system Composer installation may appear earlier on
`PATH` than Riff's shim.

For a project outside the current directory, pass `-d <directory>` to commands
that support it. If `COMPOSER` is set, confirm it names a file rather than a
directory and points to the intended manifest.

## PHP is missing or the platform is wrong

Check the selected PHP directly:

```sh
php --version
riff --php /absolute/path/to/php show --platform
riff --php /absolute/path/to/php check-platform-reqs --lock
```

The precedence is `--php`, `RIFF_PHP`, `PHP_BINARY`, then `php` on `PATH`.
Inspect those variables if Riff finds a different runtime than your shell.

Platform probes are cached. Force one fresh probe while diagnosing changes to
PHP or ini configuration:

```sh
RIFF_NO_PLATFORM_CACHE=1 riff show --platform
```

## A Composer plugin is unsupported

Riff stops before installation when an enabled plugin has no native adapter;
this protects the project from a silently incomplete install. Check the adapter
list in [Compatibility](compatibility.md#composer-plugins).

If the plugin's behavior is required, use Composer for that project or help add
a native implementation. Use `--no-plugins` only when you have established that
the project does not depend on the plugin's generated files, installer changes,
or lifecycle behavior.

## Downloads or repository metadata fail

Run the network diagnostics and inspect configured repositories:

```sh
riff diagnose
riff repository list
riff config --list --source
```

Confirm credentials come from the expected auth file or `COMPOSER_AUTH`. Do not
paste tokens into an issue or CI log.

If cached content is suspected, garbage-collect first, then clear the entire
Riff cache only if needed:

```sh
riff clear-cache --gc
riff clear-cache
```

Set `RIFF_CACHE_DIR` to isolate a reproduction without changing the normal
cache:

```sh
RIFF_CACHE_DIR=/tmp/riff-reproduction-cache riff install
```

## The lock file and manifest disagree

Validate the project first:

```sh
riff validate --strict
```

If `composer.json` changed intentionally, preview resolution with
`riff update --dry-run`, then run the narrowest appropriate update and review
the lock diff. Do not delete a committed lock file merely to hide a freshness
error.

## Installed package files changed

```sh
riff status --verbose
```

Use `riff reinstall vendor/package` to restore an unpatched package from its
distribution. If the change should be maintained, use the first-class
[Package patching](patching.md) workflow instead of editing `vendor/` directly.

## Patch state is invalid

```sh
riff patches-doctor
riff patches-repatch --dry-run
```

If declarations and patch files were intentionally edited, regenerate locks
with `riff patches-relock`, review the resulting diff, then run
`riff patches-repatch`. Do not hand-edit installed patch fingerprints below
`vendor/composer/`.

## Completion is stale

Regenerate completion using the current binary:

```sh
riff completion bash > /path/loaded/by/your/shell/riff.bash
```

Replace `bash` and the destination for your shell. Start a new shell or reload
its startup file afterward.

## Report a reproducible issue

Include:

- `riff --version` and how Riff was installed;
- operating system and architecture;
- `php --version` and the selected PHP path when platform behavior is involved;
- the exact Riff command and complete error output;
- a minimal manifest and repository setup with every credential removed;
- whether the failure persists with a fresh `RIFF_CACHE_DIR`;
- plugin and patch configuration relevant to the operation.

For sensitive or private repositories, reduce the case to synthetic package
names and metadata before sharing it.
