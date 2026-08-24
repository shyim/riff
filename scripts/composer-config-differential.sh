#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
composer_dir=${COMPOSER_SRC_DIR:-$(cd "$repo_root/.." && pwd)/composer}
php_bin=${PHP_BIN:-$(command -v php || true)}
composer_rs_bin=${COMPOSER_RS_BIN:-$repo_root/target/debug/composer-rs}
export COMPOSER_RS_PHP="$php_bin"
read_fixture=$repo_root/tests/composer-parity/config/read/composer.json

if [[ ! -x "$php_bin" ]]; then
    echo "Missing PHP binary at $php_bin; run 'make php' first." >&2
    exit 1
fi
if [[ ! -x "$composer_rs_bin" ]]; then
    echo "Missing composer-rs binary at $composer_rs_bin; run 'make build' first." >&2
    exit 1
fi
if [[ ! -f "$composer_dir/bin/composer" || ! -f "$composer_dir/vendor/autoload.php" ]]; then
    echo "Composer reference checkout is not bootstrapped at $composer_dir." >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "jq is required for config differential tests." >&2
    exit 1
fi

tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT
home=$tmp_dir/home
mkdir -p "$home"
failures=0

normalize_output() {
    local input=$1
    local reference_dir=$2
    local candidate_dir=$3

    sed -E $'s/\x1B\[[0-9;]*[[:alpha:]]//g; s/\r$//' "$input" \
        | sed -e "s#$reference_dir#<project>#g" \
              -e "s#$candidate_dir#<project>#g" \
              -e "s#$home#<home>#g"
}

prepare_case() {
    local case_name=$1
    local manifest=$2
    reference_dir=$tmp_dir/$case_name/reference
    candidate_dir=$tmp_dir/$case_name/candidate
    mkdir -p "$reference_dir" "$candidate_dir"
    printf '%s\n' "$manifest" > "$reference_dir/composer.json"
    cp "$reference_dir/composer.json" "$candidate_dir/composer.json"
}

compare_outputs() {
    local case_name=$1
    local reference_code=$2
    local candidate_code=$3
    local reference_output=$4
    local candidate_output=$5
    local reference_normalized=$tmp_dir/$case_name-reference.normalized
    local candidate_normalized=$tmp_dir/$case_name-composer-rs.normalized

    normalize_output "$reference_output" "$reference_dir" "$candidate_dir" > "$reference_normalized"
    normalize_output "$candidate_output" "$reference_dir" "$candidate_dir" > "$candidate_normalized"

    if [[ $reference_code -ne $candidate_code ]]; then
        printf 'FAIL %-30s exit code: Composer=%s composer-rs=%s\n' \
            "$case_name" "$reference_code" "$candidate_code" >&2
        failures=$((failures + 1))
        return 1
    fi
    if ! diff -u "$reference_normalized" "$candidate_normalized"; then
        printf 'FAIL %-30s output differs\n' "$case_name" >&2
        failures=$((failures + 1))
        return 1
    fi
    return 0
}

run_read() {
    local case_name=$1
    local manifest=$2
    shift 2
    prepare_case "$case_name" "$manifest"
    local reference_output=$tmp_dir/$case_name-reference.out
    local candidate_output=$tmp_dir/$case_name-composer-rs.out
    local reference_code candidate_code

    set +e
    COMPOSER_HOME="$home" "$php_bin" "$composer_dir/bin/composer" \
        config --no-ansi -d "$reference_dir" "$@" > "$reference_output" 2>&1
    reference_code=$?
    COMPOSER_HOME="$home" "$composer_rs_bin" config -d "$candidate_dir" "$@" > "$candidate_output" 2>&1
    candidate_code=$?
    set -e

    if compare_outputs "$case_name" "$reference_code" "$candidate_code" "$reference_output" "$candidate_output"; then
        printf 'PASS %-30s exit=%s\n' "$case_name" "$candidate_code"
    fi
}

compare_json_file() {
    local case_name=$1
    local relative_path=$2
    local reference_path=$reference_dir/$relative_path
    local candidate_path=$candidate_dir/$relative_path

    if [[ ! -f "$reference_path" || ! -f "$candidate_path" ]]; then
        if [[ -f "$reference_path" || -f "$candidate_path" ]]; then
            printf 'FAIL %-30s %s presence differs\n' "$case_name" "$relative_path" >&2
            failures=$((failures + 1))
            return 1
        fi
        return 0
    fi

    jq -S . "$reference_path" > "$tmp_dir/$case_name-reference-${relative_path//\//-}"
    jq -S . "$candidate_path" > "$tmp_dir/$case_name-composer-rs-${relative_path//\//-}"
    if ! diff -u \
        "$tmp_dir/$case_name-reference-${relative_path//\//-}" \
        "$tmp_dir/$case_name-composer-rs-${relative_path//\//-}"; then
        printf 'FAIL %-30s %s differs\n' "$case_name" "$relative_path" >&2
        failures=$((failures + 1))
        return 1
    fi
    return 0
}

run_write() {
    local case_name=$1
    local manifest=$2
    shift 2
    prepare_case "$case_name" "$manifest"
    local reference_output=$tmp_dir/$case_name-reference.out
    local candidate_output=$tmp_dir/$case_name-composer-rs.out
    local reference_code candidate_code

    set +e
    COMPOSER_HOME="$home" "$php_bin" "$composer_dir/bin/composer" \
        config --no-ansi -d "$reference_dir" "$@" > "$reference_output" 2>&1
    reference_code=$?
    COMPOSER_HOME="$home" "$composer_rs_bin" config -d "$candidate_dir" "$@" > "$candidate_output" 2>&1
    candidate_code=$?
    set -e

    local passed=true
    compare_outputs "$case_name" "$reference_code" "$candidate_code" "$reference_output" "$candidate_output" || passed=false
    compare_json_file "$case_name" composer.json || passed=false
    compare_json_file "$case_name" auth.json || passed=false
    if [[ $passed == true ]]; then
        printf 'PASS %-30s exit=%s\n' "$case_name" "$candidate_code"
    fi
}

run_rejected() {
    local case_name=$1
    local expected_message=$2
    local manifest=$3
    shift 3
    prepare_case "$case_name" "$manifest"
    local reference_output=$tmp_dir/$case_name-reference.out
    local candidate_output=$tmp_dir/$case_name-composer-rs.out
    local reference_code candidate_code

    set +e
    COMPOSER_HOME="$home" "$php_bin" "$composer_dir/bin/composer" \
        config --no-ansi -d "$reference_dir" "$@" > "$reference_output" 2>&1
    reference_code=$?
    COMPOSER_HOME="$home" "$composer_rs_bin" config -d "$candidate_dir" "$@" > "$candidate_output" 2>&1
    candidate_code=$?
    set -e

    local passed=true
    if [[ $reference_code -eq 0 || $candidate_code -ne $reference_code ]]; then
        printf 'FAIL %-30s exit code: Composer=%s composer-rs=%s\n' \
            "$case_name" "$reference_code" "$candidate_code" >&2
        failures=$((failures + 1))
        passed=false
    fi
    if ! grep -Fq -- "$expected_message" "$reference_output" \
        || ! grep -Fq -- "$expected_message" "$candidate_output"; then
        printf 'FAIL %-30s expected error text %q\n' "$case_name" "$expected_message" >&2
        failures=$((failures + 1))
        passed=false
    fi
    compare_json_file "$case_name" composer.json || passed=false
    compare_json_file "$case_name" auth.json || passed=false
    if [[ $passed == true ]]; then
        printf 'PASS %-30s exit=%s\n' "$case_name" "$candidate_code"
    fi
}

read_manifest=$(<"$read_fixture")
run_read read-description "$read_manifest" description
run_read read-source "$read_manifest" vendor-dir --source
run_read read-default '{}' vendor-dir
run_read read-absolute "$read_manifest" vendor-dir --absolute
run_read read-repository "$read_manifest" repositories.example
run_read read-repositories "$read_manifest" repos
run_read list-defaults '{}' --list
run_read list-sources "$read_manifest" --list --source

run_write set-script '{}' scripts.test 'foo bar'
run_write set-script-list '{}' scripts.test first second
run_write set-boolean '{}' use-github-api 1
run_write set-multi '{}' github-protocols https git
run_write set-version '{}' version 1.2.3
run_write unset-property '{"random-prop":"value"}' random-prop --unset
run_write set-preferred '{}' 'preferred-install.foo/*' source
run_write unset-platform \
    '{"config":{"platform":{"php":"7.2.5"},"platform-check":false}}' \
    platform.php --unset
run_write merge-extra-object \
    '{"extra":{"patches":{"foo/bar":{"5":"old"}}}}' \
    extra.patches.foo/bar --json --merge '{"123":"new"}'
run_write merge-extra-list \
    '{"extra":{"items":["old"]}}' \
    extra.items --json --merge '["new"]'
run_write merge-audit-array \
    '{"config":{"audit":{"ignore":["CVE-old"]}}}' \
    audit.ignore --json --merge '["CVE-new"]'
run_write merge-audit-object \
    '{"config":{"audit":{"ignore":{"CVE-old":"old"}}}}' \
    audit.ignore --json --merge '{"CVE-new":"new"}'
run_write set-policy-block '{}' policy.advisories.block 0
run_write set-policy-custom '{}' policy.my-list.audit report
run_write set-policy-ignore '{}' policy.advisories.ignore --json '["CVE"]'
run_write set-policy-scope '{}' policy.ignore-unreachable update install
run_write set-policy-boolean '{}' policy.ignore-unreachable true
run_write unset-policy \
    '{"config":{"policy":{"advisories":{"block":false}}}}' \
    policy.advisories.block --unset
run_write add-repository '{}' repositories.example vcs https://example.org/repository.git
run_write append-repository \
    '{"repositories":{"first":{"type":"vcs","url":"https://first.example.org"}}}' \
    repositories.second vcs https://second.example.org --append
run_write disable-packagist '{}' repo.packagist.org false
run_write set-http-basic '{}' http-basic.repo.example.org alice secret
run_write set-github-token '{}' github-oauth.github.com token-value
run_write set-allow-plugin '{}' 'allow-plugins.example/*' true
run_write set-platform-false '{}' platform.ext-missing false
run_write disable-tls '{}' disable-tls true

run_rejected reject-unset-value 'You can not combine' '{}' process-timeout --unset 300
run_rejected reject-invalid-boolean 'invalid value' '{}' optimize-autoloader bogus
run_rejected reject-invalid-severity 'valid severities include' '{}' \
    audit.ignore-severity low bogus
run_rejected reject-merge-types 'Cannot merge array and object' \
    '{"config":{"audit":{"ignore":["CVE"]}}}' \
    audit.ignore --json --merge '{"CVE-2":"reason"}'
run_rejected reject-reserved-policy 'Invalid dependency policy name' '{}' \
    policy.ignore-custom true
run_rejected reject-policy-sources 'Setting dependency policy sources is not supported' '{}' \
    policy.advisories.sources value
run_rejected reject-global-property 'can not be set in the global config.json' '{}' \
    --global description value
run_rejected reject-global-file '--file and --global can not be combined' '{}' \
    --global --file composer.json vendor-dir

reference_dir=$tmp_dir/global/reference
candidate_dir=$tmp_dir/global/candidate
mkdir -p "$reference_dir" "$candidate_dir"
reference_output=$tmp_dir/global-reference.out
candidate_output=$tmp_dir/global-composer-rs.out
set +e
COMPOSER_HOME="$reference_dir" "$php_bin" "$composer_dir/bin/composer" \
    config --global --no-ansi vendor-dir global-vendor > "$reference_output" 2>&1
reference_code=$?
COMPOSER_HOME="$candidate_dir" "$composer_rs_bin" config --global vendor-dir global-vendor > "$candidate_output" 2>&1
candidate_code=$?
set -e
global_passed=true
compare_outputs global "$reference_code" "$candidate_code" "$reference_output" "$candidate_output" || global_passed=false
for file in config.json auth.json; do
    jq -S . "$reference_dir/$file" > "$tmp_dir/global-reference-$file"
    jq -S . "$candidate_dir/$file" > "$tmp_dir/global-composer-rs-$file"
    if ! diff -u "$tmp_dir/global-reference-$file" "$tmp_dir/global-composer-rs-$file"; then
        printf 'FAIL %-30s %s differs\n' global "$file" >&2
        failures=$((failures + 1))
        global_passed=false
    fi
done
if [[ $(stat -c '%a' "$reference_dir/config.json") != $(stat -c '%a' "$candidate_dir/config.json") ]]; then
    printf 'FAIL %-30s config.json permissions differ\n' global >&2
    failures=$((failures + 1))
    global_passed=false
fi
if [[ $global_passed == true ]]; then
    printf 'PASS %-30s exit=%s\n' global "$candidate_code"
fi

if [[ $failures -ne 0 ]]; then
    printf '%s config differential check(s) failed\n' "$failures" >&2
    exit 1
fi

printf 'All Composer config differential cases passed.\n'
