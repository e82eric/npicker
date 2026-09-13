# Scan regression benchmarks

## Compare a revision with current changes

The recommended workflow builds both versions and measures them back-to-back:

```powershell
./nfm-file-system/Compare-ScanBenchmark.ps1 -Root G:\src
```

This compares `HEAD` with your current working tree, including uncommitted
changes. It records the root once and replays the exact same dataset through
both binaries. Each pair alternates order (baseline/current, current/baseline).
There are seven measured pairs by default, with a warm-up before each individual
measurement. No pre-existing CSV baseline is needed.

Use a previous revision, reuse a recording, or include live scans:

```powershell
./nfm-file-system/Compare-ScanBenchmark.ps1 -Recording ./paths.nfm -Base HEAD~1
./nfm-file-system/Compare-ScanBenchmark.ps1 -Root G:\src -LiveScan -Runs 9 -ThresholdPercent 10
```

`HEAD` is appropriate for uncommitted changes; `HEAD~1` compares against the
parent of the latest commit. `-Base` accepts another commit or branch too.
`-LiveScan` adds alternating live measurements after replay. Live filesystem
changes can still invalidate the comparison; counts and dataset metadata are
checked. Replay remains the controlled regression test.

The script requires Windows, PowerShell 7, Git, and Cargo. Both builds use
`--locked --release`, the same installed toolchain, and separate target
directories. Compilation is outside the timer. Do not edit the current source
while the script is building/measuring. Existing cargo configuration applies.

The baseline is a detached temporary worktree. If it predates the harness, the
script installs the current measurement module, example, and feature/module
wiring into that worktree only, and reports this overlay. It does not transplant
the current store, scanner, interning, or hashing implementation. Older revisions
must still expose the storage APIs the harness uses; incompatible versions fail
to build rather than silently changing baseline behavior. If the baseline has
its own harness, its CSV protocol must match the current one.

Reports, metadata, both binaries/build directories, and any new recording are
retained in a uniquely named directory under the system temporary directory.
Use `-OutputDirectory C:\bench-results\run-001` to choose a new directory. It
must be outside the checkout and scanned root. The temporary baseline worktree
and its harness-only changes are removed on success or failure; your working
tree and Git index are not changed. Retained build directories can be large.

The script exits nonzero on build/measurement failure, incompatible results, or
a median regression above the threshold (default 10%). Check `$LASTEXITCODE`.
CSV files and comparison reports remain available for inspection. Metadata
records the revision IDs, initial working-tree status, toolchain versions, and
whether a baseline harness overlay was used. It does not archive uncommitted
source contents. This is a regression signal, not statistical proof; rerun noisy
or borderline results on an otherwise idle machine.

## Standalone commands

### Private memory measurements

Run memory comparisons separately from normal timing checks:

```powershell
./nfm-file-system/Compare-ScanBenchmark.ps1 -Recording ./paths.nfm -Base HEAD -Memory
./nfm-file-system/Compare-ScanBenchmark.ps1 -Recording ./paths.nfm -Root G:\src -LiveScan -Base 9de7640 -Memory
```

The standalone executable accepts `--memory` as the final argument to `replay`
or `scan`. This adds CSV columns for starting, ending, and peak sampled private
bytes, signed end-minus-start and peak-minus-start deltas, and sample count.
Comparison reports baseline/current medians in MiB. Memory comparisons do not
apply the timing threshold or enforce a memory threshold; incompatible results
still fail. Memory CSVs cannot be compared with unsampled timing CSVs.

The sampler uses Windows `GetProcessMemoryInfo` (`PrivateUsage`), measuring total
private committed memory for the benchmark process, not resident working set or
the whole process tree. Sampling occurs every 10 ms during each measured run,
plus explicit start/end samples. Brief peaks can be missed. The ending sample
is taken while the final snapshot is alive, after completion has freed the
intern table; the sampled peak may capture that table's earlier memory use.

Replay input is loaded before sampling and remains included in absolute totals.
Warm-up allocator retention, runtime memory, and the sampler thread are also
included. Deltas are useful context, not exact allocation accounting. Warm-up
is not sampled. Sampling is disabled entirely without `-Memory`/`--memory`.
Older baselines without a harness receive this support through the harness-only
overlay; revisions with an existing harness must support the memory CSV protocol.

The harness now uses production hashing directly. Hash-selection dependencies
and dispatch are removed; NFM_BENCH_HASH is ignored. Existing .nfm recordings
remain compatible, but old hash-specific CSV results require new baselines.

Build in release mode:

```powershell
cargo build --release -p nfm_file_system --example scan_bench --features scan-bench
```

Record a representative dataset once if needed (destination must not exist):

```powershell
target\release\examples\scan_bench.exe record paths.nfm G:\src
```

Recordings contain parent indexes and UTF-8 names in writer insertion order, not
file contents. Multiple roots can be supplied to record. Serialization occurs
after the scan and is excluded from its reported time.

## Baseline and comparison

On the known-good version, save measurements:

```powershell
target\release\examples\scan_bench.exe replay paths.nfm 7 > replay-baseline.csv
target\release\examples\scan_bench.exe scan G:\src 7 > scan-baseline.csv
```

After changing code and rebuilding, measure the candidate:

```powershell
target\release\examples\scan_bench.exe replay paths.nfm 7 > replay-current.csv
target\release\examples\scan_bench.exe scan G:\src 7 > scan-current.csv
target\release\examples\scan_bench.exe compare replay-baseline.csv replay-current.csv 10
target\release\examples\scan_bench.exe compare scan-baseline.csv scan-current.csv 10
```

The final argument is the allowed median regression percentage (default 10).
Compare exits nonzero if that threshold is exceeded or results are incompatible.
Check $LASTEXITCODE after every command; PowerShell does not automatically stop
on native command failure. Use PowerShell 7/UTF-8 redirection. Keep CSV output
outside scanned roots so creating it does not alter the dataset. Do not overwrite
known-good baselines with candidate results.

Comparison requires matching mode, dataset identity, available CPU count,
node/name/byte counts, and measured run count. Replay identity is a fixed checksum
of parent/name records, computed outside timing and independent of production
hashing. Live identity identifies the canonical root; matching counts cannot
detect every filesystem change. Use stable data and the same machine, power
settings, and build profile. CPU count alone does not identify a machine.

## Measurement scope

Both modes run one unreported warm-up followed by RUNS measurements (five by
default). CSV stdout reports individual durations, throughput, and counts;
stderr reports median/min/max. Counts must agree across repetitions.

Replay loads the dataset before timing, then constructs a fresh store per run,
inserts all nodes, publishes the usual periodic snapshots, and calls
complete_adding(). Input loading and final snapshot destruction are excluded.
The dataset stays in memory as one String per node, so allow extra replay memory.

Live scanning includes startup, enumeration, interning, snapshot delivery to a
minimal sink, and completion notification, without UI rendering. Worker count
and enumeration error handling use production defaults. Parallel insertion order
may vary. These are warm-cache measurements, not cold-disk measurements.

Use replay for storage regressions and scan for end-to-end impact. A passing
threshold is not proof of identical performance. Inspect the ranges and rerun
borderline results; alternate baseline/candidate binaries when noise is high.
Avoid debuggers and background workloads. No timing sink is installed. Use the
separate memory mode above for sampled private-memory measurements.
