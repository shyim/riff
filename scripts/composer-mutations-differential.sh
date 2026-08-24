#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
composer_dir=${COMPOSER_SRC_DIR:-$(cd "$repo_root/.." && pwd)/composer}
php_bin=${PHP_BIN:-$(command -v php || true)}
composer_rs_bin=${COMPOSER_RS_BIN:-$repo_root/target/debug/sonata}
export COMPOSER_RS_PHP="$php_bin"

if [[ ! -x "$php_bin" || ! -x "$composer_rs_bin" || ! -f "$composer_dir/vendor/autoload.php" ]]; then
    echo "Build PHP, sonata, and the Composer reference before running mutation differential tests." >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "jq is required for mutation differential tests." >&2
    exit 1
fi

tmp_dir=$(mktemp -d)
if [[ ${KEEP_TMP:-0} == 1 ]]; then
    printf 'Keeping mutation differential workspace at %s\n' "$tmp_dir"
else
    trap 'rm -rf "$tmp_dir"' EXIT
fi
reference_root=$tmp_dir/reference
candidate_root=$tmp_dir/candidate
reference_dir=$reference_root/project
candidate_dir=$candidate_root/project
mkdir -p "$reference_dir" "$candidate_dir" "$tmp_dir/home-reference" "$tmp_dir/home-candidate"

create_fixture() {
    local root=$1
    mkdir -p \
        "$root/packages/base/src" \
        "$root/packages/app/src" \
        "$root/packages/tool/src" \
        "$root/packages/dev/src" \
        "$root/packages/optional/src" \
        "$root/project/src" \
        "$root/project/tests"

    printf '%s\n' '{"name":"fixture/base","version":"1.0.0","autoload":{"psr-4":{"Fixture\\Base\\":"src/"}}}' \
        > "$root/packages/base/composer.json"
    printf '%s\n' '<?php namespace Fixture\Base; final class Base { public static function value(): string { return "base"; } }' \
        > "$root/packages/base/src/Base.php"

    printf '%s\n' '{"name":"fixture/app","version":"1.2.3","require":{"fixture/base":"^1.0"},"autoload":{"psr-4":{"Fixture\\App\\":"src/"}}}' \
        > "$root/packages/app/composer.json"
    printf '%s\n' '<?php namespace Fixture\App; final class App { public static function value(): string { return \Fixture\Base\Base::value() . "-app"; } }' \
        > "$root/packages/app/src/App.php"

    printf '%s\n' '{"name":"fixture/tool","version":"0.1.3","autoload":{"psr-4":{"Fixture\\Tool\\":"src/"}}}' \
        > "$root/packages/tool/composer.json"
    printf '%s\n' '<?php namespace Fixture\Tool; final class Tool {}' \
        > "$root/packages/tool/src/Tool.php"

    printf '%s\n' '{"name":"fixture/dev","version":"1.0.0","autoload":{"psr-4":{"Fixture\\Dev\\":"src/"}}}' \
        > "$root/packages/dev/composer.json"
    printf '%s\n' '<?php namespace Fixture\Dev; final class Tool {}' \
        > "$root/packages/dev/src/Tool.php"

    printf '%s\n' '{"name":"fixture/optional","version":"2.4.1","autoload":{"psr-4":{"Fixture\\Optional\\":"src/"}}}' \
        > "$root/packages/optional/composer.json"
    printf '%s\n' '<?php namespace Fixture\Optional; final class Feature {}' \
        > "$root/packages/optional/src/Feature.php"

    printf '%s\n' '<?php namespace Root; final class App { public static function value(): string { return \Fixture\App\App::value(); } }' \
        > "$root/project/src/App.php"
    printf '%s\n' '<?php namespace Root\Tests; final class Fixture { public static function value(): string { return "tests"; } }' \
        > "$root/project/tests/Fixture.php"

    jq -n '{
        name: "fixture/root",
        repositories: [
            {type: "path", url: "../packages/*", options: {symlink: false}},
            {"packagist.org": false}
        ],
        autoload: {"psr-4": {"Root\\": "src/"}},
        "autoload-dev": {"psr-4": {"Root\\Tests\\": "tests/"}},
        scripts: {
            "pre-update-cmd": "printf \"pre-update\\n\" >> events.log",
            "post-update-cmd": "printf \"post-update\\n\" >> events.log",
            "pre-autoload-dump": "printf \"pre-autoload\\n\" >> events.log",
            "post-autoload-dump": "printf \"post-autoload\\n\" >> events.log"
        }
    }' > "$root/project/composer.json"
}

create_fixture "$reference_root"
create_fixture "$candidate_root"

fail() {
    printf 'FAIL %-32s %s\n' "$1" "$2" >&2
    exit 1
}

run_pair() {
    local case_name=$1
    local command=$2
    shift 2
    local reference_output=$tmp_dir/$case_name-reference.out
    local candidate_output=$tmp_dir/$case_name-sonata.out
    local -a reference_flags=(--no-ansi --no-interaction)
    if [[ $command == require || $command == remove ]]; then
        reference_flags+=(--no-progress --no-audit)
    fi

    set +e
    COMPOSER_HOME="$tmp_dir/home-reference" "$php_bin" "$composer_dir/bin/composer" \
        "$command" "${reference_flags[@]}" -d "$reference_dir" "$@" \
        > "$reference_output" 2>&1
    local reference_code=$?
    COMPOSER_HOME="$tmp_dir/home-candidate" "$composer_rs_bin" "$command" -d "$candidate_dir" "$@" \
        > "$candidate_output" 2>&1
    local candidate_code=$?
    set -e

    if [[ $reference_code -ne 0 || $candidate_code -ne 0 ]]; then
        printf '%s\n' '--- Composer output ---' >&2
        sed -n '1,120p' "$reference_output" >&2
        printf '%s\n' '--- sonata output ---' >&2
        sed -n '1,120p' "$candidate_output" >&2
        fail "$case_name" "exit code: Composer=$reference_code sonata=$candidate_code"
    fi
    printf 'PASS %-32s exit=0\n' "$case_name"
}

compare_projection() {
    local case_name=$1
    local relative=$2
    local filter=$3
    local reference_file=$reference_dir/$relative
    local candidate_file=$candidate_dir/$relative
    [[ -f $reference_file && -f $candidate_file ]] || fail "$case_name" "$relative is missing"
    jq -S "$filter" "$reference_file" > "$tmp_dir/$case_name-reference.json"
    jq -S "$filter" "$candidate_file" > "$tmp_dir/$case_name-sonata.json"
    diff -u "$tmp_dir/$case_name-reference.json" "$tmp_dir/$case_name-sonata.json" \
        || fail "$case_name" "$relative projection differs"
    printf 'PASS %-32s semantic JSON\n' "$case_name"
}

manifest_projection='{require: (.require // {}), require_dev: (."require-dev" // {}), repositories}'
lock_projection='[.packages[], .["packages-dev"][]] | sort_by(.name) | map({name, version})'
installed_projection='.packages | sort_by(.name) | map({name, version})'

run_pair require-explicit require 'fixture/app:^1.0'
compare_projection require-explicit-manifest composer.json "$manifest_projection"
compare_projection require-explicit-lock composer.lock "$lock_projection"
compare_projection require-explicit-installed vendor/composer/installed.json "$installed_projection"

run_pair require-inferred require fixture/tool
compare_projection require-inferred-manifest composer.json "$manifest_projection"
[[ $(jq -r '.require["fixture/tool"]' "$candidate_dir/composer.json") == '^0.1.3' ]] \
    || fail require-inferred-constraint "expected ^0.1.3"
printf 'PASS %-32s %s\n' require-inferred-constraint '^0.1.3'

run_pair require-dev require --dev 'fixture/dev:^1.0'
compare_projection require-dev-manifest composer.json "$manifest_projection"
compare_projection require-dev-lock composer.lock "$lock_projection"

reference_runtime=$($php_bin -r "require '$reference_dir/vendor/autoload.php'; echo Root\\App::value(), ':', Root\\Tests\\Fixture::value();")
candidate_runtime=$($php_bin -r "require '$candidate_dir/vendor/autoload.php'; echo Root\\App::value(), ':', Root\\Tests\\Fixture::value();")
[[ $reference_runtime == "$candidate_runtime" && $candidate_runtime == 'base-app:tests' ]] \
    || fail require-autoload-runtime "Composer=$reference_runtime sonata=$candidate_runtime"
printf 'PASS %-32s %s\n' require-autoload-runtime "$candidate_runtime"

for project in "$reference_dir" "$candidate_dir"; do
    : > "$project/events.log"
done
run_pair dump-autoload dump-autoload
cmp -s "$reference_dir/events.log" "$candidate_dir/events.log" \
    || fail dump-autoload-events "lifecycle events differ"
printf 'PASS %-32s lifecycle events\n' dump-autoload-events

run_pair dump-authoritative dump-autoload --classmap-authoritative --no-scripts
reference_runtime=$($php_bin -r "require '$reference_dir/vendor/autoload.php'; echo Root\\App::value();")
candidate_runtime=$($php_bin -r "require '$candidate_dir/vendor/autoload.php'; echo Root\\App::value();")
[[ $reference_runtime == "$candidate_runtime" && $candidate_runtime == 'base-app' ]] \
    || fail dump-authoritative-runtime "Composer=$reference_runtime sonata=$candidate_runtime"
printf 'PASS %-32s %s\n' dump-authoritative-runtime "$candidate_runtime"

run_pair dump-no-dev dump-autoload --no-dev --no-scripts
set +e
$php_bin -r "require '$reference_dir/vendor/autoload.php'; new Root\\Tests\\Fixture();" >/dev/null 2>&1
reference_code=$?
$php_bin -r "require '$candidate_dir/vendor/autoload.php'; new Root\\Tests\\Fixture();" >/dev/null 2>&1
candidate_code=$?
set -e
[[ $reference_code -ne 0 && $candidate_code -ne 0 ]] \
    || fail dump-no-dev-runtime "dev root namespace remained autoloadable"
printf 'PASS %-32s dev namespace omitted\n' dump-no-dev-runtime

reference_json_hash=$(sha256sum "$reference_dir/composer.json" | cut -d' ' -f1)
candidate_json_hash=$(sha256sum "$candidate_dir/composer.json" | cut -d' ' -f1)
reference_lock_hash=$(sha256sum "$reference_dir/composer.lock" | cut -d' ' -f1)
candidate_lock_hash=$(sha256sum "$candidate_dir/composer.lock" | cut -d' ' -f1)
set +e
COMPOSER_HOME="$tmp_dir/home-reference" "$php_bin" "$composer_dir/bin/composer" require \
    --no-ansi --no-interaction --no-progress --no-audit -d "$reference_dir" 'missing/package:^1.0' \
    > "$tmp_dir/require-failure-reference.out" 2>&1
reference_code=$?
COMPOSER_HOME="$tmp_dir/home-candidate" "$composer_rs_bin" require -d "$candidate_dir" 'missing/package:^1.0' \
    > "$tmp_dir/require-failure-sonata.out" 2>&1
candidate_code=$?
set -e
[[ $reference_code -ne 0 && $candidate_code -ne 0 ]] || fail require-failure "both commands must fail"
[[ $reference_json_hash == "$(sha256sum "$reference_dir/composer.json" | cut -d' ' -f1)" \
    && $candidate_json_hash == "$(sha256sum "$candidate_dir/composer.json" | cut -d' ' -f1)" \
    && $reference_lock_hash == "$(sha256sum "$reference_dir/composer.lock" | cut -d' ' -f1)" \
    && $candidate_lock_hash == "$(sha256sum "$candidate_dir/composer.lock" | cut -d' ' -f1)" ]] \
    || fail require-failure-rollback "composer.json or composer.lock changed"
printf 'PASS %-32s project files restored\n' require-failure-rollback

run_pair remove-explicit remove fixture/app
compare_projection remove-manifest composer.json "$manifest_projection"
compare_projection remove-lock composer.lock "$lock_projection"
[[ ! -e $reference_dir/vendor/fixture/app && ! -e $candidate_dir/vendor/fixture/app \
    && ! -e $reference_dir/vendor/fixture/base && ! -e $candidate_dir/vendor/fixture/base ]] \
    || fail remove-vendor "removed dependency closure remains installed"
printf 'PASS %-32s dependency closure removed\n' remove-vendor

run_pair remove-dev remove --dev fixture/dev
compare_projection remove-dev-manifest composer.json "$manifest_projection"
[[ ! -e $reference_dir/vendor/fixture/dev && ! -e $candidate_dir/vendor/fixture/dev ]] \
    || fail remove-dev-vendor "development dependency remains installed"
printf 'PASS %-32s development package removed\n' remove-dev-vendor

reference_lock_hash=$(sha256sum "$reference_dir/composer.lock" | cut -d' ' -f1)
candidate_lock_hash=$(sha256sum "$candidate_dir/composer.lock" | cut -d' ' -f1)
run_pair require-no-update require --no-update fixture/optional
compare_projection require-no-update-manifest composer.json "$manifest_projection"
[[ $reference_lock_hash == "$(sha256sum "$reference_dir/composer.lock" | cut -d' ' -f1)" \
    && $candidate_lock_hash == "$(sha256sum "$candidate_dir/composer.lock" | cut -d' ' -f1)" ]] \
    || fail require-no-update-lock "composer.lock changed"
printf 'PASS %-32s lock unchanged\n' require-no-update-lock

printf 'All Composer mutation differential cases passed.\n'
