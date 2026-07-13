[CmdletBinding()]
param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$BuildArguments
)

$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$BuildArguments = @($BuildArguments | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
$unknownArguments = @($BuildArguments | Where-Object { $_ -notin @("--debug", "--release") })
if ($unknownArguments.Count -gt 0) {
    throw "Unsupported build arguments: $($unknownArguments -join ', ')"
}
if ($BuildArguments -contains "--debug" -and $BuildArguments -contains "--release") {
    throw "Specify only one of --debug or --release."
}
$isRelease = $BuildArguments -contains "--release"
$profile = if ($isRelease) { "release" } else { "debug" }
$cargoProfileArguments = if ($isRelease) { @("--release") } else { @() }

function Add-PathDirectory {
    param([Parameter(Mandatory = $true)][string]$Path)

    if (($env:PATH -split ';') -notcontains $Path) {
        $env:PATH = "$Path;$env:PATH"
    }
}

function Invoke-Checked {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$ArgumentList = @(),
        [string]$WorkingDirectory = $repoRoot
    )

    Push-Location $WorkingDirectory
    try {
        & $FilePath @ArgumentList
        if ($LASTEXITCODE -ne 0) {
            throw "$FilePath exited with code $LASTEXITCODE."
        }
    }
    finally {
        Pop-Location
    }
}

function Resolve-RequiredCommand {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$InstallHint
    )

    $command = Get-Command $Name -ErrorAction SilentlyContinue
    if ($null -eq $command) {
        throw "$Name was not found. $InstallHint"
    }
    return $command.Source
}

function Resolve-SwiftExecutable {
    function Test-SwiftCandidate {
        param([Parameter(Mandatory = $true)][string]$Path)

        try {
            $versionText = (& $Path --version 2>$null) -join "`n"
            return $LASTEXITCODE -eq 0 -and
                $versionText -match 'Swift version\s+(\d+\.\d+)' -and
                [version]$matches[1] -ge [version]"6.1"
        }
        catch {
            return $false
        }
    }

    $command = Get-Command swift.exe -ErrorAction SilentlyContinue
    if ($null -ne $command -and (Test-SwiftCandidate -Path $command.Source)) {
        return $command.Source
    }

    $toolchainsRoot = Join-Path $env:LOCALAPPDATA "Programs\Swift\Toolchains"
    if (Test-Path -LiteralPath $toolchainsRoot -PathType Container) {
        $candidates = Get-ChildItem -LiteralPath $toolchainsRoot -Directory |
            Sort-Object Name -Descending |
            ForEach-Object { Join-Path $_.FullName "usr\bin\swift.exe" } |
            Where-Object { Test-Path -LiteralPath $_ -PathType Leaf }
        foreach ($candidate in $candidates) {
            if (Test-SwiftCandidate -Path $candidate) {
                return $candidate
            }
        }
    }

    throw "Swift was not found. Install Swift for Windows 6.1 or newer."
}

function Resolve-MsvcDevCmd {
    if (-not [string]::IsNullOrEmpty($env:VSCMD_VER) -and
        -not [string]::IsNullOrEmpty($env:INCLUDE) -and
        -not [string]::IsNullOrEmpty($env:LIB) -and
        $env:VSCMD_ARG_TGT_ARCH -eq "x64") {
        return $null
    }

    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) {
        throw "vswhere.exe was not found. Install Visual Studio 2022 Build Tools with Desktop development with C++."
    }
    $installationPath = (& $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath | Select-Object -First 1)
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($installationPath)) {
        throw "A Visual Studio installation containing the MSVC x86/x64 tools was not found."
    }
    $vsDevCmd = Join-Path $installationPath "Common7\Tools\VsDevCmd.bat"
    if (-not (Test-Path -LiteralPath $vsDevCmd -PathType Leaf)) {
        throw "VsDevCmd.bat was not found at $vsDevCmd."
    }
    return $vsDevCmd
}

function Import-MsvcEnvironment {
    param([AllowNull()][string]$VsDevCmd)

    if ([string]::IsNullOrWhiteSpace($VsDevCmd)) {
        return
    }

    # VsDevCmd exports its own PSModulePath. Keeping it would hide built-in
    # Windows PowerShell modules such as Microsoft.PowerShell.Utility.
    $powerShellModulePath = $env:PSModulePath
    $environmentLines = & $env:COMSPEC /d /s /c `
        "`"$VsDevCmd`" -no_logo -arch=x64 -host_arch=x64 >nul && set"
    if ($LASTEXITCODE -ne 0) {
        throw "VsDevCmd.bat failed with exit code $LASTEXITCODE."
    }
    foreach ($line in $environmentLines) {
        if ($line -match '^([^=]+)=(.*)$') {
            if ($matches[1] -ieq "PSModulePath") {
                continue
            }
            Set-Item -Path "Env:$($matches[1])" -Value $matches[2]
        }
    }
    $env:PSModulePath = $powerShellModulePath
}

$cargo = Resolve-RequiredCommand -Name "cargo.exe" -InstallHint "Install Rust with the MSVC toolchain."
$rustup = Resolve-RequiredCommand -Name "rustup.exe" -InstallHint "Install Rust with rustup."
$null = Resolve-RequiredCommand -Name "git.exe" -InstallHint "Install Git for Windows."
$swift = Resolve-SwiftExecutable
$vsDevCmd = Resolve-MsvcDevCmd

# Fail on a missing Rust target before downloading the larger build assets.
Invoke-Checked -FilePath $rustup -ArgumentList @("target", "add", "i686-pc-windows-msvc")

& (Join-Path $PSScriptRoot "setup-build-tools.ps1")

$npm = $env:AZOOKEY_NPM
$iscc = $env:AZOOKEY_ISCC
if (-not (Test-Path -LiteralPath $npm -PathType Leaf)) {
    throw "npm.cmd was not configured by setup-build-tools.ps1."
}
if (-not (Test-Path -LiteralPath $iscc -PathType Leaf)) {
    throw "ISCC.exe was not configured by setup-build-tools.ps1."
}

Write-Host "Building AzooKey ($profile)..."
& (Join-Path $PSScriptRoot "setup-build-assets.ps1")
Invoke-Checked -FilePath $cargo -ArgumentList @("fmt", "--all", "--", "--check")

# Asset verification uses Windows PowerShell modules that should be loaded
# before VsDevCmd adjusts the compiler environment.
Import-MsvcEnvironment -VsDevCmd $vsDevCmd
Add-PathDirectory (Split-Path $swift -Parent)
$env:AZOOKEY_SWIFT = $swift

$serverSwift = Join-Path $repoRoot "server-swift"
Invoke-Checked -FilePath $swift -ArgumentList @("package", "resolve") -WorkingDirectory $serverSwift
& (Join-Path $PSScriptRoot "patch-kkc.ps1") -PackagePath $serverSwift
Invoke-Checked -FilePath $swift `
    -ArgumentList @("build", "-c", "release", "--disable-automatic-resolution") `
    -WorkingDirectory $serverSwift
$swiftLibrary = Join-Path $serverSwift ".build\x86_64-unknown-windows-msvc\release\azookey-server.lib"
if (-not (Test-Path -LiteralPath $swiftLibrary -PathType Leaf)) {
    throw "Swift build succeeded but did not produce $swiftLibrary."
}
$libraryDestination = Join-Path $repoRoot "azookey-server.lib"
$temporaryLibrary = "$libraryDestination.$([guid]::NewGuid().ToString('N')).tmp"
try {
    Copy-Item -LiteralPath $swiftLibrary -Destination $temporaryLibrary
    Move-Item -LiteralPath $temporaryLibrary -Destination $libraryDestination -Force
}
finally {
    Remove-Item -LiteralPath $temporaryLibrary -Force -ErrorAction SilentlyContinue
}

Invoke-Checked -FilePath $cargo -ArgumentList (@("build") + $cargoProfileArguments)
Invoke-Checked -FilePath $cargo `
    -ArgumentList (@("build", "-p", "azookey-windows", "--target=i686-pc-windows-msvc") + $cargoProfileArguments)

$frontend = Join-Path $repoRoot "frontend"
Invoke-Checked -FilePath $npm -ArgumentList @("ci") -WorkingDirectory $frontend
$tauriArguments = @("run", "tauri", "--", "build")
if (-not $isRelease) {
    $tauriArguments += "--debug"
}
Invoke-Checked -FilePath $npm -ArgumentList $tauriArguments -WorkingDirectory $frontend

$startupValidationVbs = Join-Path $repoRoot `
    (".build-tools\startup-task-validation-{0}.vbs" -f [guid]::NewGuid().ToString("N"))
try {
    Invoke-Checked -FilePath (Join-Path $env:WINDIR "System32\WindowsPowerShell\v1.0\powershell.exe") `
        -ArgumentList @(
            "-NoProfile",
            "-ExecutionPolicy", "Bypass",
            "-File", (Join-Path $repoRoot "installer\SetupStartupTask.ps1"),
            "-TaskXmlTemplatePath", (Join-Path $repoRoot "installer\Azookey Startup.xml"),
            "-LaunchVbsPath", $startupValidationVbs,
            "-LauncherPath", (Join-Path $repoRoot "target\$profile\launcher.exe"),
            "-TaskName", "Azookey Startup Validation",
            "-ValidateOnly"
        )
}
finally {
    Remove-Item -LiteralPath $startupValidationVbs -Force -ErrorAction SilentlyContinue
}

$env:AZOOKEY_PREVIOUS_BUILD = $null
& (Join-Path $PSScriptRoot "stage-build.ps1") -KeepPreviousBuild @BuildArguments
$previousBuild = $env:AZOOKEY_PREVIOUS_BUILD
$installer = Join-Path $repoRoot "build\azookey-setup.exe"

function Assert-PreviousBuildPath {
    param([Parameter(Mandatory = $true)][string]$Path)

    $fullPath = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    $parent = [IO.Directory]::GetParent($fullPath)
    if ($null -eq $parent -or
        -not $parent.FullName.TrimEnd('\').Equals($repoRoot.TrimEnd('\'), [StringComparison]::OrdinalIgnoreCase) -or
        -not [IO.Path]::GetFileName($fullPath).StartsWith(
            ".azookey-build-backup-",
            [StringComparison]::OrdinalIgnoreCase
        )) {
        throw "Invalid previous-build path: $fullPath"
    }
    return $fullPath
}

try {
    Invoke-Checked -FilePath $iscc -ArgumentList @(
        "/DBuildProfile=$profile",
        (Join-Path $repoRoot "installer\Installer.iss")
    )
    if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) {
        throw "The installer was not produced at $installer."
    }
}
catch {
    $packagingError = $_
    $buildDirectory = [IO.Path]::GetFullPath((Join-Path $repoRoot "build"))
    if ([IO.Directory]::Exists($buildDirectory)) {
        [IO.Directory]::Delete($buildDirectory, $true)
    }
    if (-not [string]::IsNullOrWhiteSpace($previousBuild)) {
        $previousBuild = Assert-PreviousBuildPath -Path $previousBuild
        Move-Item -LiteralPath $previousBuild -Destination $buildDirectory
    }
    throw $packagingError
}

if (-not [string]::IsNullOrWhiteSpace($previousBuild)) {
    $previousBuild = Assert-PreviousBuildPath -Path $previousBuild
    if ([IO.Directory]::Exists($previousBuild)) {
        try {
            [IO.Directory]::Delete($previousBuild, $true)
        }
        catch {
            Write-Warning "The previous build remains at $previousBuild and can be removed after closing processes that use it."
        }
    }
}
Write-Host "Build complete: $installer"
