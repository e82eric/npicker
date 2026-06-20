[CmdletBinding()]
param(
    [Alias('RootDirectories')]
    [string[]]$RootDirectory = @((Get-Location).Path),

    [string]$PipeName = 'nfm.picker.v1',

    [switch]$Win32,

    [int]$ConnectTimeoutMs = 2000,

    [int]$MaxDepth = [int]::MaxValue,

    [switch]$DirectoriesOnly,

    [switch]$FilesOnly,

    [switch]$ShowPreview,

    [switch]$WrapLines,

    [switch]$ShowGap,

    [string]$SearchString
)

Set-StrictMode -Version Latest

if ($Win32 -and $PSBoundParameters.ContainsKey('PipeName')) {
    Write-Error '-Win32 and -PipeName cannot be used together.'
    $global:LASTEXITCODE = 1
    return
}

if ($Win32) {
    $PipeName = 'nfm.win32.picker.v1'
}

if ($DirectoriesOnly -and $FilesOnly) {
    Write-Error '-DirectoriesOnly and -FilesOnly are mutually exclusive.'
    $global:LASTEXITCODE = 1
    return
}

$request = [ordered]@{
    command = 'filesystem'
    rootDirectories = @($RootDirectory | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    maxDepth = $MaxDepth
    directoriesOnly = [bool]$DirectoriesOnly
    filesOnly = [bool]$FilesOnly
    showPreview = [bool]$ShowPreview
    wrapLines = [bool]$WrapLines
    showGap = [bool]$ShowGap
}

if (-not [string]::IsNullOrWhiteSpace($SearchString)) {
    $request.searchString = $SearchString
}

$pipe = $null
$writer = $null
$reader = $null

try {
    $pipe = [System.IO.Pipes.NamedPipeClientStream]::new(
        '.',
        $PipeName,
        [System.IO.Pipes.PipeDirection]::InOut,
        [System.IO.Pipes.PipeOptions]::Asynchronous)

    $pipe.Connect($ConnectTimeoutMs)

    $utf8NoBom = [System.Text.UTF8Encoding]::new($false)
    $writer = [System.IO.StreamWriter]::new($pipe, $utf8NoBom, 1024, $true)
    $writer.WriteLine(($request | ConvertTo-Json -Compress -Depth 8))
    $writer.Flush()

    $reader = [System.IO.StreamReader]::new($pipe, [System.Text.Encoding]::UTF8, $false, 1024, $true)
    $line = $reader.ReadLine()
    if ([string]::IsNullOrWhiteSpace($line)) {
        Write-Error 'Picker server closed the connection without a response.'
        $global:LASTEXITCODE = 1
        return
    }

    $response = $line | ConvertFrom-Json
    switch ($response.status) {
        'selected' {
            $selected = $response.selectedItem
            if ([string]::IsNullOrEmpty($selected)) {
                $selected = $response.selectedPath
            }

            if ([string]::IsNullOrEmpty($selected)) {
                Write-Error 'Picker server returned selected without an item.'
                $global:LASTEXITCODE = 1
                return
            }

            $selected
            $global:LASTEXITCODE = 0
            return
        }
        'cancelled' {
            $global:LASTEXITCODE = 130
            return
        }
        'error' {
            $message = $response.errorMessage
            if ([string]::IsNullOrEmpty($message)) {
                $message = 'Picker server returned an error.'
            }

            Write-Error $message
            $global:LASTEXITCODE = 1
            return
        }
        default {
            Write-Error "Picker server returned an unknown status: $($response.status)"
            $global:LASTEXITCODE = 1
            return
        }
    }
}
catch [System.TimeoutException] {
    Write-Error "Picker server '$PipeName' is not running."
    $global:LASTEXITCODE = 1
}
catch {
    Write-Error "Failed to contact picker server '$PipeName': $($_.Exception.Message)"
    $global:LASTEXITCODE = 1
}
finally {
    if ($reader) {
        $reader.Dispose()
    }
    if ($writer) {
        $writer.Dispose()
    }
    if ($pipe) {
        $pipe.Dispose()
    }
}
