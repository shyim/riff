#!/usr/bin/env bash
set -euo pipefail

project_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
registry="$project_root/crates/riff/tests/fixtures/composer/php-ported.tsv"
excluded_registry="$project_root/crates/riff/tests/fixtures/composer/php-non-portable-files.tsv"
delegated_registry="$project_root/crates/riff/tests/fixtures/composer/php-delegated.tsv"
fixture_ported="$project_root/crates/riff/tests/fixtures/composer/ported.txt"
composer_root=${COMPOSER_SRC_DIR:-$project_root/shopware/composer}

run_registered_test() {
    package=$1
    filter=$2
    local_source=$3
    case "$local_source" in
        crates/"$package"/tests/*.rs)
            target=${local_source##*/}
            target=${target%.rs}
            cargo test -p "$package" --test "$target" "$filter" -- --exact
            ;;
        *) cargo test -p "$package" --lib "$filter" -- --exact ;;
    esac
}

usage() {
    printf 'Usage: %s [--list | --pending | --excluded | --delegated | --run UPSTREAM-SYMBOL | --run-group UPSTREAM-PREFIX]\n' "$0" >&2
    exit 2
}

mode=inventory
query=
case ${1:-} in
    '') ;;
    --list) mode=list ;;
    --pending) mode=pending ;;
    --excluded) mode=excluded ;;
    --delegated) mode=delegated ;;
    --run)
        test "$#" -eq 2 || usage
        mode=run
        query=$2
        ;;
    --run-group)
        test "$#" -eq 2 || usage
        mode=run-group
        query=$2
        ;;
    *) usage ;;
esac

test -f "$registry" || { printf 'Missing registry: %s\n' "$registry" >&2; exit 1; }
test -f "$excluded_registry" || { printf 'Missing registry: %s\n' "$excluded_registry" >&2; exit 1; }
test -f "$delegated_registry" || { printf 'Missing registry: %s\n' "$delegated_registry" >&2; exit 1; }
test -f "$fixture_ported" || { printf 'Missing fixture registry: %s\n' "$fixture_ported" >&2; exit 1; }
test -d "$composer_root/tests/Composer/Test" || {
    printf 'Missing Composer checkout: %s\n' "$composer_root" >&2
    exit 1
}

temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT
sed -e '/^[[:space:]]*#/d' -e '/^[[:space:]]*$/d' "$registry" > "$temporary/registry"
sed -e '/^[[:space:]]*#/d' -e '/^[[:space:]]*$/d' "$excluded_registry" > "$temporary/excluded-files"
sed -e '/^[[:space:]]*#/d' -e '/^[[:space:]]*$/d' "$delegated_registry" > "$temporary/delegated-registry"
find "$composer_root/tests/Composer/Test" -type f -name '*Test.php' -print0 \
    | while IFS= read -r -d '' file; do
        relative=${file#"$composer_root/tests/Composer/Test/"}
        grep -Eo 'public function test[A-Za-z0-9_]+' "$file" \
            | sed "s#public function #$relative::#"
    done \
    | sort > "$temporary/upstream"
cut -f1 "$temporary/registry" | sort > "$temporary/mapped"
: > "$temporary/excluded"
while IFS=$'\t' read -r upstream_symbol reason extra; do
    if test -n "${extra:-}" || test -z "$upstream_symbol" || test -z "$reason"; then
        printf 'Malformed non-portable registry row: %s\n' "$upstream_symbol" >&2
        exit 1
    fi
    upstream_file=${upstream_symbol%%::*}
    if test ! -f "$composer_root/tests/Composer/Test/$upstream_file"; then
        printf 'Missing excluded upstream file: %s\n' "$upstream_file" >&2
        exit 1
    fi
    if test "$upstream_symbol" != "$upstream_file"; then
        if ! grep -Fqx "$upstream_symbol" "$temporary/upstream"; then
            printf 'Missing excluded upstream method: %s\n' "$upstream_symbol" >&2
            exit 1
        fi
        printf '%s\n' "$upstream_symbol" >> "$temporary/excluded"
    else
        awk -v prefix="$upstream_file::" 'index($0, prefix) == 1 {print}' "$temporary/upstream" \
            >> "$temporary/excluded"
    fi
done < "$temporary/excluded-files"
sort -u -o "$temporary/excluded" "$temporary/excluded"
cut -f1 "$temporary/delegated-registry" | sort -u > "$temporary/delegated"
excluded_duplicates=$(cut -f1 "$temporary/excluded-files" | sort | uniq -d)
if test -n "$excluded_duplicates"; then
    printf 'Duplicate non-portable entries:\n%s\n' "$excluded_duplicates" >&2
    exit 1
fi
overlap=$(comm -12 "$temporary/mapped" "$temporary/excluded")
if test -n "$overlap"; then
    printf 'Methods cannot be both mapped and excluded:\n%s\n' "$overlap" >&2
    exit 1
fi
for left in mapped excluded; do
    overlap=$(comm -12 "$temporary/$left" "$temporary/delegated")
    if test -n "$overlap"; then
        printf 'Methods cannot be both delegated and %s:\n%s\n' "$left" "$overlap" >&2
        exit 1
    fi
done
sort -u "$temporary/mapped" "$temporary/excluded" "$temporary/delegated" > "$temporary/reviewed"

if test "$mode" = list; then
    cut -f1 "$temporary/registry"
    exit 0
fi

if test "$mode" = pending; then
    comm -23 "$temporary/upstream" "$temporary/reviewed"
    exit 0
fi

if test "$mode" = excluded; then
    cat "$temporary/excluded-files"
    exit 0
fi

if test "$mode" = delegated; then
    cat "$delegated_registry"
    exit 0
fi

if test "$mode" = run; then
    awk -F '\t' -v query="$query" '$1 == query {print}' \
        "$temporary/registry" > "$temporary/matches"
    if test ! -s "$temporary/matches"; then
        awk -F '\t' -v query="$query" 'index($1, query) > 0 {print}' \
            "$temporary/registry" > "$temporary/matches"
    fi
    count=$(wc -l < "$temporary/matches")
    if test "$count" -ne 1; then
        printf 'Expected one registered test matching %q, found %s:\n' "$query" "$count" >&2
        cut -f1 "$temporary/matches" >&2
        exit 1
    fi
    IFS=$'\t' read -r upstream package filter local_source < "$temporary/matches"
    printf 'Composer contract: %s -> %s\n' "$upstream" "$filter"
    run_registered_test "$package" "$filter" "$local_source"
    exit
fi

if test "$mode" = run-group; then
    awk -F '\t' -v query="$query" 'index($1, query) == 1 {print}' \
        "$temporary/registry" > "$temporary/matches"
    if test ! -s "$temporary/matches"; then
        awk -F '\t' -v query="$query" 'index($1, query) > 0 {print}' \
            "$temporary/registry" > "$temporary/matches"
    fi
    count=$(wc -l < "$temporary/matches")
    if test "$count" -eq 0; then
        printf 'No registered tests match %q\n' "$query" >&2
        exit 1
    fi
    printf 'Running %s Composer contracts matching %s\n' "$count" "$query"
    status=0
    while IFS=$'\t' read -r upstream package filter local_source; do
        printf 'Composer contract: %s -> %s\n' "$upstream" "$filter"
        run_registered_test "$package" "$filter" "$local_source" || status=1
    done < "$temporary/matches"
    exit "$status"
fi

total_methods=$(wc -l < "$temporary/upstream")
ported=$(wc -l < "$temporary/registry")
excluded=$(wc -l < "$temporary/excluded")
delegated=$(wc -l < "$temporary/delegated")
pending=$((total_methods - ported - excluded - delegated))
errors=0

duplicates=$(cut -f1 "$temporary/registry" | sort | uniq -d)
if test -n "$duplicates"; then
    printf 'Duplicate upstream mappings:\n%s\n' "$duplicates" >&2
    errors=1
fi

delegated_duplicates=$(cut -f1 "$temporary/delegated-registry" | sort | uniq -d)
if test -n "$delegated_duplicates"; then
    printf 'Duplicate delegated entries:\n%s\n' "$delegated_duplicates" >&2
    errors=1
fi

while IFS=$'\t' read -r upstream suite reason extra; do
    if test -n "${extra:-}" || test -z "$upstream" || test -z "$suite" || test -z "$reason"; then
        printf 'Malformed delegated registry row: %s\n' "$upstream" >&2
        errors=1
        continue
    fi
    if ! grep -Fqx "$upstream" "$temporary/upstream"; then
        printf 'Missing delegated upstream method: %s\n' "$upstream" >&2
        errors=1
    fi
    upstream_suite="$composer_root/tests/Composer/Test/Fixtures/$suite"
    local_suite="$project_root/crates/riff/tests/fixtures/composer/$suite"
    if test ! -d "$upstream_suite" || test ! -d "$local_suite"; then
        printf 'Missing delegated fixture suite: %s\n' "$suite" >&2
        errors=1
        continue
    fi
    while IFS= read -r fixture; do
        relative=${fixture#"$composer_root/tests/Composer/Test/Fixtures/"}
        if test ! -f "$project_root/crates/riff/tests/fixtures/composer/$relative" \
            || ! grep -Fqx "$relative" "$fixture_ported"; then
            printf 'Delegated fixture is not copied and ported: %s\n' "$relative" >&2
            errors=1
        fi
    done < <(find "$upstream_suite" -type f -name '*.test' | sort)
done < "$temporary/delegated-registry"

while IFS=$'\t' read -r upstream package filter local_source extra; do
    if test -n "${extra:-}" || test -z "$upstream" || test -z "$package" \
        || test -z "$filter" || test -z "$local_source"; then
        printf 'Malformed registry row: %s\n' "$upstream" >&2
        errors=1
        continue
    fi
    upstream_file=${upstream%%::*}
    upstream_method=${upstream##*::}
    if ! grep -Eq "function ${upstream_method}\\(" \
        "$composer_root/tests/Composer/Test/$upstream_file"; then
        printf 'Missing upstream method: %s\n' "$upstream" >&2
        errors=1
    fi
    local_function=${filter##*::}
    if test ! -f "$project_root/$local_source" \
        || ! grep -Eq "fn ${local_function}\\(" "$project_root/$local_source"; then
        printf 'Missing local test: %s (%s)\n' "$filter" "$local_source" >&2
        errors=1
    fi
done < "$temporary/registry"

printf 'Composer PHP contract inventory\n'
printf '  upstream test methods: %s\n' "$total_methods"
printf '  explicitly mapped:     %s\n' "$ported"
printf '  reviewed non-portable: %s\n' "$excluded"
printf '  covered by fixtures:   %s\n' "$delegated"
printf '  pending review:        %s\n' "$pending"

exit "$errors"
