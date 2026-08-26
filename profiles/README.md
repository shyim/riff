# Hot update profile

The host sets `kernel.perf_event_paranoid=3`, so `perf record` and
`cargo flamegraph` cannot sample this process. The checked-in profile uses a
symbolized release-equivalent build and Valgrind Callgrind instead.

Build without the release-only `mimalloc` feature so allocator internals stay
visible to Valgrind:

```sh
cargo build --profile profiling -p riff-cli --bin riff \
  --no-default-features
```

Workload:

```sh
RIFF_CACHE_DIR=/tmp/riff-profile-cache \
XDG_CACHE_HOME=/tmp/riff-profile-cache \
valgrind --tool=callgrind --collect-jumps=yes \
  --callgrind-out-file=profiles/callgrind.hot-update.optimized.out \
  /tmp/riff-target/profiling/riff update --dry-run --no-scripts \
  --no-interaction --no-ansi
```

Render the interactive call graph:

```sh
gprof2dot --format=callgrind --strip --wrap \
  profiles/callgrind.hot-update.optimized.out |
  dot -Tsvg -o profiles/callgraph.hot-update.optimized.svg
```

## Wall-clock comparison

`profiles/hyperfine.hot-update.json` compares 15 shell-free hot-cache dry-run
updates of this release binary and the PHP Composer checkout in
`/workspace/composer` on the same project and cache. PHP Composer averaged
1.757 seconds; `riff` averaged 20.6 milliseconds, an 85.5x mean speedup
for this workload. The Rust samples ranged from 17.6 to 23.4 milliseconds, and
repeated shared-host sets varied more than the optimization under test, so the
deterministic Callgrind instruction count below is the acceptance metric for
code changes.

The final profile executes 30,308,388 instructions, down from 158,367,471
for the pre-optimization release on the same project and cache (80.9%). The
borrowed constraint-matching path removed 1,324,755 instructions, selecting the
current-thread runtime for dry-run updates removed another 966,772, and using
randomly seeded `foldhash` maps in `PoolOptimizer` removed 5,952,359 more.
Replacing the optimizer's remaining order-independent version cache and transient
SipHash fingerprints removed another 1,768,686 instructions. A cache-only,
fixed-layout MessagePack representation then removed 4,957,365 instructions and
reduced the 37 filtered package files from 1,306,556 to 1,034,610 bytes (20.8%).
Using a randomized foldhash-backed ordered map for package dependency links
removed another 877,292 instructions (0.7%) while retaining their serialized
order and the filtered-cache v2 wire format. Finally, parsing numeric wildcard
branches without regex avoids initializing four general-purpose regex automata
for the common `x-dev` root alias path. A frozen-cache control measured
126,142,122 instructions before that change and 115,936,569 after it (8.1%).
Replacing the remaining regex-based OR separator and stability-suffix handling
with borrowed string scanners removed another 4,430,912 instructions (3.8%).
Replacing the cache-key and repository-URL sanitization regexes with direct
character scanners removed another 1,781,871 instructions (1.6%).
Compiling the shared caret/tilde version grammar once instead of as two regex
automata removed another 2,053,615 instructions (1.9%).
Replacing that remaining shared range regex with a direct scanner removed
another 2,493,436 instructions (2.3%) while retaining its capture semantics.
Replacing the optimizer policy's full candidate sort with equivalent linear
best-and-boundary selection removed another 1,146,472 instructions (1.1%).
Replacing the policy's generic split/filter/parse numeric-version iterator with
a checked byte scanner removed another 1,023,713 instructions (1.0%).
Storing dependency-map keys and constraints inline with `CompactString` removed
another 4,164,764 instructions (4.0%) while preserving the filtered-cache v2
wire format.
Using the same inline representation for author and funding metadata removed
another 2,238,104 instructions (2.3%), again without changing the v2 bytes.
Borrowing parsed constraints directly from the pool cache instead of deep-cloning
boxed semver trees on every lookup removed another 2,441,461 instructions (2.5%).
Keeping normalized versions borrowed across matches removed another 572,566
instructions (0.6%). Using randomly seeded foldhash maps for the Pool's private
name, provider, and constraint indexes removed another 1,245,955 instructions
(1.3%). Finally, borrowed-key cache entries reduced hit paths from two hash-table
lookups to one and removed another 1,269,297 instructions (1.4%).
Keeping normalized versions, pretty versions, and package/source/dist type tags
inline removed another 2,025,636 instructions (2.2%). The frozen workload keeps
all 4,805 of these values inline while preserving the filtered-cache v2 wire
format.
Keeping package keywords, licenses, binary paths, and autoload paths inline
removed another 2,185,249 instructions (2.5%). All but one of the 6,835 values
in the frozen workload fit inline, and `AutoloadPath::iter` now borrows its
existing storage instead of allocating a temporary vector.
Borrowing already-canonical Composer package names, using foldhash for the
installer's transient load queues, and avoiding redundant constraint display
formatting removed another 871,834 instructions (1.0%). This reduced lowercase
conversions from 7,716 to 1,992 calls in the frozen workload. Parsing numeric
version parts during the existing token scan and dispatching internal operators
directly removed another 529,525 instructions (0.6%).
Replacing each dependency section's `IndexMap` with a contiguous ordered small
map removed another 2,263,301 instructions (2.6%). Resolver code primarily
iterates these typically short maps, so the change avoids building two heap
buffers and hashing every key without making the measured point lookups
significant. Appending entries directly when reading the trusted binary cache
removed another 258,227 instructions (0.3%); human-readable Composer JSON still
deduplicates keys with last-value-wins semantics. The filtered-cache v2 bytes
remain unchanged.
Replacing the final alias and build-metadata regexes on the hot normalization
path with borrowed scanners removed another 920,952 instructions (1.1%). The
scanners preserve the previous regex capture semantics while avoiding two
automata and a temporary owned string for stripped build metadata.
Merging repeated pending constraints through the hash-table entry in place and
copying inline package versions directly removed another 1,499,519 instructions
(1.8%). This eliminates 1,824 generic formatting calls as well as a second hash
lookup and repeated copies of each growing disjunction.
Splitting the filtered v3 cache into solver-hot fields and opaque cold metadata
removed another 25,488,719 instructions (31.7%). The frozen workload now
decodes dependency data for all 961 candidates but materializes source, dist,
autoload, author, funding, and other install-only metadata for just 36 selected
packages. The 37 filtered files total 1,038,454 bytes, only 3,844 bytes (0.4%)
larger than v2.
Comparing the current and generated lock files structurally instead of
serializing both into `serde_json::Value` trees removed another 4,787,072
instructions (8.7%). The comparison retains the lock serializer's custom skip
semantics while avoiding JSON map allocation, hashing, and destruction on every
dry-run update.
Passing the updater's parsed lock and installed package-name set directly into
the implicit dry-run audit removed another 4,833,209 instructions (9.6%). This
eliminates a second `composer.lock` parse and a second `installed.json` load
while preserving standalone audit behavior and normal updates' use of the
newly written lock.
Matching parsed constraints directly against normalized package versions,
instead of allocating and caching equality constraints for both the pool and
optimizer, removed another 1,681,034 instructions (3.7%). This follows Aube's
direct range-to-version matching model while preserving the former provider
constraint semantics through a differential matrix.
Caching Aube-style parsed versions alongside normalized package versions and
inside parsed constraints removed another 1,718,347 instructions (3.9%). The
common Composer form uses a six-part inline representation, while an exact
heap fallback preserves PHP comparison behavior for arbitrary long versions.
Replacing the optimizer's transient nested owned sets and cloned snapshots with
an Aube-style borrowed name index and one-entry inline buckets removed another
1,283,356 instructions (3.1%). The index assigns each unique constraint one ID
while collecting it, borrows OR branches directly from request and pool data,
and uses linear per-name deduplication for the typically tiny buckets. Borrowing
the dry-run audit's package names and versions in foldhash collections removed
another 92,656 instructions (0.2%) without changing its serialized responses.
Replacing the optimizer's sorted dependency hashes with typed, commutative
128-bit fingerprints removed another 362,665 instructions (0.9%). This avoids
1,338 temporary vectors and sorts while retaining insertion-order-independent
grouping; a differential matrix compares its decisions with the former sorted
reference. Removing a disabled package-specific normalization trace condition
then removed another 187,792 instructions (0.5%) and 824 substring searches.
Borrowing each candidate's opaque cold metadata directly from its MessagePack
cache file removed another 717,090 instructions (1.8%). The solver now retains
the existing 37 file buffers and stores byte ranges for deferred hydration,
avoiding 961 metadata allocations and copies while preserving the filtered v3
wire format.
Keeping the installer's loaded-name set, pending dependency map, and sorted
loading frontier in `CompactString` removed another 273,308 instructions
(0.7%). Names and constraints that fit inline no longer round-trip through heap
`String`s; direct allocator calls attributed to `update_with_result` fell from
2,001 to 207 while deterministic batch ordering and constraint merge order stay
unchanged.
Replacing `RepositoryManager`'s formatted `name@version` deduplication keys
with Aube-style typed, foldhash-backed package identities removed another
1,860,407 instructions (4.8%). The manager now hashes each candidate's existing
name and normalized version without allocating a composite string, retains
collision-safe field equality through an `Arc<Package>` wrapper, and reserves
from each repository result's known length. The frozen workload performs exactly
961 identity hashes and no longer has a visible identity-table rehash path.
Indexing the optimizer's unique constraint strings once and lazily compiling
them by compact ID removed another 565,746 instructions (1.5%). Each candidate
now probes its normalized-version cache once before checking all matching
require and conflict constraints, and the grouping path no longer performs
4,042 string-keyed parsed-constraint cache probes. Keeping compilation lazy
preserves the profile's 179 parser calls; the optimized pool stage fell from
22.6% to 20.7% of the complete workload.
Keeping each candidate's matched constraint IDs in an Aube-style four-entry
inline buffer removed another 184,751 instructions (0.5%). This eliminates all
899 `RawVec<u32>` growth calls in the frozen workload; the optimized pool stage
now accounts for 20.4% of the complete profile without changing grouping or
selection behavior.
Reading transaction-planning state through an Aube-style borrowed JSON
projection removed another 930,912 instructions (2.6%). Updates materialize
only installed package identity, type, and source/dist references; Serde scans
but does not allocate dependencies, autoload rules, descriptions, licenses, or
other command-facing metadata. Installed-state loading fell from 5.75% to 3.24%
of the complete profile while the full `InstalledRepository` path remains
available to commands that need those fields.
Replacing the optimizer's three transient hash sets with compact membership
bitsets removed another 319,081 instructions (0.9%). Package IDs are dense and
one-based, so the 915-package frozen pool needs only fifteen `u64` words per
set. The foldhash set rehash path disappears, optimizer-attributed allocator
calls fall from 1,375 to 916, and the optimizer's total profile share falls
from 20.94% to 20.34% without changing package selection.
Replacing rule deduplication's clone-sort-SipHash sequence with a typed,
commutative literal fingerprint and using randomized foldhash tables in
transient rule generation removed another 539,195 instructions (1.5%).
`RuleSet::add` fell from 1.49% to 0.54% of the complete profile and SipHash fell
from 1.49% to 0.85%. Collision checks still compare literal multisets, including
duplicate multiplicity, without allocating on the uncommon reordered path.
Applying an Aube-style typed projection to dry-run transaction hydration removed
another 2,474,231 instructions (7.1%). When the selected production and
development package identity sequences differ from the current lock, that
mismatch proves the lock changed, so the dry-run path decodes only package type
and source/dist references needed for transaction planning. Matching identities
fall back to full hydration and exact structural lock comparison, preserving
metadata-only lock change detection. Selected-package hydration fell from 3.30%
to 2.22% of the complete profile; full lock conversion, content hashing, and
generated-lock destruction disappear from the proven-changed path.
Replacing the filtered v3 MessagePack solver envelope with an Aube-style,
validated `rkyv` v4 archive removed another 1,350,384 instructions (4.2%). Hot
strings and dependency pairs are validated in place, then copied directly into
their solver representation; already-deduplicated dependency maps use an ordered
append path. UTF-8 conversion fell from 1,401,143 to 385,854 instructions. The
37 v4 archives total 1,112,240 bytes, 73,786 bytes (7.1%) larger than v3; archive
validation remains enabled so a corrupt cache falls back safely instead of
creating unchecked strings.
Flattening the optimizer's three-level dependency-group map into one pre-sized
borrowed-key index, hashing each package's already-ordered constraint bucket in
place, and keeping common package-ID groups inline removed another 142,196
instructions (0.46%, using the highest of three captures). This applies Aube's
borrowed fixed-hash index and `SmallVec` bucket pattern without changing the
public policy API. Copied group-name allocations disappear, group-vector growth
falls from 243 to 110 calls, and the identical-dependency phase falls from
15.56% to 14.81% of the complete profile.
Interning optimizer constraints once into dense IDs, storing four IDs inline per
package-name bucket, and inserting each unique constraint through one randomized
hash-table entry removed another 13,441 instructions (0.044%, using the highest
of three captures). Constraint-insertion string comparisons fall from 8,151 to
3,072 and constraint-bucket growth falls from 63 to 10 calls. Random seeding is
retained for repository-controlled constraint text instead of adopting Aube's
fixed-seed maps at an untrusted-input boundary.
Preserving repository fetch results in an Aube-style ordered package-name map
and sorting versions only within each name removed another 251,808 instructions
(0.82%, using the highest of three captures). The former global stable sort of
1,213 packages becomes fourteen name-local sorts; its profile share falls from
2.75% to 1.60%, and its string comparisons fall from 11,682 to 5,646. The 37
asynchronously completed name batches still enter the pool deterministically,
including lock packages merged for partial updates.
Using each ordered map key as the package-name order and comparing only versions
inside its canonical-name bucket removed another 154,372 instructions (0.51%,
using the highest of three captures). The fourteen bucket sorts fall from 1.60%
to 0.99% of the complete profile. Debug builds verify that every fetched or
locked package canonicalizes to its bucket key, and the stable version sort
continues to preserve repository precedence for duplicate versions.
The runtime change has a larger wall-time effect because it also avoids
worker-thread creation and scheduler overhead that Callgrind does not model. The
optimized pool stage averages about 6 ms, down from about 10.8 ms.
