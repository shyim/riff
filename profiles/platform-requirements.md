# Composer 2.11 platform requirement reuse

Composer 2.11.0 adopted semver 3.6.0's memoization of constraint compaction
([upstream change](https://github.com/composer/semver/compare/3.5.2...3.6.0)).
Riff already caches parsed constraints in its pool and optimizer. The remaining
repeat work identified in this review was platform compatibility checking during
candidate selection and `require` recommendations.

`PlatformRequirementMatcher` memoizes each platform-name/constraint result within
one selection. Its borrowed platform cannot change underneath the cache. Both
successful and unsuccessful checks are reused. The existing uncached function
remains available and supplies the comparison implementation. After 64 consecutive
misses, the matcher releases its cache and uses uncached checks for the rest of
that selection. This avoids retaining an expensive cache for unique constraints.

## Measurements

Measured locally on 2026-11-07 with Rust 1.98.0, the repository release profile,
Linux x86_64, and an AMD EPYC-Genoa processor. Raw samples, fixture and binary
hashes, source hashes, and the Shopware revision are in
[platform-requirements.benchmark.json](platform-requirements.benchmark.json).
These hashes capture the cache optimization before the subsequent strict-PSR and
require-constraint fixes; those fixes do not modify the measured matcher.

| Workload | Uncached median | Candidate median | Change |
| --- | ---: | ---: | ---: |
| 10,000 candidates, three repeated constraints | 6.466 ms | 0.293 ms | -95.5% |
| 10,000 candidates, all unique constraints | 4.638 ms | 4.560 ms | -1.7% |
| Shopware lockfile, 253 packages | 0.190 ms | 0.101 ms | -46.8% |

Each workload uses nine alternating cached/uncached pairs, with twenty passes per
sample. Fixture construction is outside the timed region; matcher construction
and destruction are included. All samples assert the same accepted-package count.
The Shopware fixture contains 349 platform requirement occurrences and 79 distinct
name/constraint pairs; short-circuit evaluation means not every pair is visited.
The benchmark uses a fixed **synthetic platform**, version `8.2.0.0` for every
required platform package, rather than the host's PHP/extension inventory.

An initial unlimited-cache experiment regressed the unique-constraint workload by
50.6%, motivating the consecutive-miss cutoff. The final unique result is small
enough to treat as noise, not a claimed speedup. Builds and test processes were
finished before the final samples were collected.

These are platform-check microbenchmarks, including a real package graph. They
are not end-to-end install/update measurements, nor evidence of a comparable
whole-solver speedup. Peak memory was not measured.

## Reproduction

```sh
mise exec -- cargo build --release -p riff-core --example platform_requirement_benchmark --locked
target/release/examples/platform_requirement_benchmark repeated
target/release/examples/platform_requirement_benchmark unique
target/release/examples/platform_requirement_benchmark /path/to/shopware/composer.lock
```

Use the recorded Shopware revision and lockfile hash for the real-graph result.
The deterministic synthetic fixtures require no downloads or credentials.

Regression coverage compares cached and uncached results for compatible,
incompatible, malformed, and missing-extension requirements and for different
platforms. Existing candidate-selection and Composer fixture suites cover policy
integration. See [compatibility notes](../docs/compatibility.md#composer-211-follow-up)
for the security and compatibility work accompanying this optimization.
