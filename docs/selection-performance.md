# Explorer selection cost

The additive `featuredGames` list is prepared while indexing and selected from
all matches. A query does not scan the database to rank them. Old `topGames`
semantics and all counts remain intact; clients can adopt the new list without
breaking old bridges.

## Reproduce without a private database

`crates/bridge/examples/selection_profile.rs` constructs 50,000 hand-built
games, all following a 28-ply line, with varied ratings and dates. It prints
only counts, byte sizes, peak process RSS and HTTP timings. Four workers bound
the experiment on a shared host:

```sh
cargo build --release -p bridge --example selection_profile
OSCHESS_BRIDGE_THREADS=4 nice -n 19 target/release/examples/selection_profile /tmp/selection-after
```

Use a new empty output folder. To compare the baseline, create a detached
worktree at `4f3c02573d2a4a01b535315bbafe411b84cdf1c3`, copy only this example
into its `crates/bridge/examples/`, build it and use another empty folder.
The workload and command must be identical for both versions.

`first` is the initial start-position response after building. Navigation
visits plies 0 through 10 once and then five times. Filters query ply 2 for
`whiteelo:2700..` and `date:2020..`, then repeat each five times. Deep positions
are plies 22, 26 and 28, once and then five times. Every response must be 200.
The OS page cache is warm after building; these are not cold-disk guarantees.

Recorded on 2026-10-05, Rust 1.98.1 release builds on a shared Linux/WSL host,
four search workers, nice 19. Values are observations, not timing assertions.

| Synthetic cost | Before | After |
| --- | ---: | ---: |
| Index build, ms | 112.343 | 108.324 |
| Position index, bytes | 671697 | 672411 |
| Move stream, bytes | 3900316 | 4100316 |
| Peak RSS at build end, bytes | 27906048 | 36925440 |
| Peak RSS after queries, bytes | 31211520 | 36925440 |

| HTTP workload (samples) | Before median / p95, ms | After median / p95, ms |
| --- | ---: | ---: |
| first (1) | 1.103 / 1.103 | 1.113 / 1.113 |
| navigation_first (11) | 0.981 / 1.042 | 1.020 / 1.117 |
| navigation_repeat (55) | 0.925 / 1.104 | 0.977 / 1.084 |
| filter_first (2) | 6.685 / 6.685 | 4.300 / 4.300 |
| filter_repeat (10) | 0.889 / 0.992 | 1.013 / 1.226 |
| deep_first (3) | 7.565 / 8.772 | 7.510 / 7.973 |
| deep_repeat (15) | 7.457 / 10.597 | 7.626 / 8.271 |

The bounded fixture keeps navigation around 1 ms and repeated deep queries
around 8 ms. Each worker adds 104 reserved bytes for 13 selection slots
(12 retained plus the insertion slot). Build entries grow from 16 to 24 bytes,
with their reservations calculated from the actual entry size. Folding keeps
the union of both top-12 lists, at most 24 unique games. The header prepass
reuses one budgeted workspace and reads at most 2,048 records per batch.

Opening records append at most 12 selected record numbers. Stream slots grow
from 64 to 68 bytes, four bytes per database record plus alignment padding.
Index version 6 and stream version 5 rebuild older cache files. Filtered
answers include both lists in their existing budget and retain at most 64
entries, keyed by build, database generation, position and filter.

## Real database delivery evidence

This is delivery evidence, separate from repository-runnable acceptance.
Neither a private database nor access to its owner's machine is needed to test
or repair this change. No game, player or event contents are recorded here.

Mega Database 2026: 11,966,514 records, 11,959,813 indexed games. Source files
were read through the Windows filesystem mount from WSL; scratch indices were
on the Linux filesystem. This is not a native Windows latency measurement.

| Mega cost | Before | After |
| --- | ---: | ---: |
| Index build, ms | 84844.729 | 96730.941 |
| Position index, bytes | 2209349697 | 2464939602 |
| Move stream, bytes | 2164226168 | 2212092280 |
| Peak RSS at build end, bytes | 2691694592 | 2744172544 |
| Peak RSS after queries, bytes | 2691694592 | 2744172544 |

| HTTP workload (samples) | Before median / p95, ms | After median / p95, ms |
| --- | ---: | ---: |
| first (1) | 44.537 / 44.537 | 59.929 / 59.929 |
| navigation_first (11) | 8.663 / 26.612 | 22.074 / 44.634 |
| navigation_repeat (55) | 5.037 / 6.559 | 9.276 / 23.419 |
| filter_first (2) | 9434.290 / 9434.290 | 10132.523 / 10132.523 |
| filter_repeat (10) | 6.179 / 6.926 | 5.663 / 9.803 |
| deep_first (3) | 28.103 / 51.585 | 24.502 / 82.646 |
| deep_repeat (15) | 7.334 / 26.875 | 6.693 / 18.725 |

The index grows by about 256 MB and the stream by 48 MB (decimal units).
The extra header pass and larger entries add build cost; the final observed
build took 96.7 s versus 84.8 s. The shared host also affects latency: another
complete run of the new implementation took 90.0 s, with 5.1 ms median repeated
navigation. The final run measured 9.3 ms. A single noisy run cannot establish
a precise regression percentage or a platform-wide latency guarantee.

To repeat with a local database, append its path to the command. To compare
queries without rebuilding, append `--reuse` after that path; it reopens the
existing scratch index and still makes fresh HTTP requests in a new process.
Run both binaries on the same CPUs and alternate their order when investigating
host load. The first build attempt encountered an OS header-read error
(`ENOMEM`); a separate bounded scan and two subsequent complete builds read
all headers successfully. The error did not reproduce.

### Repeated comparison with existing indices

Three baseline/changed pairs were then run in fresh processes on CPUs 7-10
(`taskset -c 7-10`), reusing the indices. Each row below is the median of the
three runs' medians and the median of their p95s, not a pooled percentile.
The background stress campaign used CPUs 0-6; other host activity continued.

| HTTP workload | Before median / p95, ms | After median / p95, ms |
| --- | ---: | ---: |
| first | 23.980 / 23.980 | 23.678 / 23.678 |
| navigation_first | 11.088 / 16.730 | 11.687 / 22.322 |
| navigation_repeat | 5.791 / 8.609 | 5.901 / 10.574 |
| filter_first | 9478.075 / 9478.075 | 8687.363 / 8687.363 |
| filter_repeat | 6.659 / 9.249 | 6.067 / 6.688 |
| deep_first | 22.693 / 59.870 | 24.784 / 64.058 |
| deep_repeat | 6.381 / 31.935 | 7.076 / 42.938 |

The paired observations support fast navigation but also show why shared-host
timing is evidence rather than a deterministic test. The synthetic and
structural checks above are the reproducible acceptance gate.

## Automated acceptance

`cargo test -p bridge --test explorer_selection` compares indexed, deep and
combined answers with an independent full-sort oracle over 228 generated
games. It covers filters, cache generations, historical/partial/unknown dates,
no quotas, unchanged All games membership and date-independent ties. Ranking
unit tests cover exact/half-point thresholds and calendar boundaries.

The source header file is removed after indexing in the oracle test: all
position selection still works, including deep and combined results. The
`found_games_are_added_up_in_the_room_reserved` unit test sends 50,000 matches
through bounded accumulators, checks that selection capacity never grows, and
compares one accumulator and a merge with a full-sort oracle. Existing explorer
tests continue to cover move counts, legacy selection, corruption, cancellation,
memory budgets and background work.
