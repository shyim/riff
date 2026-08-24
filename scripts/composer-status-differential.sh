#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
composer_dir=${COMPOSER_SRC_DIR:-$(cd "$repo_root/.." && pwd)/composer}
php_bin=${PHP_BIN:-$(command -v php || true)}
composer_rs_bin=${COMPOSER_RS_BIN:-$repo_root/target/debug/composer-rs}
export COMPOSER_RS_PHP="$php_bin"
script_fixture=$repo_root/tests/composer-parity/status/scripts

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
for command in git jq zip; do
    if ! command -v "$command" >/dev/null 2>&1; then
        echo "$command is required for status differential tests." >&2
        exit 1
    fi
done

tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT
mkdir -p "$tmp_dir/home/reference" "$tmp_dir/home/candidate"
failures=0

normalize_output() {
    local input=$1
    local reference_dir=$2
    local candidate_dir=$3

    sed -E $'s/\x1B\[[0-9;]*[[:alpha:]]//g; s/\r$//' "$input" \
        | sed -e "s#$reference_dir#<project>#g" \
              -e "s#$candidate_dir#<project>#g" \
              -e "s#$tmp_dir#<tmp>#g"
}

run_status() {
    local case_name=$1
    local reference_dir=$2
    local candidate_dir=$3
    shift 3
    local reference_output=$tmp_dir/$case_name-reference.out
    local candidate_output=$tmp_dir/$case_name-composer-rs.out
    local reference_normalized=$tmp_dir/$case_name-reference.normalized
    local candidate_normalized=$tmp_dir/$case_name-composer-rs.normalized
    local reference_code candidate_code

    set +e
    COMPOSER_HOME="$tmp_dir/home/reference" "$php_bin" "$composer_dir/bin/composer" \
        status --no-ansi -d "$reference_dir" "$@" > "$reference_output" 2>&1
    reference_code=$?
    COMPOSER_HOME="$tmp_dir/home/candidate" "$composer_rs_bin" \
        status -d "$candidate_dir" "$@" > "$candidate_output" 2>&1
    candidate_code=$?
    set -e

    normalize_output "$reference_output" "$reference_dir" "$candidate_dir" > "$reference_normalized"
    normalize_output "$candidate_output" "$reference_dir" "$candidate_dir" > "$candidate_normalized"

    if [[ $reference_code -ne $candidate_code ]]; then
        printf 'FAIL %-30s exit code: Composer=%s composer-rs=%s\n' \
            "$case_name" "$reference_code" "$candidate_code" >&2
        failures=$((failures + 1))
        return
    fi
    if ! diff -u "$reference_normalized" "$candidate_normalized"; then
        printf 'FAIL %-30s output differs\n' "$case_name" >&2
        failures=$((failures + 1))
        return
    fi

    printf 'PASS %-30s exit=%s\n' "$case_name" "$candidate_code"
}

prepare_project_pair() {
    local case_name=$1
    reference_dir=$tmp_dir/$case_name/reference
    candidate_dir=$tmp_dir/$case_name/candidate
    mkdir -p "$reference_dir/vendor/composer" "$candidate_dir/vendor/composer"
    printf '{}\n' > "$reference_dir/composer.json"
    cp "$reference_dir/composer.json" "$candidate_dir/composer.json"
}

write_dist_installed() {
    local project=$1
    local archive=$2
    jq -n --arg url "file://$archive" '{
        packages: [{
            name: "fixture/dist",
            version: "1.0.0",
            version_normalized: "1.0.0.0",
            type: "library",
            "installation-source": "dist",
            dist: {type: "zip", url: $url, reference: "dist-reference", shasum: ""},
            "install-path": "../fixture/dist"
        }],
        dev: true,
        "dev-package-names": []
    }' > "$project/vendor/composer/installed.json"
}

prepare_dist_pair() {
    local case_name=$1
    prepare_project_pair "$case_name"
    local source_dir=$tmp_dir/$case_name/archive-source
    local archive=$tmp_dir/$case_name/package.zip
    mkdir -p "$source_dir/package-root/src"
    printf '{"name":"fixture/dist","version":"1.0.0"}\n' \
        > "$source_dir/package-root/composer.json"
    printf 'baseline\n' > "$source_dir/package-root/src/file.txt"
    : > "$source_dir/package-root/empty.txt"
    (cd "$source_dir" && zip -qr "$archive" package-root)
    mkdir -p "$reference_dir/vendor/fixture" "$candidate_dir/vendor/fixture"
    cp -R "$source_dir/package-root" "$reference_dir/vendor/fixture/dist"
    cp -R "$source_dir/package-root" "$candidate_dir/vendor/fixture/dist"
    write_dist_installed "$reference_dir" "$archive"
    write_dist_installed "$candidate_dir" "$archive"
}

write_source_installed() {
    local project=$1
    local source_url=$2
    local reference=$3
    jq -n --arg url "$source_url" --arg reference "$reference" '{
        packages: [{
            name: "fixture/source",
            version: "1.0.0",
            version_normalized: "1.0.0.0",
            type: "library",
            "installation-source": "source",
            source: {type: "git", url: $url, reference: $reference},
            "install-path": "../fixture/source"
        }],
        dev: true,
        "dev-package-names": []
    }' > "$project/vendor/composer/installed.json"
}

create_git_remote() {
    local case_name=$1
    git_remote=$tmp_dir/$case_name/remote.git
    git_work=$tmp_dir/$case_name/remote-work
    mkdir -p "$git_work"
    git init -q -b main "$git_work"
    git -C "$git_work" config user.name 'Status Fixture'
    git -C "$git_work" config user.email 'status@example.org'
    printf '{"name":"fixture/source","version":"1.0.0"}\n' > "$git_work/composer.json"
    printf 'baseline\n' > "$git_work/tracked.txt"
    git -C "$git_work" add composer.json tracked.txt
    GIT_AUTHOR_DATE='2001-01-01T00:00:00Z' GIT_COMMITTER_DATE='2001-01-01T00:00:00Z' \
        git -C "$git_work" commit -qm baseline
    first_reference=$(git -C "$git_work" rev-parse HEAD)
    git init -q --bare "$git_remote"
    git -C "$git_work" remote add origin "$git_remote"
    git -C "$git_work" push -qu origin main
    git --git-dir="$git_remote" symbolic-ref HEAD refs/heads/main
}

clone_source_pair() {
    local case_name=$1
    local reference=$2
    prepare_project_pair "$case_name"
    mkdir -p "$reference_dir/vendor/fixture" "$candidate_dir/vendor/fixture"
    git clone -q "$git_remote" "$reference_dir/vendor/fixture/source"
    git clone -q "$git_remote" "$candidate_dir/vendor/fixture/source"
    write_source_installed "$reference_dir" "$git_remote" "$reference"
    write_source_installed "$candidate_dir" "$git_remote" "$reference"
}

prepare_dist_pair dist-clean
run_status dist-clean "$reference_dir" "$candidate_dir"

printf 'modified\n' > "$reference_dir/vendor/fixture/dist/src/file.txt"
printf 'modified\n' > "$candidate_dir/vendor/fixture/dist/src/file.txt"
run_status dist-modified "$reference_dir" "$candidate_dir"
run_status dist-modified-verbose "$reference_dir" "$candidate_dir" -v

prepare_dist_pair dist-symlink
mv "$reference_dir/vendor/fixture/dist" "$reference_dir/vendor/fixture/dist-real"
mv "$candidate_dir/vendor/fixture/dist" "$candidate_dir/vendor/fixture/dist-real"
ln -s dist-real "$reference_dir/vendor/fixture/dist"
ln -s dist-real "$candidate_dir/vendor/fixture/dist"
run_status dist-symlink "$reference_dir" "$candidate_dir" -v

create_git_remote source-clean
clone_source_pair source-clean "$first_reference"
run_status source-clean "$reference_dir" "$candidate_dir"

printf '{"name":"fixture/source","changed":true}\n' \
    > "$reference_dir/vendor/fixture/source/composer.json"
printf '{"name":"fixture/source","changed":true}\n' \
    > "$candidate_dir/vendor/fixture/source/composer.json"
run_status source-modified "$reference_dir" "$candidate_dir"
run_status source-modified-verbose "$reference_dir" "$candidate_dir" -v

create_git_remote source-unpushed
clone_source_pair source-unpushed "$first_reference"
for project in "$reference_dir" "$candidate_dir"; do
    package=$project/vendor/fixture/source
    git -C "$package" config user.name 'Status Fixture'
    git -C "$package" config user.email 'status@example.org'
    printf 'unpushed\n' > "$package/unpushed.txt"
    git -C "$package" add unpushed.txt
    GIT_AUTHOR_DATE='2001-01-02T00:00:00Z' GIT_COMMITTER_DATE='2001-01-02T00:00:00Z' \
        git -C "$package" commit -qm unpushed
done
unpushed_reference=$(git -C "$reference_dir/vendor/fixture/source" rev-parse HEAD)
write_source_installed "$reference_dir" "$git_remote" "$unpushed_reference"
write_source_installed "$candidate_dir" "$git_remote" "$unpushed_reference"
run_status source-unpushed "$reference_dir" "$candidate_dir"
run_status source-unpushed-verbose "$reference_dir" "$candidate_dir" -v

printf 'locally modified\n' > "$reference_dir/vendor/fixture/source/tracked.txt"
printf 'locally modified\n' > "$candidate_dir/vendor/fixture/source/tracked.txt"
run_status source-local-unpushed "$reference_dir" "$candidate_dir"

create_git_remote source-version
printf 'second commit\n' > "$git_work/tracked.txt"
git -C "$git_work" add tracked.txt
GIT_AUTHOR_DATE='2001-01-03T00:00:00Z' GIT_COMMITTER_DATE='2001-01-03T00:00:00Z' \
    git -C "$git_work" commit -qm second
second_reference=$(git -C "$git_work" rev-parse HEAD)
git -C "$git_work" push -qu origin main
clone_source_pair source-version "$first_reference"
git -C "$reference_dir/vendor/fixture/source" switch -q --detach "$second_reference"
git -C "$candidate_dir/vendor/fixture/source" switch -q --detach "$second_reference"
run_status source-version "$reference_dir" "$candidate_dir"
run_status source-version-verbose "$reference_dir" "$candidate_dir" -v
run_status source-version-very-verbose "$reference_dir" "$candidate_dir" -vv

create_git_remote all-conditions
clone_source_pair all-conditions "$first_reference"
for project in "$reference_dir" "$candidate_dir"; do
    package=$project/vendor/fixture/source
    git -C "$package" config user.name 'Status Fixture'
    git -C "$package" config user.email 'status@example.org'
    printf 'unpushed\n' > "$package/unpushed.txt"
    git -C "$package" add unpushed.txt
    GIT_AUTHOR_DATE='2001-01-04T00:00:00Z' GIT_COMMITTER_DATE='2001-01-04T00:00:00Z' \
        git -C "$package" commit -qm unpushed
    printf 'locally modified\n' > "$package/tracked.txt"
done
run_status all-conditions "$reference_dir" "$candidate_dir"

run_status lifecycle-scripts "$script_fixture" "$script_fixture"
run_status lifecycle-no-scripts "$script_fixture" "$script_fixture" --no-scripts

if [[ $failures -ne 0 ]]; then
    printf '%s status differential check(s) failed\n' "$failures" >&2
    exit 1
fi

printf 'All Composer status differential cases passed.\n'
