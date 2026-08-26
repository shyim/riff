#!/usr/bin/env bash
set -euo pipefail

project_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
registry="$project_root/crates/riff/tests/fixtures/composer/functional-ported.tsv"
excluded_registry="$project_root/crates/riff/tests/fixtures/composer/functional-non-portable.tsv"
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
    printf 'Usage: %s [--list | --pending | --excluded | --run UPSTREAM-FIXTURE]\n' "$0" >&2
    exit 2
}

mode=inventory
query=
case ${1:-} in
    '') ;;
    --list) mode=list ;;
    --pending) mode=pending ;;
    --excluded) mode=excluded ;;
    --run)
        test "$#" -eq 2 || usage
        mode=run
        query=$2
        ;;
    *) usage ;;
esac

test -f "$registry" || { printf 'Missing registry: %s\n' "$registry" >&2; exit 1; }
test -f "$excluded_registry" || { printf 'Missing registry: %s\n' "$excluded_registry" >&2; exit 1; }
fixture_root="$composer_root/tests/Composer/Test/Fixtures/functional"
test -d "$fixture_root" || { printf 'Missing Composer functional fixtures: %s\n' "$fixture_root" >&2; exit 1; }

temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT
sed -e '/^[[:space:]]*#/d' -e '/^[[:space:]]*$/d' "$registry" > "$temporary/registry"
sed -e '/^[[:space:]]*#/d' -e '/^[[:space:]]*$/d' "$excluded_registry" > "$temporary/excluded"
find "$fixture_root" -type f -name '*.test' -printf '%f\n' | sort > "$temporary/upstream"
cut -f1 "$temporary/registry" | sort > "$temporary/mapped"
cut -f1 "$temporary/excluded" | sort > "$temporary/excluded-names"

for set_name in mapped excluded-names; do
    duplicates=$(uniq -d "$temporary/$set_name")
    if test -n "$duplicates"; then
        printf 'Duplicate functional fixture entries:\n%s\n' "$duplicates" >&2
        exit 1
    fi
    unknown=$(comm -23 "$temporary/$set_name" "$temporary/upstream")
    if test -n "$unknown"; then
        printf 'Unknown functional fixture entries:\n%s\n' "$unknown" >&2
        exit 1
    fi
done

overlap=$(comm -12 "$temporary/mapped" "$temporary/excluded-names")
if test -n "$overlap"; then
    printf 'Functional fixtures cannot be both mapped and excluded:\n%s\n' "$overlap" >&2
    exit 1
fi
sort -u "$temporary/mapped" "$temporary/excluded-names" > "$temporary/reviewed"

if test "$mode" = list; then
    cut -f1 "$temporary/registry"
    exit 0
fi
if test "$mode" = pending; then
    comm -23 "$temporary/upstream" "$temporary/reviewed"
    exit 0
fi
if test "$mode" = excluded; then
    cat "$temporary/excluded"
    exit 0
fi
if test "$mode" = run; then
    awk -F '\t' -v query="$query" '$1 == query {print}' "$temporary/registry" > "$temporary/matches"
    if test ! -s "$temporary/matches"; then
        awk -F '\t' -v query="$query" 'index($1, query) > 0 {print}' "$temporary/registry" > "$temporary/matches"
    fi
    count=$(wc -l < "$temporary/matches")
    if test "$count" -ne 1; then
        printf 'Expected one registered functional fixture matching %q, found %s:\n' "$query" "$count" >&2
        cut -f1 "$temporary/matches" >&2
        exit 1
    fi
    IFS=$'\t' read -r upstream package filter local_source < "$temporary/matches"
    printf 'Composer functional contract: %s -> %s\n' "$upstream" "$filter"
    run_registered_test "$package" "$filter" "$local_source"
    exit
fi

errors=0
while IFS=$'\t' read -r upstream package filter local_source extra; do
    if test -n "${extra:-}" || test -z "$upstream" || test -z "$package" \
        || test -z "$filter" || test -z "$local_source"; then
        printf 'Malformed functional registry row: %s\n' "$upstream" >&2
        errors=1
        continue
    fi
    local_function=${filter##*::}
    if test ! -f "$project_root/$local_source" \
        || ! grep -Eq "fn ${local_function}\\(" "$project_root/$local_source"; then
        printf 'Missing local functional test: %s (%s)\n' "$filter" "$local_source" >&2
        errors=1
    fi
done < "$temporary/registry"

while IFS=$'\t' read -r upstream reason extra; do
    if test -n "${extra:-}" || test -z "$upstream" || test -z "$reason"; then
        printf 'Malformed functional non-portable row: %s\n' "$upstream" >&2
        errors=1
    fi
done < "$temporary/excluded"

total=$(wc -l < "$temporary/upstream")
mapped=$(wc -l < "$temporary/mapped")
excluded=$(wc -l < "$temporary/excluded-names")
pending=$((total - mapped - excluded))
printf 'Composer functional fixture inventory\n'
printf '  upstream fixtures:      %s\n' "$total"
printf '  explicitly mapped:      %s\n' "$mapped"
printf '  reviewed non-portable:  %s\n' "$excluded"
printf '  pending review or port: %s\n' "$pending"
exit "$errors"
