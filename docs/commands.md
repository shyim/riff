# Command reference

This page maps tasks to Riff's top-level commands. The installed CLI is the
exhaustive reference for arguments and flags:

```sh
riff --help
riff <command> --help
```

## Project and dependency lifecycle

| Command | Purpose |
| --- | --- |
| `init` | Create a `composer.json` interactively or from flags. |
| `create-project` | Create a project from a package and optionally install it. |
| `install` | Install the dependency graph, preferring `composer.lock` when present. |
| `update` | Resolve constraints and update `composer.lock`. |
| `require` (`add`) | Add requirements and normally update/install them. |
| `remove` | Remove requirements and normally update/install the result. |
| `reinstall` | Reinstall selected packages without changing requirements. |
| `bump` | Raise dependency lower bounds to installed versions. |
| `dump-autoload` | Regenerate Composer-compatible autoload files. |
| `archive` | Create a `tar` or `zip` archive of a package. |

## Inspect and verify

| Command | Purpose |
| --- | --- |
| `show` (`info`) | Inspect installed, locked, available, or platform packages. |
| `outdated` | Find dependencies with newer matching versions. |
| `why` (`depends`) | Explain why a package is in the dependency graph. |
| `why-not` (`prohibits`) | Explain which constraints prohibit a package version. |
| `status` | Detect local changes in installed packages. |
| `validate` | Validate `composer.json`, its lock relation, and optional dependencies. |
| `check-platform-reqs` | Check PHP and extension requirements against the runtime. |
| `audit` | Report security advisories and abandoned packages. |
| `licenses` (`license`) | Report dependency licenses. |
| `suggests` (`suggest`) | Show package suggestions. |
| `fund` | Show dependency funding links. |
| `search` | Search configured package repositories. |
| `browse` (`home`) | Open or print a package repository or homepage URL. |

## Scripts and binaries

| Command | Purpose |
| --- | --- |
| `run` (`run-script`) | List or execute project scripts. |
| `exec` | List or execute binaries installed by dependencies. |
| `global` | Run a Riff command in Composer's global home directory. |

A project script can also be invoked directly with `riff <script-name>` when
its name does not match a built-in command.

## Configuration and maintenance

| Command | Purpose |
| --- | --- |
| `config` | Read, list, edit, set, or unset project/global configuration and auth. |
| `repository` (`repo`) | List, add, edit, order, enable, or disable repositories. |
| `policy` | Add custom dependency policy sources. |
| `diagnose` | Check platform settings, project files, and network connectivity. |
| `clear-cache` (`clearcache`, `cc`) | Clear Riff's internal runtime cache or run cache GC. |
| `completion` | Generate completion for Bash, Zsh, Fish, or PowerShell. |
| `about` | Show a short description of Riff. |

## Package patching

| Command | Alias | Purpose |
| --- | --- | --- |
| `patch` | — | Extract an installed dependency into an editable workspace. |
| `patch-commit` | — | Generate, declare, lock, and install a patch from that workspace. |
| `patch-remove` | — | Remove native patch declarations and restore package contents. |
| `patches-relock` | `prl` | Regenerate native and Composer-compatible patch locks. |
| `patches-repatch` | `prp` | Cleanly reinstall packages and apply their current patches. |
| `patches-doctor` | `pd` | Validate declarations, patch files, locks, and installed state. |

See [Package patching](patching.md) for the full workflow and supported patch
formats.

## Symfony Flex recipes

These commands are available when `symfony/flex` 2.x is enabled by the root
project:

| Command | Aliases | Purpose |
| --- | --- | --- |
| `recipes` | `symfony:recipes` | Show installed recipes and report available updates. |
| `recipes:install` | `symfony:recipes:install`, `sync-recipes`, `symfony:sync-recipes`, `fix-recipes` | Install missing recipes or reinstall selected recipes. |
| `recipes:update` | `symfony:recipes:update` | Three-way merge the latest recipe into a Git working tree. |
| `dump-env` | `symfony:dump-env` | Compile Symfony dotenv files into `.env.local.php`. |

Recipe metadata and downloads use Riff's cache namespace. Use
`RIFF_CACHE_DIR` to relocate it without mixing it with Composer's cache.

## Global flags

These flags are available across the command surface:

| Flag | Purpose |
| --- | --- |
| `--php <PATH>` | Select the PHP executable used for platform detection and `@php` scripts. |
| `--output text|json` | Select human text or newline-delimited JSON event output. |
| `-q`, `--quiet` | Suppress informational, success, and progress events. |
| `--no-progress` | Disable interactive progress output. |
| `--ansi`, `--no-ansi` | Force or disable ANSI styling. |
| `-h`, `--help` | Show help for the selected command. |
| `-V`, `--version` | Show the Riff version at the top level. |

Many project commands additionally accept `-d` or `--working-dir`. Flag
availability and placement are always shown by the command's own help.
