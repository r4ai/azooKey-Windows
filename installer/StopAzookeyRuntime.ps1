[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$InstallDirectory,

    [Parameter(Mandatory = $true)]
    [string]$TaskName
)

$ErrorActionPreference = "Stop"

function Get-AzookeyRuntimeProcess {
    param(
        [Parameter(Mandatory = $true)]
        [Collections.Generic.HashSet[string]]$TargetPaths
    )

    $processes = Get-Process -Name @("launcher", "azookey-server", "ui") `
        -ErrorAction SilentlyContinue
    foreach ($process in $processes) {
        try {
            $processPath = [IO.Path]::GetFullPath($process.Path)
        }
        catch {
            continue
        }

        if ($TargetPaths.Contains($processPath)) {
            $process
        }
    }
}

try {
    $installDirectoryFullPath = [IO.Path]::GetFullPath($InstallDirectory).TrimEnd('\')
    $targetPaths = [Collections.Generic.HashSet[string]]::new(
        [StringComparer]::OrdinalIgnoreCase
    )
    foreach ($fileName in @("launcher.exe", "azookey-server.exe", "ui.exe")) {
        $null = $targetPaths.Add((Join-Path $installDirectoryFullPath $fileName))
    }

    $schtasks = Join-Path $env:WINDIR "System32\schtasks.exe"
    & $schtasks /Query /TN $TaskName
    if ($LASTEXITCODE -eq 0) {
        & $schtasks /End /TN $TaskName
        # A Ready task returns a nonzero status because it has no running instance. That is
        # already the desired state; SetupStartupTask.ps1 overwrites it with /Create /F later.
    }

    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    do {
        $running = @(Get-AzookeyRuntimeProcess -TargetPaths $targetPaths)
        foreach ($process in $running) {
            try {
                $process.Kill()
            }
            catch [InvalidOperationException] {
                # The process exited between enumeration and termination.
            }
        }

        if ($running.Count -gt 0) {
            Start-Sleep -Milliseconds 100
        }
    } while ($running.Count -gt 0 -and [DateTime]::UtcNow -lt $deadline)

    $running = @(Get-AzookeyRuntimeProcess -TargetPaths $targetPaths)
    if ($running.Count -gt 0) {
        $descriptions = $running | ForEach-Object { "{0} (PID {1})" -f $_.Path, $_.Id }
        throw "AzooKey runtime processes did not stop: $($descriptions -join ', ')."
    }
}
catch {
    $errorText = ($_ | Out-String).Trim()
    [Console]::Error.WriteLine($errorText)
    exit 1
}

exit 0
