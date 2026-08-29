#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

RIFF_BIN="${RIFF_BIN:-$REPO_DIR/target/release/riff}"
COMPOSER_BIN="${COMPOSER_BIN:-composer}"
PHP_BIN="${PHP_BIN:-php}"
COLD_RUNS="${COLD_RUNS:-3}"
WARM_RUNS="${WARM_RUNS:-5}"
FIXTURE_REPOSITORY="${FIXTURE_REPOSITORY:-https://github.com/symfony/demo.git}"
FIXTURE_COMMIT="${FIXTURE_COMMIT:-920d86dc809f837543cb519d3df5b364a2c36577}"
RESULTS_JSON="${RESULTS_JSON:-$REPO_DIR/docs/assets/symfony-demo-install.json}"
OUTPUT_SVG="${OUTPUT_SVG:-$REPO_DIR/docs/assets/symfony-demo-install.svg}"

for command in git hyperfine jq tar; do
	if ! command -v "$command" >/dev/null 2>&1; then
		echo "error: $command is required for the Symfony Demo benchmark" >&2
		exit 1
	fi
done

resolve_command() {
	local requested=$1 resolved
	if [[ "$requested" == */* ]]; then
		if [[ ! -x "$requested" ]]; then
			echo "error: executable not found at $requested" >&2
			return 1
		fi
		printf '%s\n' "$requested"
	else
		if ! resolved="$(command -v "$requested")"; then
			echo "error: $requested was not found on PATH" >&2
			return 1
		fi
		printf '%s\n' "$resolved"
	fi
}

RIFF_BIN="$(resolve_command "$RIFF_BIN")"
COMPOSER_BIN="$(resolve_command "$COMPOSER_BIN")"
PHP_BIN="$(resolve_command "$PHP_BIN")"

if [[ ! "$COLD_RUNS" =~ ^[1-9][0-9]*$ ]] || [[ ! "$WARM_RUNS" =~ ^[1-9][0-9]*$ ]]; then
	echo "error: COLD_RUNS and WARM_RUNS must be positive integers" >&2
	exit 1
fi

BENCH_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/riff-symfony-demo-bench.XXXXXX")"
trap 'rm -rf -- "$BENCH_ROOT"' EXIT

fixture_dir="${SYMFONY_DEMO_DIR:-}"
if [[ -z "$fixture_dir" ]]; then
	fixture_dir="$BENCH_ROOT/fixture"
	git clone --quiet --no-checkout "$FIXTURE_REPOSITORY" "$fixture_dir"
	git -C "$fixture_dir" checkout --quiet "$FIXTURE_COMMIT"
fi

fixture_dir="$(cd "$fixture_dir" && pwd)"
actual_commit="$(git -C "$fixture_dir" rev-parse HEAD)"
if [[ "$actual_commit" != "$FIXTURE_COMMIT" ]]; then
	echo "error: fixture is at $actual_commit, expected $FIXTURE_COMMIT" >&2
	exit 1
fi

if [[ ! -f "$fixture_dir/composer.json" || ! -f "$fixture_dir/composer.lock" ]]; then
	echo "error: $fixture_dir is not a locked Composer project" >&2
	exit 1
fi

riff_version="$("$RIFF_BIN" --version | awk '{print $2}')"
composer_version="$("$COMPOSER_BIN" --version --no-ansi | awk 'NR == 1 {print $3}')"
php_version="$("$PHP_BIN" -r 'echo PHP_VERSION;')"
riff_commit="$(git -C "$REPO_DIR" rev-parse HEAD)"
hyperfine_version="$(hyperfine --version | awk '{print $2}')"
system="$(uname -srm)"
cpu="$(awk -F ':' '/model name/ { value = $2; sub(/^[[:space:]]+/, "", value); print value; exit }' /proc/cpuinfo 2>/dev/null || true)"
if [[ -z "$cpu" ]] && command -v sysctl >/dev/null 2>&1; then
	cpu="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || true)"
fi
package_count="$(jq '(.packages | length) + (."packages-dev" | length)' "$fixture_dir/composer.lock")"

riff_home="$BENCH_ROOT/home-riff"
composer_home="$BENCH_ROOT/home-composer"
mkdir -p "$riff_home" "$composer_home"

common_flags=(
	install
	--prefer-dist
	--no-interaction
	--no-progress
	--no-plugins
	--no-scripts
	--no-ansi
)

extract_project() {
	local destination=$1
	mkdir -p "$destination"
	git -C "$fixture_dir" archive "$FIXTURE_COMMIT" | tar -x -C "$destination"
}

riff_platform_template="$BENCH_ROOT/riff-platform-template"
mkdir -p "$riff_platform_template"
RIFF_CACHE_DIR="$riff_platform_template" \
	RIFF_PHP="$PHP_BIN" \
	COMPOSER_HOME="$riff_home" \
	"$RIFF_BIN" check-platform-reqs --lock --quiet --no-ansi -d "$fixture_dir" \
	>/dev/null 2>&1

if [[ ! -d "$riff_platform_template/platform" ]]; then
	echo "error: Riff did not create its PHP platform cache" >&2
	exit 1
fi

copy_riff_platform_cache() {
	local cache=$1
	mkdir -p "$cache"
	cp -R "$riff_platform_template/platform" "$cache/platform"
}

run_install() {
	local tool=$1 project=$2 cache=$3
	if [[ "$tool" == riff ]]; then
		RIFF_CACHE_DIR="$cache" \
			RIFF_PHP="$PHP_BIN" \
			COMPOSER_HOME="$riff_home" \
			"$RIFF_BIN" "${common_flags[@]}" --no-audit -d "$project"
	else
		COMPOSER_CACHE_DIR="$cache" \
			COMPOSER_HOME="$composer_home" \
			"$COMPOSER_BIN" "${common_flags[@]}" -d "$project"
	fi
}

populate_archive_template() {
	local tool=$1
	local project="$BENCH_ROOT/populate-project-$tool"
	local cache="$BENCH_ROOT/populate-cache-$tool"
	local template="$BENCH_ROOT/archive-template-$tool"

	extract_project "$project"
	if [[ "$tool" == riff ]]; then
		copy_riff_platform_cache "$cache"
	fi
	run_install "$tool" "$project" "$cache" >/dev/null 2>&1
	if [[ ! -d "$cache/files" ]]; then
		echo "error: $tool did not populate a package archive cache" >&2
		exit 1
	fi
	mkdir -p "$template"
	cp -R "$cache/files" "$template/files"
}

echo "Fixture:  Symfony Demo ${FIXTURE_COMMIT:0:8} ($package_count packages)"
echo "Riff:     $riff_version ($RIFF_BIN)"
echo "Composer: $composer_version ($COMPOSER_BIN)"
echo "PHP:      $php_version ($PHP_BIN)"
echo "Runs:     $COLD_RUNS cold, $WARM_RUNS warm"
echo "Policy:   fresh projects; warm runs retain archives only; Riff platform facts primed"
echo
echo "Populating untimed package archive templates..."
populate_archive_template riff
populate_archive_template composer

samples="$BENCH_ROOT/samples.tsv"
: >"$samples"

prepare_run() {
	local phase=$1 tool=$2 iteration=$3
	local project="$BENCH_ROOT/project-$phase-$tool-$iteration"
	local cache="$BENCH_ROOT/cache-$phase-$tool-$iteration"

	extract_project "$project"
	mkdir -p "$cache"
	if [[ "$phase" == warm ]]; then
		cp -R "$BENCH_ROOT/archive-template-$tool/files" "$cache/files"
	fi
	if [[ "$tool" == riff ]]; then
		cp -R "$riff_platform_template/platform" "$cache/platform"
	fi

	printf '%s\n%s\n' "$project" "$cache"
}

time_run() {
	local phase=$1 tool=$2 iteration=$3
	local prepared project cache result command_string
	prepared="$(prepare_run "$phase" "$tool" "$iteration")"
	project="$(sed -n '1p' <<<"$prepared")"
	cache="$(sed -n '2p' <<<"$prepared")"
	result="$BENCH_ROOT/hyperfine-$phase-$tool-$iteration.json"

	local -a command
	if [[ "$tool" == riff ]]; then
		command=(
			env
			"RIFF_CACHE_DIR=$cache"
			"RIFF_PHP=$PHP_BIN"
			"COMPOSER_HOME=$riff_home"
			"$RIFF_BIN"
			"${common_flags[@]}"
			--no-audit
			-d "$project"
		)
	else
		command=(
			env
			"COMPOSER_CACHE_DIR=$cache"
			"COMPOSER_HOME=$composer_home"
			"$COMPOSER_BIN"
			"${common_flags[@]}"
			-d "$project"
		)
	fi

	printf -v command_string '%q ' "${command[@]}"
	hyperfine \
		--runs 1 \
		--shell=none \
		--style none \
		--export-json "$result" \
		"$command_string"

	local milliseconds
	milliseconds="$(jq -r '.results[0].times[0] * 1000' "$result")"
	printf '%s\t%s\t%.6f\n' "$phase" "$tool" "$milliseconds" >>"$samples"
	printf '  %-4s %d/%d  %-8s %8.1f ms\n' "$phase" "$iteration" "$([[ "$phase" == cold ]] && echo "$COLD_RUNS" || echo "$WARM_RUNS")" "$tool" "$milliseconds"
}

run_phase() {
	local phase=$1 runs=$2 iteration
	for ((iteration = 1; iteration <= runs; iteration++)); do
		if ((iteration % 2 == 1)); then
			time_run "$phase" riff "$iteration"
			time_run "$phase" composer "$iteration"
		else
			time_run "$phase" composer "$iteration"
			time_run "$phase" riff "$iteration"
		fi
	done
}

echo
echo "Running timed installs..."
run_phase cold "$COLD_RUNS"
run_phase warm "$WARM_RUNS"

sample_array() {
	local phase=$1 tool=$2
	awk -F '\t' -v phase="$phase" -v tool="$tool" '$1 == phase && $2 == tool { print $3 }' "$samples" \
		| jq -R -s 'split("\n") | map(select(length > 0) | tonumber)'
}

median() {
	local phase=$1 tool=$2
	awk -F '\t' -v phase="$phase" -v tool="$tool" '$1 == phase && $2 == tool { print $3 }' "$samples" \
		| sort -n \
		| awk '{ values[NR] = $1 } END { if (NR % 2) print values[(NR + 1) / 2]; else print (values[NR / 2] + values[NR / 2 + 1]) / 2 }'
}

cold_riff_samples="$(sample_array cold riff)"
cold_composer_samples="$(sample_array cold composer)"
warm_riff_samples="$(sample_array warm riff)"
warm_composer_samples="$(sample_array warm composer)"
cold_riff_median="$(median cold riff)"
cold_composer_median="$(median cold composer)"
warm_riff_median="$(median warm riff)"
warm_composer_median="$(median warm composer)"
measured_at="$(date -u +%F)"

mkdir -p "$(dirname "$RESULTS_JSON")"
temporary_json="$(mktemp "${RESULTS_JSON}.XXXXXX")"
jq -n \
	--arg measured_at "$measured_at" \
	--arg repository "$FIXTURE_REPOSITORY" \
	--arg commit "$FIXTURE_COMMIT" \
	--argjson packages "$package_count" \
	--arg php "$php_version" \
	--arg composer "$composer_version" \
	--arg riff "$riff_version" \
	--arg riff_commit "$riff_commit" \
	--arg hyperfine "$hyperfine_version" \
	--arg system "$system" \
	--arg cpu "$cpu" \
	--argjson cold_runs "$COLD_RUNS" \
	--argjson warm_runs "$WARM_RUNS" \
	--argjson cold_riff_samples "$cold_riff_samples" \
	--argjson cold_composer_samples "$cold_composer_samples" \
	--argjson warm_riff_samples "$warm_riff_samples" \
	--argjson warm_composer_samples "$warm_composer_samples" \
	--argjson cold_riff_median "$cold_riff_median" \
	--argjson cold_composer_median "$cold_composer_median" \
	--argjson warm_riff_median "$warm_riff_median" \
	--argjson warm_composer_median "$warm_composer_median" \
	'{
		schema: 1,
		measured_at: $measured_at,
		fixture: {
			repository: $repository,
			commit: $commit,
			packages: $packages
		},
		environment: {
			php: $php,
			composer: $composer,
			riff: $riff,
			riff_commit: $riff_commit,
			hyperfine: $hyperfine,
			system: $system,
			cpu: $cpu
		},
		command: {
			common: ["install", "--prefer-dist", "--no-interaction", "--no-progress", "--no-plugins", "--no-scripts", "--no-ansi"],
			riff_only: ["--no-audit"]
		},
		cache_policy: {
			projects: "A fresh project tree is extracted before every timed run.",
			cold: "No package archives or repository metadata are retained.",
			warm: "Only package archives from an untimed install are retained; repository metadata is not retained.",
			riff_platform: "PHP platform facts are primed before timing and retained identically for cold and warm runs."
		},
		order: "Tools alternate which one runs first on every iteration.",
		runs: {
			cold: $cold_runs,
			warm: $warm_runs
		},
		results: {
			cold: {
				riff: {samples_ms: $cold_riff_samples, median_ms: $cold_riff_median},
				composer: {samples_ms: $cold_composer_samples, median_ms: $cold_composer_median}
			},
			warm: {
				riff: {samples_ms: $warm_riff_samples, median_ms: $warm_riff_median},
				composer: {samples_ms: $warm_composer_samples, median_ms: $warm_composer_median}
			}
		}
	}' >"$temporary_json"
mv "$temporary_json" "$RESULTS_JSON"

"$SCRIPT_DIR/render-symfony-demo-benchmark.sh" "$RESULTS_JSON" "$OUTPUT_SVG"

echo
printf 'Cold median: Riff %.0f ms, Composer %.0f ms (%.2f× faster)\n' \
	"$cold_riff_median" "$cold_composer_median" \
	"$(awk -v composer="$cold_composer_median" -v riff="$cold_riff_median" 'BEGIN { print composer / riff }')"
printf 'Warm median: Riff %.0f ms, Composer %.0f ms (%.2f× faster)\n' \
	"$warm_riff_median" "$warm_composer_median" \
	"$(awk -v composer="$warm_composer_median" -v riff="$warm_riff_median" 'BEGIN { print composer / riff }')"
echo "Results: $RESULTS_JSON"
