#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
composer_dir=${COMPOSER_SRC_DIR:-$(cd "$repo_root/.." && pwd)/composer}
php_bin=${PHP_BIN:-$(command -v php || true)}
composer_rs_bin=${COMPOSER_RS_BIN:-$repo_root/target/debug/sonata}
export COMPOSER_RS_PHP="$php_bin"
fixtures=$repo_root/tests/composer-parity/validate

if [[ ! -x "$php_bin" ]]; then
    echo "Missing PHP binary at $php_bin; run 'make php' first." >&2
    exit 1
fi
if [[ ! -x "$composer_rs_bin" ]]; then
    echo "Missing sonata binary at $composer_rs_bin; run 'make build' first." >&2
    exit 1
fi
if [[ ! -f "$composer_dir/bin/composer" || ! -f "$composer_dir/vendor/autoload.php" ]]; then
    echo "Composer reference checkout is not bootstrapped at $composer_dir." >&2
    echo "Run: $composer_rs_bin install --no-scripts --no-audit -n -d $composer_dir" >&2
    exit 1
fi

tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT
failures=0

normalize() {
    local case_name=$1
    local input=$2

    if [[ "$case_name" == "invalid-json" ]]; then
        if grep -q 'does not contain valid JSON' "$input"; then
            printf '%s\n' '<invalid-json-error>'
            return
        fi
    fi

    sed -E $'s/\x1B\[[0-9;]*[[:alpha:]]//g; s/\r$//' "$input" \
        | sed '/^Composer could not detect the root package .* version, defaulting to/d'
}

run_case() {
    local case_name=$1
    shift
    local case_dir=$fixtures/$case_name
    run_directory_case "$case_name" "$case_dir" "$@"
}

run_directory_case() {
    local case_name=$1
    local case_dir=$2
    shift 2
    local reference_output=$tmp_dir/$case_name-reference.out
    local candidate_output=$tmp_dir/$case_name-sonata.out
    local reference_normalized=$tmp_dir/$case_name-reference.normalized
    local candidate_normalized=$tmp_dir/$case_name-sonata.normalized
    local reference_code candidate_code

    set +e
    (cd "$case_dir" && "$php_bin" "$composer_dir/bin/composer" validate "$@") \
        >"$reference_output" 2>&1
    reference_code=$?
    (cd "$case_dir" && "$composer_rs_bin" validate "$@") >"$candidate_output" 2>&1
    candidate_code=$?
    set -e

    normalize "$case_name" "$reference_output" >"$reference_normalized"
    normalize "$case_name" "$candidate_output" >"$candidate_normalized"

    if [[ $reference_code -ne $candidate_code ]]; then
        printf 'FAIL %-24s exit code: Composer=%s sonata=%s\n' \
            "$case_name $*" "$reference_code" "$candidate_code" >&2
        failures=$((failures + 1))
        return
    fi
    if ! diff -u "$reference_normalized" "$candidate_normalized"; then
        printf 'FAIL %s %s output differs\n' "$case_name" "$*" >&2
        failures=$((failures + 1))
        return
    fi

    printf 'PASS %-24s exit=%s\n' "$case_name $*" "$candidate_code"
}

run_case valid
run_case publish-errors
run_case publish-errors --no-check-publish
run_case version-warning
run_case version-warning --strict
run_case version-warning --no-check-version
run_case invalid-json
run_case stale-lock
run_case stale-lock --no-check-lock
run_case missing-requirement
run_case lock-disabled
run_case lock-disabled --check-lock
run_directory_case composer-tree "$composer_dir" \
    --with-dependencies --no-check-publish --no-check-version

if [[ $failures -ne 0 ]]; then
    printf '%s differential validation case(s) failed\n' "$failures" >&2
    exit 1
fi

printf 'All Composer validate differential cases passed.\n'
