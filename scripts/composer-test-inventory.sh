#!/usr/bin/env bash
set -euo pipefail

project_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
fixture_root="$project_root/crates/riff/tests/fixtures/composer"
ported_file="$fixture_root/ported.txt"
composer_root=${COMPOSER_SRC_DIR:-$project_root/shopware/composer}
mode=${1:-inventory}
if test "$mode" != inventory && test "$mode" != --pending && test "$mode" != --ported; then
    printf 'Usage: %s [--pending | --ported]\n' "$0" >&2
    exit 2
fi

temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT

find "$fixture_root" -type f -name '*.test' -printf '%P\n' | sort > "$temporary/local"
sed -e 's/#.*//' -e '/^[[:space:]]*$/d' "$ported_file" | sort > "$temporary/ported"

duplicates=$(uniq -d "$temporary/ported")
if test -n "$duplicates"; then
    printf 'Duplicate ported fixture entries:\n%s\n' "$duplicates" >&2
    exit 1
fi
unknown=$(comm -23 "$temporary/ported" "$temporary/local")
if test -n "$unknown"; then
    printf 'Ported fixture entries without a local fixture:\n%s\n' "$unknown" >&2
    exit 1
fi

if test "$mode" = --pending; then
    comm -23 "$temporary/local" "$temporary/ported"
    exit 0
fi
if test "$mode" = --ported; then
    cat "$temporary/ported"
    exit 0
fi

total=$(wc -l < "$temporary/local")
ported=$(wc -l < "$temporary/ported")
pending=$((total - ported))

printf 'Composer fixture inventory\n'
printf '  copied:  %s\n' "$total"
printf '  ported:  %s\n' "$ported"
printf '  pending: %s\n' "$pending"

if test ! -d "$composer_root/tests/Composer/Test"; then
    printf '  upstream comparison skipped: %s is not a Composer checkout\n' "$composer_root"
    exit 0
fi

for family in installer installer-slow; do
    find "$composer_root/tests/Composer/Test/Fixtures/$family" -type f -name '*.test' \
        -printf "$family/%P\n"
done | sort > "$temporary/upstream"

missing=$(comm -23 "$temporary/upstream" "$temporary/local" | wc -l)
extra=$(comm -13 "$temporary/upstream" "$temporary/local" | wc -l)
php_tests=$(find "$composer_root/tests/Composer/Test" -type f -name '*Test.php' | wc -l)
functional=$(find "$composer_root/tests/Composer/Test/Fixtures/functional" -type f -name '*.test' | wc -l)

printf '  upstream installer fixtures: %s\n' "$(wc -l < "$temporary/upstream")"
printf '  missing local copies:         %s\n' "$missing"
printf '  extra local copies:           %s\n' "$extra"
printf '  upstream functional fixtures: %s\n' "$functional"
printf '  upstream PHP test files:      %s\n' "$php_tests"

if test "$missing" -ne 0; then
    printf '\nMissing fixture copies:\n'
    comm -23 "$temporary/upstream" "$temporary/local"
fi
if test "$extra" -ne 0; then
    printf '\nLocal fixtures absent from the upstream snapshot:\n'
    comm -13 "$temporary/upstream" "$temporary/local"
fi
if test "$missing" -ne 0 || test "$extra" -ne 0; then
    exit 1
fi
