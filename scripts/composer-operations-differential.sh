#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
composer_dir=${COMPOSER_SRC_DIR:-$(cd "$repo_root/.." && pwd)/composer}
php_bin=${PHP_BIN:-$(command -v php || true)}
composer_rs_bin=${COMPOSER_RS_BIN:-$repo_root/target/debug/composer-rs}
export COMPOSER_RS_PHP="$php_bin"

if [[ ! -x "$php_bin" || ! -x "$composer_rs_bin" || ! -f "$composer_dir/vendor/autoload.php" ]]; then
    echo "Build PHP, composer-rs, and the Composer reference before running operation differential tests." >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "jq is required for operation differential tests." >&2
    exit 1
fi

tmp_dir=$(mktemp -d)
if [[ ${KEEP_TMP:-0} == 1 ]]; then
    printf 'Keeping operation differential workspace at %s\n' "$tmp_dir"
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
        "$root/project/scripts"

    printf '%s\n' '{"name":"fixture/base","version":"1.0.0"}' \
        > "$root/packages/base/composer.json"
    printf '%s\n' '<?php' > "$root/packages/base/src/Base.php"

    printf '%s\n' '{"name":"fixture/app","version":"1.2.3","require":{"fixture/base":"^1.0"}}' \
        > "$root/packages/app/composer.json"
    printf '%s\n' '<?php' > "$root/packages/app/src/App.php"

    printf '%s\n' '{"name":"fixture/tool","version":"0.1.3"}' \
        > "$root/packages/tool/composer.json"
    printf '%s\n' '<?php' > "$root/packages/tool/src/Tool.php"

    printf '%s\n' '{"name":"fixture/dev","version":"1.0.0"}' \
        > "$root/packages/dev/composer.json"
    printf '%s\n' '<?php' > "$root/packages/dev/src/Dev.php"

    printf '%s\n' '<?php file_put_contents("script-args.json", json_encode(array_slice($argv, 1)));' \
        > "$root/project/scripts/args.php"

    jq -n '{
        name: "fixture/root",
        require: {"fixture/app": "^1.0", "fixture/tool": "^0.1.3"},
        "require-dev": {"fixture/dev": "^1.0"},
        repositories: [
            {type: "path", url: "../packages/*", options: {symlink: false}},
            {"packagist.org": false}
        ],
        scripts: {
            "capture-args": "@php scripts/args.php"
        },
        "scripts-descriptions": {
            "capture-args": "Capture forwarded arguments"
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
    local candidate_output=$tmp_dir/$case_name-composer-rs.out

    set +e
    COMPOSER_HOME="$tmp_dir/home-reference" "$php_bin" "$composer_dir/bin/composer" \
        "$command" --no-ansi --no-interaction -d "$reference_dir" "$@" \
        > "$reference_output" 2> "$tmp_dir/$case_name-reference.err"
    reference_code=$?
    COMPOSER_HOME="$tmp_dir/home-candidate" "$composer_rs_bin" "$command" -d "$candidate_dir" "$@" \
        > "$candidate_output" 2> "$tmp_dir/$case_name-composer-rs.err"
    candidate_code=$?
    set -e

    if [[ $reference_code -ne 0 || $candidate_code -ne 0 ]]; then
        printf '%s\n' '--- Composer output ---' >&2
        cat "$reference_output" "$tmp_dir/$case_name-reference.err" >&2
        printf '%s\n' '--- composer-rs output ---' >&2
        cat "$candidate_output" "$tmp_dir/$case_name-composer-rs.err" >&2
        fail "$case_name" "exit code: Composer=$reference_code composer-rs=$candidate_code"
    fi
    printf 'PASS %-32s exit=0\n' "$case_name"
}

run_pair initial-update update --no-progress --no-audit --no-scripts

run_pair run-arguments run capture-args -- 'two words' 'semi;colon' "quote's value"
if ! cmp -s "$reference_dir/script-args.json" "$candidate_dir/script-args.json"; then
    fail run-arguments-file "forwarded arguments differ"
fi
[[ $(cat "$candidate_dir/script-args.json") == '["two words","semi;colon","quote'"'"'s value"]' ]] \
    || fail run-arguments-boundaries "argument boundaries were not preserved"
printf 'PASS %-32s shell-safe arguments\n' run-arguments-boundaries

run_pair show-names show --name-only
sort "$tmp_dir/show-names-reference.out" > "$tmp_dir/show-names-reference.sorted"
sort "$tmp_dir/show-names-composer-rs.out" > "$tmp_dir/show-names-composer-rs.sorted"
diff -u "$tmp_dir/show-names-reference.sorted" "$tmp_dir/show-names-composer-rs.sorted" \
    || fail show-names-output "installed names differ"
printf 'PASS %-32s installed names\n' show-names-output

run_pair why-transitive why fixture/base
for output in "$tmp_dir/why-transitive-reference.out" "$tmp_dir/why-transitive-composer-rs.out"; do
    grep -q 'fixture/app' "$output" || fail why-transitive-output "fixture/app missing"
    grep -q '\^1.0' "$output" || fail why-transitive-output "constraint missing"
done
printf 'PASS %-32s dependent and constraint\n' why-transitive-output

for project in "$reference_dir" "$candidate_dir"; do
    mv "$project/vendor" "$project/vendor.saved"
done
run_pair show-locked show --locked --name-only
sort "$tmp_dir/show-locked-reference.out" > "$tmp_dir/show-locked-reference.sorted"
sort "$tmp_dir/show-locked-composer-rs.out" > "$tmp_dir/show-locked-composer-rs.sorted"
diff -u "$tmp_dir/show-locked-reference.sorted" "$tmp_dir/show-locked-composer-rs.sorted" \
    || fail show-locked-output "locked names differ"
printf 'PASS %-32s lock works without vendor\n' show-locked-output

run_pair why-locked why --locked fixture/base
for output in "$tmp_dir/why-locked-reference.out" "$tmp_dir/why-locked-composer-rs.out"; do
    grep -q 'fixture/app' "$output" || fail why-locked-output "fixture/app missing"
done
printf 'PASS %-32s lock graph works without vendor\n' why-locked-output
for project in "$reference_dir" "$candidate_dir"; do
    mv "$project/vendor.saved" "$project/vendor"
done

for root in "$reference_root" "$candidate_root"; do
    jq '.version = "1.3.0"' "$root/packages/app/composer.json" \
        > "$root/packages/app/composer.json.tmp"
    mv "$root/packages/app/composer.json.tmp" "$root/packages/app/composer.json"
    jq '.version = "0.2.0"' "$root/packages/tool/composer.json" \
        > "$root/packages/tool/composer.json.tmp"
    mv "$root/packages/tool/composer.json.tmp" "$root/packages/tool/composer.json"
done

run_pair outdated-json outdated --format=json
jq -S '.installed | sort_by(.name) | map({name, version, latest, status: .["latest-status"]})' \
    "$tmp_dir/outdated-json-reference.out" > "$tmp_dir/outdated-json-reference.projected"
jq -S '.installed | sort_by(.name) | map({name, version, latest, status: .["latest-status"]})' \
    "$tmp_dir/outdated-json-composer-rs.out" > "$tmp_dir/outdated-json-composer-rs.projected"
diff -u "$tmp_dir/outdated-json-reference.projected" "$tmp_dir/outdated-json-composer-rs.projected" \
    || fail outdated-json-output "outdated package data differs"
printf 'PASS %-32s versions and compatibility\n' outdated-json-output

set +e
COMPOSER_HOME="$tmp_dir/home-reference" "$php_bin" "$composer_dir/bin/composer" outdated \
    --no-ansi --no-interaction --strict -d "$reference_dir" >/dev/null 2>&1
reference_code=$?
COMPOSER_HOME="$tmp_dir/home-candidate" "$composer_rs_bin" outdated --strict -d "$candidate_dir" >/dev/null 2>&1
candidate_code=$?
set -e
[[ $reference_code -eq 1 && $candidate_code -eq 1 ]] \
    || fail outdated-strict "expected exit 1, Composer=$reference_code composer-rs=$candidate_code"
printf 'PASS %-32s exit=1\n' outdated-strict

run_pair outdated-major outdated --major-only --format=json
[[ $(jq -r '.installed | map(.name) | join(",")' "$tmp_dir/outdated-major-reference.out") == 'fixture/tool' \
    && $(jq -r '.installed | map(.name) | join(",")' "$tmp_dir/outdated-major-composer-rs.out") == 'fixture/tool' ]] \
    || fail outdated-major-output "major filter differs"
printf 'PASS %-32s fixture/tool\n' outdated-major-output

run_pair outdated-ignore outdated --ignore fixture/tool --format=json
[[ $(jq -r '.installed | map(.name) | join(",")' "$tmp_dir/outdated-ignore-reference.out") == 'fixture/app' \
    && $(jq -r '.installed | map(.name) | join(",")' "$tmp_dir/outdated-ignore-composer-rs.out") == 'fixture/app' ]] \
    || fail outdated-ignore-output "ignore filter differs"
printf 'PASS %-32s fixture/app\n' outdated-ignore-output

printf 'All Composer operational differential cases passed.\n'
