[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$BuildDirectory,
    [ValidateRange(1, 120)]
    [int]$StartupTimeoutSeconds = 30,
    [ValidateRange(100, 10000)]
    [int]$MinimumLifetimeMilliseconds = 1000
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$buildDirectory = (Resolve-Path -LiteralPath $BuildDirectory).Path
$serverExecutable = Join-Path $buildDirectory "azookey-server.exe"
$cpuBackendDirectory = Join-Path $buildDirectory "llama_cpu"
if (-not (Test-Path -LiteralPath $serverExecutable -PathType Leaf)) {
    throw "Server smoke test executable was not found: $serverExecutable"
}
if (-not (Test-Path -LiteralPath $cpuBackendDirectory -PathType Container)) {
    throw "Server smoke test CPU backend was not found: $cpuBackendDirectory"
}

$id = [guid]::NewGuid().ToString("N")
$pipeName = "azookey_server_smoke_$id"
$tempBase = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')
$tempRoot = Join-Path $tempBase "azookey-server-smoke-$id"
$appDataDirectory = Join-Path $tempRoot "AppData"
$settingsDirectory = Join-Path $appDataDirectory "Azookey"
$stdoutPath = Join-Path $tempRoot "stdout.log"
$stderrPath = Join-Path $tempRoot "stderr.log"

$process = $null
$failureMessage = $null
$cleanupMessage = $null
$stdout = ""
$stderr = ""

function Stop-SmokeProcess {
    param([AllowNull()][Diagnostics.Process]$Process)

    if ($null -eq $Process) {
        return
    }
    $Process.Refresh()
    if (-not $Process.HasExited) {
        try {
            $Process.Kill()
        }
        catch [InvalidOperationException] {
            # The process can exit between Refresh and Kill.
            $Process.Refresh()
            if (-not $Process.HasExited) {
                throw
            }
        }
        if (-not $Process.WaitForExit(5000)) {
            throw "Timed out while stopping server smoke-test process $($Process.Id)."
        }
    }
}

function Remove-SmokeDirectory {
    param([Parameter(Mandatory = $true)][string]$Path)

    $fullPath = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    $parent = [IO.Directory]::GetParent($fullPath)
    if ($null -eq $parent -or
        -not $parent.FullName.TrimEnd('\').Equals(
            $tempBase,
            [StringComparison]::OrdinalIgnoreCase
        ) -or
        -not [IO.Path]::GetFileName($fullPath).StartsWith(
            "azookey-server-smoke-",
            [StringComparison]::Ordinal
        )) {
        throw "Refusing to remove an unexpected smoke-test directory: $fullPath"
    }
    if ([IO.Directory]::Exists($fullPath)) {
        [IO.Directory]::Delete($fullPath, $true)
    }
}

try {
    New-Item -Path $settingsDirectory -ItemType Directory -Force | Out-Null
    $settings = @{
        version = "0.1.0"
        zenzai = @{
            enable = $false
            profile = ""
            backend = "cpu"
        }
    } | ConvertTo-Json -Depth 3
    [IO.File]::WriteAllText(
        (Join-Path $settingsDirectory "settings.json"),
        $settings,
        [Text.UTF8Encoding]::new($false)
    )

    $previousAppData = $env:APPDATA
    $previousPath = $env:PATH
    $previousPipeName = $env:AZOOKEY_SERVER_SMOKE_TEST_PIPE_NAME
    try {
        $env:APPDATA = $appDataDirectory
        $env:PATH = "$cpuBackendDirectory;$buildDirectory;$previousPath"
        $env:AZOOKEY_SERVER_SMOKE_TEST_PIPE_NAME = $pipeName
        $process = Start-Process `
            -FilePath $serverExecutable `
            -WorkingDirectory $buildDirectory `
            -WindowStyle Hidden `
            -RedirectStandardOutput $stdoutPath `
            -RedirectStandardError $stderrPath `
            -PassThru
    }
    finally {
        $env:APPDATA = $previousAppData
        $env:PATH = $previousPath
        $env:AZOOKEY_SERVER_SMOKE_TEST_PIPE_NAME = $previousPipeName
    }

    $startupTimer = [Diagnostics.Stopwatch]::StartNew()
    $connected = $false
    while ($startupTimer.Elapsed.TotalSeconds -lt $StartupTimeoutSeconds) {
        $process.Refresh()
        if ($process.HasExited) {
            $process.WaitForExit()
            throw "Server exited with code $($process.ExitCode) before opening pipe '$pipeName'."
        }

        $pipeClient = [IO.Pipes.NamedPipeClientStream]::new(
            ".",
            $pipeName,
            [IO.Pipes.PipeDirection]::InOut,
            [IO.Pipes.PipeOptions]::Asynchronous
        )
        try {
            try {
                $pipeClient.Connect(100)
                $connected = $true
            }
            catch [TimeoutException] {
                # Initialization can include dictionary preload; retry until the deadline.
            }
        }
        finally {
            $pipeClient.Dispose()
        }

        if ($connected) {
            break
        }
        Start-Sleep -Milliseconds 50
    }
    if (-not $connected) {
        throw "Server did not open pipe '$pipeName' within $StartupTimeoutSeconds seconds."
    }

    $lifetimeTimer = [Diagnostics.Stopwatch]::StartNew()
    while ($lifetimeTimer.ElapsedMilliseconds -lt $MinimumLifetimeMilliseconds) {
        $process.Refresh()
        if ($process.HasExited) {
            $process.WaitForExit()
            throw "Server exited with code $($process.ExitCode) after accepting a pipe connection."
        }
        Start-Sleep -Milliseconds 50
    }
}
catch {
    $failureMessage = $_.Exception.Message
}
finally {
    try {
        Stop-SmokeProcess -Process $process
    }
    catch {
        $cleanupMessage = $_.Exception.Message
    }

    try {
        if (Test-Path -LiteralPath $stdoutPath -PathType Leaf) {
            $stdout = [IO.File]::ReadAllText($stdoutPath).Trim()
        }
        if (Test-Path -LiteralPath $stderrPath -PathType Leaf) {
            $stderr = [IO.File]::ReadAllText($stderrPath).Trim()
        }
    }
    catch {
        if ([string]::IsNullOrWhiteSpace($cleanupMessage)) {
            $cleanupMessage = "Failed to read smoke-test logs: $($_.Exception.Message)"
        }
        else {
            $cleanupMessage = "$cleanupMessage Failed to read smoke-test logs: $($_.Exception.Message)"
        }
    }

    try {
        Remove-SmokeDirectory -Path $tempRoot
    }
    catch {
        if ([string]::IsNullOrWhiteSpace($cleanupMessage)) {
            $cleanupMessage = $_.Exception.Message
        }
        else {
            $cleanupMessage = "$cleanupMessage $($_.Exception.Message)"
        }
    }

    if ($null -ne $process) {
        $process.Dispose()
    }
}

if (-not [string]::IsNullOrWhiteSpace($failureMessage)) {
    $details = @($failureMessage)
    if (-not [string]::IsNullOrWhiteSpace($stdout)) {
        $details += "stdout: $stdout"
    }
    if (-not [string]::IsNullOrWhiteSpace($stderr)) {
        $details += "stderr: $stderr"
    }
    if (-not [string]::IsNullOrWhiteSpace($cleanupMessage)) {
        $details += "cleanup: $cleanupMessage"
    }
    throw ($details -join [Environment]::NewLine)
}
if (-not [string]::IsNullOrWhiteSpace($cleanupMessage)) {
    throw "Server smoke test cleanup failed: $cleanupMessage"
}

Write-Host "Server smoke test passed: $serverExecutable"
