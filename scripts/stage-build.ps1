[CmdletBinding()]
param(
    [switch]$KeepPreviousBuild,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$BuildArguments
)

$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$BuildArguments = @($BuildArguments | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
$unknownArguments = @($BuildArguments | Where-Object { $_ -notin @("--debug", "--release") })
if ($unknownArguments.Count -gt 0) {
    throw "Unsupported staging arguments: $($unknownArguments -join ', ')"
}
if ($BuildArguments -contains "--debug" -and $BuildArguments -contains "--release") {
    throw "Specify only one of --debug or --release."
}
$profile = if ($BuildArguments -contains "--release") { "release" } else { "debug" }

$swift = $env:AZOOKEY_SWIFT
if ([string]::IsNullOrWhiteSpace($swift)) {
    $swiftCommand = Get-Command swift.exe -ErrorAction SilentlyContinue
    if ($null -eq $swiftCommand) {
        throw "Swift was not found. Run this task through 'cargo make build' or add swift.exe to PATH."
    }
    $swift = $swiftCommand.Source
}
if (-not (Test-Path -LiteralPath $swift -PathType Leaf)) {
    throw "The configured Swift executable does not exist: $swift"
}

$workspaceRoot = $repoRoot.TrimEnd('\')
$buildDir = [IO.Path]::GetFullPath((Join-Path $workspaceRoot "build"))
$id = [guid]::NewGuid().ToString("N")
$stagingDir = Join-Path $workspaceRoot ".azookey-build-staging-$id"
$backupDir = Join-Path $workspaceRoot ".azookey-build-backup-$id"

function Remove-WorkspaceChildDirectory {
    param([Parameter(Mandatory = $true)][string]$Path)

    $fullPath = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    $parent = [IO.Directory]::GetParent($fullPath)
    if ($null -eq $parent -or
        -not $parent.FullName.TrimEnd('\').Equals(
            $workspaceRoot,
            [StringComparison]::OrdinalIgnoreCase
        )) {
        throw "Refusing to remove a directory outside the workspace: $fullPath"
    }
    if ([IO.Directory]::Exists($fullPath)) {
        [IO.Directory]::Delete($fullPath, $true)
    }
}

function Copy-RequiredItem {
    param(
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$Destination,
        [switch]$Recurse
    )

    if (-not (Test-Path -LiteralPath $Source)) {
        throw "Required build output was not found: $Source"
    }
    Copy-Item -LiteralPath $Source -Destination $Destination -Recurse:$Recurse -Force
}

try {
    New-Item -Path (Join-Path $stagingDir "x86") -ItemType Directory -Force | Out-Null

    Copy-RequiredItem (Join-Path $workspaceRoot "target\$profile\ui.exe") $stagingDir
    Copy-RequiredItem (Join-Path $workspaceRoot "target\$profile\azookey-server.exe") $stagingDir
    Copy-RequiredItem (Join-Path $workspaceRoot "target\$profile\azookey_windows.dll") $stagingDir
    Copy-RequiredItem (Join-Path $workspaceRoot "target\$profile\launcher.exe") $stagingDir
    Copy-RequiredItem `
        (Join-Path $workspaceRoot "target\i686-pc-windows-msvc\$profile\azookey_windows.dll") `
        (Join-Path $stagingDir "x86")
    Copy-RequiredItem `
        (Join-Path $workspaceRoot "server-swift\.build\x86_64-unknown-windows-msvc\release\azookey-server.dll") `
        $stagingDir

    Copy-RequiredItem (Join-Path $workspaceRoot "llama_cpu") (Join-Path $stagingDir "llama_cpu") -Recurse
    Copy-RequiredItem (Join-Path $workspaceRoot "llama_cuda") (Join-Path $stagingDir "llama_cuda") -Recurse
    Copy-RequiredItem (Join-Path $workspaceRoot "llama_vulkan") (Join-Path $stagingDir "llama_vulkan") -Recurse

    $swiftTargetInfoJson = & $swift -print-target-info
    if ($LASTEXITCODE -ne 0) {
        throw "$swift -print-target-info failed with exit code $LASTEXITCODE."
    }
    $swiftTargetInfo = $swiftTargetInfoJson | Out-String | ConvertFrom-Json
    $swiftRuntimeBin = @($swiftTargetInfo.paths.runtimeLibraryPaths) |
        Where-Object { Test-Path -LiteralPath (Join-Path $_ "swiftCore.dll") } |
        Select-Object -First 1
    if ([string]::IsNullOrWhiteSpace($swiftRuntimeBin)) {
        throw "Could not find the runtime bin directory for $swift."
    }
    Get-ChildItem -LiteralPath $swiftRuntimeBin -Force |
        Copy-Item -Destination $stagingDir -Recurse -Force

    Copy-RequiredItem `
        (Join-Path $workspaceRoot "server-swift\azooKey_emoji_dictionary_storage\EmojiDictionary") `
        (Join-Path $stagingDir "EmojiDictionary") -Recurse
    Copy-RequiredItem `
        (Join-Path $workspaceRoot "server-swift\azooKey_dictionary_storage\Dictionary") `
        (Join-Path $stagingDir "Dictionary") -Recurse
    Copy-RequiredItem (Join-Path $workspaceRoot "zenz.gguf") $stagingDir

    & "$env:WINDIR\System32\icacls.exe" `
        (Join-Path $stagingDir "azookey_windows.dll") /grant "*S-1-15-2-1:(RX)"
    if ($LASTEXITCODE -ne 0) {
        throw "icacls failed for the x64 IME DLL with exit code $LASTEXITCODE."
    }
    & "$env:WINDIR\System32\icacls.exe" `
        (Join-Path $stagingDir "x86\azookey_windows.dll") /grant "*S-1-15-2-1:(RX)"
    if ($LASTEXITCODE -ne 0) {
        throw "icacls failed for the x86 IME DLL with exit code $LASTEXITCODE."
    }

    if ([IO.Directory]::Exists($buildDir)) {
        Move-Item -LiteralPath $buildDir -Destination $backupDir
    }
    try {
        Move-Item -LiteralPath $stagingDir -Destination $buildDir
    }
    catch {
        if (-not (Test-Path -LiteralPath $buildDir) -and
            (Test-Path -LiteralPath $backupDir -PathType Container)) {
            Move-Item -LiteralPath $backupDir -Destination $buildDir
        }
        throw
    }

    if (Test-Path -LiteralPath $backupDir -PathType Container) {
        if ($KeepPreviousBuild) {
            $env:AZOOKEY_PREVIOUS_BUILD = $backupDir
        }
        else {
            try {
                Remove-WorkspaceChildDirectory $backupDir
            }
            catch {
                Write-Warning "The previous build remains at $backupDir and can be removed after closing processes that use it."
            }
        }
    }
}
finally {
    if (Test-Path -LiteralPath $stagingDir -PathType Container) {
        Remove-WorkspaceChildDirectory $stagingDir
    }
}

Write-Host "Staged $profile build at $buildDir"
