#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
composer_dir=${COMPOSER_SRC_DIR:-$(cd "$repo_root/.." && pwd)/composer}
php_bin=${PHP_BIN:-$(command -v php || true)}
composer_rs_bin=${COMPOSER_RS_BIN:-$repo_root/target/debug/composer-rs}
export COMPOSER_RS_PHP="$php_bin"

if [[ ! -x "$php_bin" || ! -x "$composer_rs_bin" ]]; then
    echo "Build PHP and composer-rs before running core differential tests." >&2
    exit 1
fi
if [[ ! -f "$composer_dir/bin/composer" || ! -f "$composer_dir/vendor/autoload.php" ]]; then
    echo "Composer reference checkout is not bootstrapped at $composer_dir." >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "jq is required for core differential tests." >&2
    exit 1
fi

tmp_dir=$(mktemp -d)
if [[ ${KEEP_TMP:-0} == 1 ]]; then
    printf 'Keeping core differential workspace at %s\n' "$tmp_dir"
else
    trap 'rm -rf "$tmp_dir"' EXIT
fi
failures=0

record_failure() {
    printf 'FAIL %-32s %s\n' "$1" "$2" >&2
    failures=$((failures + 1))
}

run_pair() {
    local case_name=$1
    local command=$2
    shift 2
    local reference_output=$tmp_dir/$case_name-reference.out
    local candidate_output=$tmp_dir/$case_name-composer-rs.out
    local reference_code candidate_code
    local -a reference_flags=(--no-ansi --no-interaction --no-progress)
    local -a candidate_flags=(--no-ansi --no-interaction --no-progress --no-audit)
    if [[ $command == update ]]; then
        reference_flags+=(--no-audit)
    fi

    set +e
    COMPOSER_HOME="$tmp_dir/home-reference" "$php_bin" "$composer_dir/bin/composer" \
        "$command" "${reference_flags[@]}" \
        -d "$reference_dir" "$@" > "$reference_output" 2>&1
    reference_code=$?
    COMPOSER_HOME="$tmp_dir/home-candidate" "$composer_rs_bin" \
        "$command" "${candidate_flags[@]}" \
        -d "$candidate_dir" "$@" > "$candidate_output" 2>&1
    candidate_code=$?
    set -e

    if [[ $reference_code -ne $candidate_code ]]; then
        record_failure "$case_name" "exit code: Composer=$reference_code composer-rs=$candidate_code"
        printf '%s\n' '--- Composer output ---' >&2
        sed -n '1,100p' "$reference_output" >&2
        printf '%s\n' '--- composer-rs output ---' >&2
        sed -n '1,100p' "$candidate_output" >&2
        return 1
    fi
    if [[ $candidate_code -ne 0 ]]; then
        record_failure "$case_name" "both commands exited $candidate_code"
        return 1
    fi

    printf 'PASS %-32s exit=0\n' "$case_name"
}

compare_json_projection() {
    local case_name=$1
    local reference_file=$2
    local candidate_file=$3
    local filter=$4
    local reference_json=$tmp_dir/$case_name-reference.json
    local candidate_json=$tmp_dir/$case_name-composer-rs.json

    if [[ ! -f "$reference_file" || ! -f "$candidate_file" ]]; then
        record_failure "$case_name" "required JSON file is missing"
        return 1
    fi
    jq -S "$filter" "$reference_file" > "$reference_json"
    jq -S "$filter" "$candidate_file" > "$candidate_json"
    if ! diff -u "$reference_json" "$candidate_json"; then
        record_failure "$case_name" "JSON projection differs"
        return 1
    fi
    printf 'PASS %-32s semantic JSON\n' "$case_name"
}

compare_file() {
    local case_name=$1
    local relative=$2
    if ! cmp -s "$reference_dir/$relative" "$candidate_dir/$relative"; then
        record_failure "$case_name" "$relative differs"
        return 1
    fi
    printf 'PASS %-32s %s\n' "$case_name" "$relative"
}

create_package_tree() {
    local root=$1
    mkdir -p "$root/packages/base/src" "$root/packages/app/src" "$root/packages/dev/src"

    printf '%s\n' '{"name":"fixture/base","version":"1.0.0","autoload":{"psr-4":{"Fixture\\Base\\":"src/"}}}' \
        > "$root/packages/base/composer.json"
    printf '%s\n' '<?php namespace Fixture\Base; final class Base { public static function value(): string { return "base"; } }' \
        > "$root/packages/base/src/Base.php"

    printf '%s\n' '{"name":"fixture/app","version":"1.0.0","require":{"fixture/base":"^1.0"},"autoload":{"psr-4":{"Fixture\\App\\":"src/"}}}' \
        > "$root/packages/app/composer.json"
    printf '%s\n' '<?php namespace Fixture\App; use Fixture\Base\Base; final class App { public static function value(): string { return Base::value() . "-app-1.0"; } }' \
        > "$root/packages/app/src/App.php"

    printf '%s\n' '{"name":"fixture/dev","version":"1.0.0","autoload":{"psr-4":{"Fixture\\Dev\\":"src/"}}}' \
        > "$root/packages/dev/composer.json"
    printf '%s\n' '<?php namespace Fixture\Dev; final class Tool {}' \
        > "$root/packages/dev/src/Tool.php"
}

write_root_manifest() {
    local project=$1
    jq -n '{
        name: "fixture/root",
        require: {"fixture/app": "^1.0"},
        "require-dev": {"fixture/dev": "^1.0"},
        repositories: [
            {type: "path", url: "../packages/*", options: {symlink: false}},
            {"packagist.org": false}
        ],
        scripts: {
            "pre-update-cmd": "printf \"pre-update\\n\" >> events.log",
            "post-update-cmd": "printf \"post-update\\n\" >> events.log",
            "pre-install-cmd": "printf \"pre-install\\n\" >> events.log",
            "post-install-cmd": "printf \"post-install\\n\" >> events.log",
            "pre-autoload-dump": "printf \"pre-autoload\\n\" >> events.log",
            "post-autoload-dump": "printf \"post-autoload\\n\" >> events.log"
        }
    }' > "$project/composer.json"
}

prepare_pair() {
    local case_name=$1
    reference_root=$tmp_dir/$case_name/reference
    candidate_root=$tmp_dir/$case_name/candidate
    reference_dir=$reference_root/project
    candidate_dir=$candidate_root/project
    mkdir -p "$reference_dir" "$candidate_dir" "$tmp_dir/home-reference" "$tmp_dir/home-candidate"
    create_package_tree "$reference_root"
    create_package_tree "$candidate_root"
    write_root_manifest "$reference_dir"
    write_root_manifest "$candidate_dir"
}

lock_projection='[.packages[], .["packages-dev"][]] | sort_by(.name) | map({name, version, require})'
installed_projection='.packages | sort_by(.name) | map({name, version, source: .["installation-source"]})'

prepare_pair core
run_pair initial-update update
compare_json_projection initial-lock \
    "$reference_dir/composer.lock" "$candidate_dir/composer.lock" "$lock_projection"
compare_json_projection initial-installed \
    "$reference_dir/vendor/composer/installed.json" \
    "$candidate_dir/vendor/composer/installed.json" "$installed_projection"
compare_file update-events events.log

set +e
reference_autoload=$("$php_bin" -r "require '$reference_dir/vendor/autoload.php'; echo Fixture\\App\\App::value();" 2>&1)
reference_autoload_code=$?
candidate_autoload=$("$php_bin" -r "require '$candidate_dir/vendor/autoload.php'; echo Fixture\\App\\App::value();" 2>&1)
candidate_autoload_code=$?
set -e
if [[ $reference_autoload_code -ne $candidate_autoload_code \
    || "$reference_autoload" != "$candidate_autoload" \
    || "$candidate_autoload" != "base-app-1.0" ]]; then
    record_failure autoload-runtime \
        "Composer[$reference_autoload_code]=$reference_autoload composer-rs[$candidate_autoload_code]=$candidate_autoload"
else
    printf 'PASS %-32s %s\n' autoload-runtime "$candidate_autoload"
fi

rm -rf "$reference_dir/vendor" "$candidate_dir/vendor"
: > "$reference_dir/events.log"
: > "$candidate_dir/events.log"
run_pair locked-install install
compare_json_projection install-installed \
    "$reference_dir/vendor/composer/installed.json" \
    "$candidate_dir/vendor/composer/installed.json" "$installed_projection"
compare_file install-events events.log

rm -rf "$reference_dir/vendor" "$candidate_dir/vendor"
: > "$reference_dir/events.log"
: > "$candidate_dir/events.log"
run_pair install-no-dev install --no-dev
if [[ -e "$reference_dir/vendor/fixture/dev" || -e "$candidate_dir/vendor/fixture/dev" ]]; then
    record_failure install-no-dev "dev dependency was installed"
else
    printf 'PASS %-32s dev dependency omitted\n' install-no-dev-vendor
fi
compare_json_projection no-dev-lock-preserved \
    "$reference_dir/composer.lock" "$candidate_dir/composer.lock" "$lock_projection"

rm -rf "$reference_dir/vendor" "$candidate_dir/vendor"
: > "$reference_dir/events.log"
: > "$candidate_dir/events.log"
run_pair install-no-autoloader install --no-autoloader
if [[ -e "$reference_dir/vendor/autoload.php" || -e "$candidate_dir/vendor/autoload.php" ]]; then
    record_failure install-no-autoloader "autoload.php was generated"
else
    printf 'PASS %-32s autoloader omitted\n' install-no-autoloader-files
fi
compare_json_projection no-autoloader-installed \
    "$reference_dir/vendor/composer/installed.json" \
    "$candidate_dir/vendor/composer/installed.json" "$installed_projection"

run_pair restore-autoloader install
for project_root in "$reference_root" "$candidate_root"; do
    jq '.version = "1.1.0"' "$project_root/packages/app/composer.json" \
        > "$project_root/packages/app/composer.json.tmp"
    mv "$project_root/packages/app/composer.json.tmp" "$project_root/packages/app/composer.json"
    printf '%s\n' '<?php namespace Fixture\App; use Fixture\Base\Base; final class App { public static function value(): string { return Base::value() . "-app-1.1"; } }' \
        > "$project_root/packages/app/src/App.php"
done
run_pair partial-upgrade update fixture/app
compare_json_projection upgraded-lock \
    "$reference_dir/composer.lock" "$candidate_dir/composer.lock" "$lock_projection"
compare_file upgraded-vendor vendor/fixture/app/src/App.php

for project in "$reference_dir" "$candidate_dir"; do
    jq 'del(.require["fixture/app"]) | .require["fixture/base"] = "^1.0"' \
        "$project/composer.json" > "$project/composer.json.tmp"
    mv "$project/composer.json.tmp" "$project/composer.json"
done
run_pair dependency-removal update
if [[ -e "$reference_dir/vendor/fixture/app" || -e "$candidate_dir/vendor/fixture/app" ]]; then
    record_failure dependency-removal "removed dependency remains in vendor"
else
    printf 'PASS %-32s stale package removed\n' dependency-removal-vendor
fi
compare_json_projection removal-lock \
    "$reference_dir/composer.lock" "$candidate_dir/composer.lock" "$lock_projection"

prepare_pair update-flags
for root in "$reference_root" "$candidate_root"; do
    jq '.require["fixture/dev"] = "^1.0"' "$root/packages/app/composer.json" \
        > "$root/packages/app/composer.json.tmp"
    mv "$root/packages/app/composer.json.tmp" "$root/packages/app/composer.json"
done
run_pair update-flags-initial update --no-scripts
for root in "$reference_root" "$candidate_root"; do
    for package in app base dev; do
        jq '.version = "1.1.0"' "$root/packages/$package/composer.json" \
            > "$root/packages/$package/composer.json.tmp"
        mv "$root/packages/$package/composer.json.tmp" "$root/packages/$package/composer.json"
    done
done
run_pair update-target-only update fixture/app --no-scripts
compare_json_projection update-target-only-lock \
    "$reference_dir/composer.lock" "$candidate_dir/composer.lock" "$lock_projection"
[[ $(jq -r '.packages[] | select(.name == "fixture/base") | .version' "$candidate_dir/composer.lock") == '1.0.0' \
    && $(jq -r '[.packages[], .["packages-dev"][]] | map(select(.name == "fixture/dev"))[0].version' "$candidate_dir/composer.lock") == '1.0.0' ]] \
    || record_failure update-target-only-lock "an unselected dependency changed"

run_pair update-with-dependencies update fixture/app --with-dependencies --no-scripts
compare_json_projection update-with-dependencies-lock \
    "$reference_dir/composer.lock" "$candidate_dir/composer.lock" "$lock_projection"
[[ $(jq -r '.packages[] | select(.name == "fixture/base") | .version' "$candidate_dir/composer.lock") == '1.1.0' \
    && $(jq -r '[.packages[], .["packages-dev"][]] | map(select(.name == "fixture/dev"))[0].version' "$candidate_dir/composer.lock") == '1.0.0' ]] \
    || record_failure update-with-dependencies-lock "dependency/root requirement selection differs"

run_pair update-with-all-dependencies update fixture/app --with-all-dependencies --no-scripts
compare_json_projection update-with-all-dependencies-lock \
    "$reference_dir/composer.lock" "$candidate_dir/composer.lock" "$lock_projection"
[[ $(jq -r '[.packages[], .["packages-dev"][]] | map(select(.name == "fixture/dev"))[0].version' "$candidate_dir/composer.lock") == '1.1.0' ]] \
    || record_failure update-with-all-dependencies-lock "root requirement was not updated"

prepare_pair no-scripts
run_pair update-no-scripts update --no-scripts
if [[ -e "$reference_dir/events.log" || -e "$candidate_dir/events.log" ]]; then
    record_failure update-no-scripts "a lifecycle script ran"
else
    printf 'PASS %-32s lifecycle hooks omitted\n' update-no-scripts-events
fi

prepare_pair dry-run
run_pair update-dry-run update --dry-run --no-scripts
if [[ -e "$reference_dir/composer.lock" || -e "$candidate_dir/composer.lock" \
    || -e "$reference_dir/vendor" || -e "$candidate_dir/vendor" ]]; then
    record_failure update-dry-run "dry run changed project files"
else
    printf 'PASS %-32s filesystem unchanged\n' update-dry-run-files
fi

if [[ $failures -ne 0 ]]; then
    printf '%s core differential check(s) failed\n' "$failures" >&2
    exit 1
fi

printf 'All core Composer differential cases passed.\n'
