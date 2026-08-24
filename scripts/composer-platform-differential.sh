#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
composer_dir=${COMPOSER_SRC_DIR:-/workspace/composer}
php_bin=${PHP_BIN:-$(command -v php || true)}
composer_rs_bin=${COMPOSER_RS_BIN:-$repo_root/target/debug/sonata}

if [[ ! -x "$php_bin" || ! -x "$composer_rs_bin" ]]; then
    echo "Build sonata and provide a PHP CLI before platform differential tests." >&2
    exit 1
fi
if [[ ! -f "$composer_dir/bin/composer" || ! -f "$composer_dir/vendor/autoload.php" ]]; then
    echo "Composer reference checkout is not bootstrapped at $composer_dir." >&2
    exit 1
fi
command -v jq >/dev/null 2>&1 || { echo "jq is required." >&2; exit 1; }

tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT

"$php_bin" "$composer_dir/bin/composer" show --platform --format=json --no-ansi \
    > "$tmp_dir/reference.json"
"$composer_rs_bin" --php "$php_bin" show --platform --format=json \
    > "$tmp_dir/candidate.json"

# Composer reports additional libraries on some builds. PHP capabilities and
# extensions must agree exactly for the same selected executable.
jq -S '[.platform[] | select(.name == "php" or (.name | startswith("php-")) or (.name | startswith("ext-"))) | {name, version}]' \
    "$tmp_dir/reference.json" > "$tmp_dir/reference-platform.json"
jq -S '[.platform[] | select(.name == "php" or (.name | startswith("php-")) or (.name | startswith("ext-"))) | {name, version}]' \
    "$tmp_dir/candidate.json" > "$tmp_dir/candidate-platform.json"

diff -u "$tmp_dir/reference-platform.json" "$tmp_dir/candidate-platform.json"

project=$tmp_dir/project
mkdir -p "$project"
printf '%s\n' '{"config":{"platform":{"php":"8.2.0","ext-json":false,"ext-sonata-test":"1.2.3"}}}' \
    > "$project/composer.json"
"$composer_rs_bin" --php "$php_bin" show --platform --format=json -d "$project" \
    > "$tmp_dir/overrides.json"

jq -e '.platform | any(.name == "php" and .version == "8.2.0")' "$tmp_dir/overrides.json" >/dev/null
jq -e '.platform | any(.name == "ext-sonata-test" and .version == "1.2.3")' "$tmp_dir/overrides.json" >/dev/null
jq -e '.platform | all(.name != "ext-json")' "$tmp_dir/overrides.json" >/dev/null

printf 'PASS %-32s PHP and extension packages\n' platform-snapshot
printf 'PASS %-32s config.platform semantics\n' platform-overrides
