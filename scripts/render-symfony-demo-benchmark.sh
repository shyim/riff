#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

RESULTS_JSON="${1:-$REPO_DIR/docs/assets/symfony-demo-install.json}"
OUTPUT_SVG="${2:-$REPO_DIR/docs/assets/symfony-demo-install.svg}"

if ! command -v jq >/dev/null 2>&1; then
	echo "error: jq is required to render the benchmark chart" >&2
	exit 1
fi

if ! jq -e '
	.schema == 1
	and (.results.cold.riff.median_ms | numbers)
	and (.results.cold.composer.median_ms | numbers)
	and (.results.warm.riff.median_ms | numbers)
	and (.results.warm.composer.median_ms | numbers)
' "$RESULTS_JSON" >/dev/null; then
	echo "error: invalid benchmark results in $RESULTS_JSON" >&2
	exit 1
fi

riff_version="$(jq -r '.environment.riff' "$RESULTS_JSON")"
composer_version="$(jq -r '.environment.composer' "$RESULTS_JSON")"
php_version="$(jq -r '.environment.php' "$RESULTS_JSON")"
fixture_commit="$(jq -r '.fixture.commit[0:8]' "$RESULTS_JSON")"
package_count="$(jq -r '.fixture.packages' "$RESULTS_JSON")"
measured_at="$(jq -r '.measured_at' "$RESULTS_JSON")"
cold_runs="$(jq -r '.runs.cold' "$RESULTS_JSON")"
warm_runs="$(jq -r '.runs.warm' "$RESULTS_JSON")"
cold_riff="$(jq -r '.results.cold.riff.median_ms | round' "$RESULTS_JSON")"
cold_composer="$(jq -r '.results.cold.composer.median_ms | round' "$RESULTS_JSON")"
warm_riff="$(jq -r '.results.warm.riff.median_ms | round' "$RESULTS_JSON")"
warm_composer="$(jq -r '.results.warm.composer.median_ms | round' "$RESULTS_JSON")"

nice_axis() {
	local value=$1 step=$2
	awk -v value="$value" -v step="$step" 'BEGIN { print int((value + step - 1) / step) * step }'
}

bar_width() {
	local value=$1 axis=$2
	awk -v value="$value" -v axis="$axis" 'BEGIN { width = int((value / axis) * 615 + 0.5); print width < 2 ? 2 : width }'
}

speedup() {
	local slower=$1 faster=$2
	awk -v slower="$slower" -v faster="$faster" 'BEGIN { printf "%.2f", slower / faster }'
}

label_position() {
	local width=$1
	local label=$((225 + width + 12))
	local anchor="start"
	if ((label + 80 > 900)); then
		label=$((225 + width - 10))
		anchor="end"
	fi
	printf '%s %s\n' "$label" "$anchor"
}

format_ms() {
	local number=$1 formatted=""
	while ((${#number} > 3)); do
		formatted=",${number: -3}${formatted}"
		number="${number:0:${#number}-3}"
	done
	printf '%s%s' "$number" "$formatted"
}

cold_axis="$(nice_axis "$cold_composer" 1000)"
warm_axis="$(nice_axis "$warm_composer" 500)"
cold_mid=$((cold_axis / 2))
warm_mid=$((warm_axis / 2))
cold_composer_width="$(bar_width "$cold_composer" "$cold_axis")"
cold_riff_width="$(bar_width "$cold_riff" "$cold_axis")"
warm_composer_width="$(bar_width "$warm_composer" "$warm_axis")"
warm_riff_width="$(bar_width "$warm_riff" "$warm_axis")"
read -r cold_composer_label cold_composer_anchor <<<"$(label_position "$cold_composer_width")"
read -r cold_riff_label cold_riff_anchor <<<"$(label_position "$cold_riff_width")"
read -r warm_composer_label warm_composer_anchor <<<"$(label_position "$warm_composer_width")"
read -r warm_riff_label warm_riff_anchor <<<"$(label_position "$warm_riff_width")"
cold_speedup="$(speedup "$cold_composer" "$cold_riff")"
warm_speedup="$(speedup "$warm_composer" "$warm_riff")"

mkdir -p "$(dirname "$OUTPUT_SVG")"
temporary_svg="$(mktemp "${OUTPUT_SVG}.XXXXXX")"
trap 'rm -f -- "$temporary_svg"' EXIT

cat >"$temporary_svg" <<SVG
<svg xmlns="http://www.w3.org/2000/svg" width="900" height="510" viewBox="0 0 900 510" role="img" aria-labelledby="title description">
  <title id="title">Symfony Demo installation benchmark</title>
  <desc id="description">Riff ${riff_version} installs Symfony Demo in a median of $(format_ms "$cold_riff") milliseconds with a cold package cache compared with Composer ${composer_version} at $(format_ms "$cold_composer") milliseconds. With package archives cached, Riff takes $(format_ms "$warm_riff") milliseconds and Composer takes $(format_ms "$warm_composer") milliseconds.</desc>
  <rect width="900" height="510" fill="#0d1117" rx="16"/>
  <text x="48" y="62" fill="#f0f6fc" font-family="system-ui, sans-serif" font-size="28" font-weight="700">Symfony Demo · composer install</text>
  <text x="48" y="92" fill="#8b949e" font-family="system-ui, sans-serif" font-size="16">Lower is better · median wall-clock time in milliseconds</text>

  <g font-family="system-ui, sans-serif">
    <text x="48" y="144" fill="#f0f6fc" font-size="20" font-weight="700">Cold package cache · ${cold_runs} runs</text>
    <text x="48" y="169" fill="#8b949e" font-size="14">ZIP archives downloaded</text>
    <line x1="225" y1="142" x2="840" y2="142" stroke="#30363d"/>
    <text x="225" y="128" fill="#8b949e" font-size="13">0 ms</text>
    <text x="530" y="128" fill="#8b949e" font-size="13">$(format_ms "$cold_mid") ms</text>
    <text x="830" y="128" fill="#8b949e" font-size="13" text-anchor="end">$(format_ms "$cold_axis") ms</text>
    <rect x="225" y="184" width="${cold_composer_width}" height="34" rx="6" fill="#8b949e"/>
    <text x="48" y="207" fill="#c9d1d9" font-size="16">Composer ${composer_version}</text>
    <text x="${cold_composer_label}" y="207" fill="#f0f6fc" font-size="16" font-weight="700" text-anchor="${cold_composer_anchor}">$(format_ms "$cold_composer") ms</text>
    <rect x="225" y="234" width="${cold_riff_width}" height="34" rx="6" fill="#f97316"/>
    <text x="48" y="257" fill="#c9d1d9" font-size="16">Riff ${riff_version}</text>
    <text x="${cold_riff_label}" y="257" fill="#f0f6fc" font-size="16" font-weight="700" text-anchor="${cold_riff_anchor}">$(format_ms "$cold_riff") ms</text>
    <text x="840" y="247" fill="#fb923c" font-size="17" font-weight="700" text-anchor="end">${cold_speedup}× faster</text>

    <text x="48" y="323" fill="#f0f6fc" font-size="20" font-weight="700">Warm package cache · ${warm_runs} runs</text>
    <text x="48" y="348" fill="#8b949e" font-size="14">Package archives already cached</text>
    <line x1="225" y1="321" x2="840" y2="321" stroke="#30363d"/>
    <text x="225" y="307" fill="#8b949e" font-size="13">0 ms</text>
    <text x="530" y="307" fill="#8b949e" font-size="13">$(format_ms "$warm_mid") ms</text>
    <text x="830" y="307" fill="#8b949e" font-size="13" text-anchor="end">$(format_ms "$warm_axis") ms</text>
    <rect x="225" y="363" width="${warm_composer_width}" height="34" rx="6" fill="#8b949e"/>
    <text x="48" y="386" fill="#c9d1d9" font-size="16">Composer ${composer_version}</text>
    <text x="${warm_composer_label}" y="386" fill="#f0f6fc" font-size="16" font-weight="700" text-anchor="${warm_composer_anchor}">$(format_ms "$warm_composer") ms</text>
    <rect x="225" y="413" width="${warm_riff_width}" height="34" rx="6" fill="#f97316"/>
    <text x="48" y="436" fill="#c9d1d9" font-size="16">Riff ${riff_version}</text>
    <text x="${warm_riff_label}" y="436" fill="#f0f6fc" font-size="16" font-weight="700" text-anchor="${warm_riff_anchor}">$(format_ms "$warm_riff") ms</text>
    <text x="840" y="426" fill="#fb923c" font-size="17" font-weight="700" text-anchor="end">${warm_speedup}× faster</text>
  </g>

  <text x="48" y="482" fill="#8b949e" font-family="system-ui, sans-serif" font-size="13">Symfony Demo ${fixture_commit} · ${package_count} packages · PHP ${php_version} · measured ${measured_at}</text>
</svg>
SVG

mv "$temporary_svg" "$OUTPUT_SVG"
trap - EXIT
echo "Rendered $OUTPUT_SVG"
