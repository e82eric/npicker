#Requires -Version 7.0
<#
.SYNOPSIS
Compare a Git revision with the current working tree using alternating runs.
.EXAMPLE
./nfm-file-system/Compare-ScanBenchmark.ps1 -Root G:\src
.EXAMPLE
./nfm-file-system/Compare-ScanBenchmark.ps1 -Recording ./paths.nfm -Base HEAD~1
.EXAMPLE
./nfm-file-system/Compare-ScanBenchmark.ps1 -Root G:\src -LiveScan
#>
[CmdletBinding()]
param(
    [string] $Base = 'HEAD',
    [string] $Root,
    [string] $Recording,
    [ValidateRange(1, 1000)] [int] $Runs = 7,
    [ValidateRange(0, 10000)] [double] $ThresholdPercent = 10,
    [switch] $LiveScan,
    [switch] $Memory,
    [string] $OutputDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
# Check native exit codes explicitly, including the expected compare failure.
$PSNativeCommandUseErrorActionPreference = $false

function Invoke-Checked {
    param([string] $Program, [string[]] $Arguments)
    & $Program @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Program failed (exit $LASTEXITCODE): $($Arguments -join ' ')" }
}

function Test-UnderPath([string] $Path, [string] $Directory) {
    $prefix = $Directory.TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    return $Path.Equals($Directory, [StringComparison]::OrdinalIgnoreCase) -or
        $Path.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)
}

$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$gitArgs = @('-c', "safe.directory=$($repo.Replace('\', '/'))", '-C', $repo)
$revision = (& git @gitArgs rev-parse --verify "$Base^{commit}").Trim()
if ($LASTEXITCODE -ne 0) { throw "Cannot resolve baseline revision '$Base'." }
$head = (& git @gitArgs rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0) { throw 'Cannot resolve current HEAD.' }
$initialStatus = @(& git @gitArgs status --porcelain=v1 --untracked-files=normal)
if ($LASTEXITCODE -ne 0) { throw 'Cannot inspect current working tree.' }
if (!$Root -and (!$Recording -or $LiveScan)) {
    throw 'Specify -Root to record a dataset or run live scans, or -Recording to replay an existing dataset.'
}
if ($Root) {
    $Root = (Resolve-Path -LiteralPath $Root).Path
    if (!(Test-Path -LiteralPath $Root -PathType Container)) { throw 'Root must be a directory.' }
}
if ($Recording) {
    $Recording = (Resolve-Path -LiteralPath $Recording).Path
    if (!(Test-Path -LiteralPath $Recording -PathType Leaf)) { throw 'Recording must be a file.' }
}
if (!$OutputDirectory) {
    $OutputDirectory = Join-Path ([IO.Path]::GetTempPath()) ('nfm-benchmark-' + [guid]::NewGuid().ToString('N'))
}
$output = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $output) { throw "Output directory must not already exist: $output" }
if (Test-UnderPath $output $repo) { throw 'Keep benchmark builds and output outside the source checkout.' }
if ($Root -and (Test-UnderPath $output $Root)) {
    throw 'Output directory must be outside the scanned root so build/output files do not affect the dataset.'
}
[void](New-Item -ItemType Directory -Path $output)
$worktree = Join-Path $output 'baseline-source'
$createdWorktree = $false
$failed = $false
$overlay = $false
$csvHeader = 'mode,dataset,cpus,run,seconds,nodes,unique_names,name_bytes,nodes_per_second'
if ($Memory) {
    $csvHeader += ',private_start_bytes,private_end_bytes,private_peak_bytes,private_end_delta_bytes,private_peak_delta_bytes,memory_samples'
}

try {
    Write-Host "Baseline: $revision ($Base)"
    Write-Host "Candidate: $head plus current working-tree changes"
    Write-Host "Artifacts: $output"
    Invoke-Checked git ($gitArgs + @('worktree', 'add', '--detach', $worktree, $revision))
    $createdWorktree = $true

    $baselinePackage = Join-Path $worktree 'nfm-file-system'
    $baselineExample = Join-Path $baselinePackage 'examples/scan_bench.rs'
    if (!(Test-Path -LiteralPath $baselineExample)) {
        # Bootstrap older revisions using only measurement code and its wiring.
        # No scan, storage, interning, or hash implementation is transplanted.
        $manifest = Join-Path $baselinePackage 'Cargo.toml'
        $manifestText = Get-Content -Raw -LiteralPath $manifest
        if ($manifestText -match '(?m)^\s*scan-bench\s*=' -or
            $manifestText -match '(?m)^\s*\[features\]') {
            throw 'Baseline has partial benchmark wiring; add a compatible harness to that revision before comparing.'
        }
        [void](New-Item -ItemType Directory -Force -Path (Join-Path $baselinePackage 'examples'))
        [void](New-Item -ItemType Directory -Force -Path (Join-Path $baselinePackage 'src/walker'))
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'examples/scan_bench.rs') -Destination $baselineExample
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'src/walker/benchmark.rs') -Destination (Join-Path $baselinePackage 'src/walker/benchmark.rs')
        Add-Content -LiteralPath $manifest -Value @'

[features]
scan-bench = []

[[example]]
name = "scan_bench"
required-features = ["scan-bench"]
'@
        Add-Content -LiteralPath (Join-Path $baselinePackage 'src/walker.rs') -Value @'

#[cfg(feature = "scan-bench")]
pub mod benchmark;
'@
        $overlay = $true
        Write-Host 'Baseline predates harness: added temporary harness-only overlay.'
    }

    $binaries = @{}
    foreach ($variant in @('baseline', 'current')) {
        $source = if ($variant -eq 'baseline') { $worktree } else { $repo }
        $target = Join-Path $output "$variant-target"
        Write-Host "Building $variant in release mode..."
        # Separate targets prevent cross-version artifact reuse. --locked keeps
        # builds from silently changing either dependency resolution or lockfile.
        Push-Location $source
        try {
            Invoke-Checked cargo @('build', '--locked', '--release', '-p', 'nfm_file_system',
                '--example', 'scan_bench', '--features', 'scan-bench', '--target-dir', $target)
        } finally { Pop-Location }
        $binaries[$variant] = Join-Path $target 'release/examples/scan_bench.exe'
        if (!(Test-Path -LiteralPath $binaries[$variant])) { throw "Missing benchmark binary: $variant" }
    }

    $metadata = [ordered]@{
        baseline = $revision; baselineExpression = $Base; currentHead = $head
        workingTreeStatus = $initialStatus; baselineHarnessOverlay = $overlay
        runs = $Runs; thresholdPercent = $ThresholdPercent; root = $Root
        memorySampling = [bool]$Memory; memoryIntervalMs = $(if ($Memory) { 10 } else { 0 })
        startedUtc = [DateTime]::UtcNow.ToString('o')
        rustc = @(& rustc -Vv); cargo = @(& cargo -V)
    }
    $metadata | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $output 'metadata.json') -Encoding utf8

    if (!$Recording) {
        $Recording = Join-Path $output 'paths.nfm'
        Write-Host 'Recording dataset once using current binary...'
        Invoke-Checked $binaries.current @('record', $Recording, $Root)
    }
    $modes = @('replay')
    if ($LiveScan) { $modes += 'scan' }
    foreach ($mode in $modes) {
        $inputPath = if ($mode -eq 'replay') { $Recording } else { $Root }
        $rows = @{ baseline = [Collections.Generic.List[string]]::new(); current = [Collections.Generic.List[string]]::new() }
        $rows.baseline.Add($csvHeader)
        $rows.current.Add($csvHeader)
        for ($iteration = 1; $iteration -le $Runs; $iteration++) {
            $order = if ($iteration % 2 -eq 1) { @('baseline', 'current') } else { @('current', 'baseline') }
            foreach ($variant in $order) {
                Write-Host "$mode pair $iteration/$Runs`: $variant"
                $log = Join-Path $output "$mode-$variant-$iteration.log"
                # Each process performs one warm-up plus one measured run.
                # Loading, process startup and compilation are not in its timer.
                $measurementArgs = @($mode, $inputPath, '1')
                if ($Memory) { $measurementArgs += '--memory' }
                $lines = @(& $binaries[$variant] @measurementArgs 2> $log)
                if ($LASTEXITCODE -ne 0) { throw "$variant $mode failed; see $log" }
                if ($lines.Count -ne 2 -or $lines[0] -ne $csvHeader) {
                    throw "Incompatible benchmark CSV from $variant. Both revisions need the current CSV protocol."
                }
                $fields = $lines[1].Split(',')
                $expectedFields = if ($Memory) { 15 } else { 9 }
                if ($fields.Length -ne $expectedFields -or $fields[0] -ne $mode -or $fields[3] -ne '1') {
                    throw "Invalid measurement from $variant"
                }
                $fields[3] = [string]$iteration
                $rows[$variant].Add(($fields -join ','))
                # Save progressively so failed runs still leave useful evidence.
                $rows[$variant] | Set-Content -LiteralPath (Join-Path $output "$mode-$variant.csv") -Encoding utf8
            }
        }
        $compareArgs = @('compare', (Join-Path $output "$mode-baseline.csv"),
            (Join-Path $output "$mode-current.csv"), $ThresholdPercent.ToString([Globalization.CultureInfo]::InvariantCulture))
        $comparison = @(& $binaries.current @compareArgs 2>&1)
        $comparisonExit = $LASTEXITCODE
        $comparison | Set-Content -LiteralPath (Join-Path $output "$mode-comparison.txt") -Encoding utf8
        $comparison | ForEach-Object { Write-Host $_ }
        if ($comparisonExit -ne 0) { $failed = $true }
    }
} finally {
    if ($createdWorktree) {
        # This is the exact worktree we created, inside our newly created output
        # directory. Force only discards its documented temporary harness overlay.
        $resolvedOutput = (Resolve-Path -LiteralPath $output).Path
        $resolvedWorktree = (Resolve-Path -LiteralPath $worktree).Path
        if (!(Test-UnderPath $resolvedWorktree $resolvedOutput) -or
            $resolvedWorktree -eq $resolvedOutput -or
            [IO.Path]::GetFileName($resolvedWorktree) -ne 'baseline-source') {
            throw 'Refusing cleanup: unexpected baseline worktree path.'
        }
        Invoke-Checked git ($gitArgs + @('worktree', 'remove', '--force', $resolvedWorktree))
    }
    Write-Host "Reports, binaries, and any recording retained at: $output"
}
if ($failed) { exit 1 }
