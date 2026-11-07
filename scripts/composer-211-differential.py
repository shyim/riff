#!/usr/bin/env python3
"""Offline CLI regression comparison for Composer 2.11.0/2.11.1 changes.

Set COMPOSER_SRC_DIR to a bootstrapped Composer 2.11.1 checkout and RIFF_BIN
if the candidate is not target/debug/riff. Fixtures and credentials are isolated.
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent
COMPOSER = Path(os.environ.get("COMPOSER_SRC_DIR", ROOT.parent / "composer")) / "bin/composer"
RIFF = Path(os.environ.get("RIFF_BIN", ROOT / "target/debug/riff"))
PHP = os.environ.get("PHP_BIN", shutil.which("php") or "php")
CASES = [
    ("set-array-entry", {"extra": {"items": ["a", "b"]}}, [["config", "extra.items.1", "changed"]]),
    ("remove-root-array-entry", {"extra": ["one", "two"]}, [["config", "extra.0", "--unset"]]),
    ("empty-array-remains-array", {"extra": ["a"]}, [["config", "extra.0", "--unset"]]),
    ("literal-dotted-property", {}, [["config", "extra.foo.bar.baz", "value"]]),
    ("require-preserves-empty-sections", {"require-dev": {}, "repositories": []}, [["require", "a/b:^1", "--no-update", "--no-plugins", "--no-scripts"]]),
    ("remove-preserves-empty-sections", {"require": {"a/b": "^1"}, "require-dev": {}, "repositories": []}, [["remove", "a/b", "--no-update", "--no-plugins", "--no-scripts"]]),
    ("nested-composer-quoted-argument", {"scripts": {"set-description": "@composer config description", "quoted": '@set-description "two words"'}}, [["run-script", "quoted"]]),
]


def strict_psr_cases(root):
    count = 0
    for command in ("install", "update", "dump-autoload"):
        for configured in (False, True):
            observed = []
            for tool, executable in [("composer", [PHP, str(COMPOSER)]), ("riff", [str(RIFF)])]:
                project = root / f"strict-{command}-{configured}" / tool
                project.mkdir(parents=True)
                (project / "src").mkdir()
                (project / "src/Wrong.php").write_text("<?php namespace App; class Correct {}")
                (project / "composer.json").write_text(json.dumps({
                    "name": "fixture/root", "repositories": {"packagist.org": False},
                    "autoload": {"psr-4": {"App\\": "src/"}},
                    "config": {"optimize-autoloader": True, "strict-psr-autoloader": configured},
                    "scripts": {
                        "post-autoload-dump": "@php -r \"file_put_contents('post-dump', 'yes');\"",
                        "post-install-cmd": "@php -r \"file_put_contents('post-operation', 'yes');\"",
                        "post-update-cmd": "@php -r \"file_put_contents('post-operation', 'yes');\"",
                    },
                }))
                env = {k: v for k, v in os.environ.items() if not k.startswith("COMPOSER") and k not in ("RIFF_PHP", "RIFF_CACHE_DIR")}
                env.update(COMPOSER_HOME=str(project / "home"), COMPOSER_ALLOW_SUPERUSER="1", COMPOSER_NO_INTERACTION="1", RIFF_PHP=PHP)
                if command == "install":
                    subprocess.run(executable + ["update", "--no-install", "--no-scripts"], cwd=project, env=env, capture_output=True, check=True, timeout=30)
                args = [command]
                if not configured:
                    args += ["--strict-psr" if command == "dump-autoload" else "--strict-psr-autoloader"]
                result = subprocess.run(executable + args, cwd=project, env=env, capture_output=True, text=True, timeout=30)
                observed.append((result.returncode, (project / "vendor/autoload.php").exists(), (project / "post-dump").exists(), (project / "post-operation").exists()))
            expected = (1 if command == "dump-autoload" else 6, True, True, False)
            assert observed == [expected, expected], (command, configured, observed)
            print(f"PASS strict-{command}-config-{configured}")
            count += 1
    return count


def main():
    with tempfile.TemporaryDirectory(prefix="riff-composer-211-") as temporary:
        root = Path(temporary)
        for name, manifest, commands in CASES:
            results = []
            for tool, executable in [("composer", [PHP, str(COMPOSER)]), ("riff", [str(RIFF)])]:
                project = root / name / tool
                project.mkdir(parents=True)
                (project / "composer.json").write_text(json.dumps(manifest))
                env = dict(os.environ)
                for key in list(env):
                    if key.startswith("COMPOSER_") or key in ("COMPOSER", "RIFF_PHP", "RIFF_CACHE_DIR"):
                        del env[key]
                env.update(COMPOSER_HOME=str(project / "home"), RIFF_CACHE_DIR=str(project / "cache"), COMPOSER_CACHE_DIR=str(project / "composer-cache"), COMPOSER_ALLOW_SUPERUSER="1", COMPOSER_NO_INTERACTION="1", RIFF_PHP=PHP)
                for command in commands:
                    result = subprocess.run(executable + ["--no-ansi"] + command, cwd=project, env=env, capture_output=True, text=True, timeout=30)
                    if result.returncode:
                        raise RuntimeError(f"{name}/{tool}: {result.stdout}{result.stderr}")
                results.append(json.loads((project / "composer.json").read_text()))
            if results[0] != results[1]:
                raise AssertionError(f"{name}: Composer={results[0]!r}, Riff={results[1]!r}")
            print(f"PASS {name}")
        strict_count = strict_psr_cases(root)
    print(f"{len(CASES) + strict_count} Composer 2.11 differential cases passed")


if __name__ == "__main__":
    main()
