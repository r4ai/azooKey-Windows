[CmdletBinding()]
param([switch]$Force)

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "build-utils.ps1")

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$toolsRoot = Join-Path $repoRoot ".build-tools"
$downloadsRoot = Join-Path $toolsRoot "downloads"

$nodeVersion = "24.18.0"
$nodeFolderName = "node-v$nodeVersion-win-x64"
$nodeArchiveName = "$nodeFolderName.zip"
$nodeUrl = "https://nodejs.org/dist/v$nodeVersion/$nodeArchiveName"
$nodeSha256 = "0AE68406B42D7725661DA979B1403EC9926DA205C6770827F33AAC9D8F26E821"

$protocVersion = "29.6"
$protocArchiveName = "protoc-$protocVersion-win64.zip"
$protocUrl = "https://github.com/protocolbuffers/protobuf/releases/download/v$protocVersion/$protocArchiveName"
$protocSha256 = "1EBD7C87BAFFB9F1C47169B640872BF5FB1E4408079C691AF527BE9561D8F6F7"

$innoVersion = "6.7.3"
$innoInstallerName = "innosetup-$innoVersion.exe"
$innoUrl = "https://github.com/jrsoftware/issrc/releases/download/is-6_7_3/$innoInstallerName"
$innoSha256 = "9C73C3BAE7ED48D44112A0F48E66742C00090BDB5BEF71D9D3C056C66E97B732"

function Test-FileHash {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ExpectedSha256
    )

    return (Test-Path -LiteralPath $Path -PathType Leaf) -and
        ((Get-AzooKeyFileSha256 -Path $Path) -eq $ExpectedSha256)
}

function Remove-ToolDirectory {
    param([Parameter(Mandatory = $true)][string]$Path)

    $fullPath = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    $rootPrefix = $toolsRoot.TrimEnd('\') + '\'
    if (-not $fullPath.StartsWith($rootPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to remove a directory outside $toolsRoot`: $fullPath"
    }
    if ([IO.Directory]::Exists($fullPath)) {
        [IO.Directory]::Delete($fullPath, $true)
    }
}

function Get-VerifiedDownload {
    param(
        [Parameter(Mandatory = $true)][string]$Uri,
        [Parameter(Mandatory = $true)][string]$Destination,
        [Parameter(Mandatory = $true)][string]$ExpectedSha256
    )

    if (-not $Force -and (Test-FileHash -Path $Destination -ExpectedSha256 $ExpectedSha256)) {
        return
    }

    $temporaryPath = "$Destination.$([guid]::NewGuid().ToString('N')).tmp"
    try {
        Write-Host "Downloading $(Split-Path $Destination -Leaf)..."
        Invoke-WebRequest -UseBasicParsing -Uri $Uri -OutFile $temporaryPath
        if (-not (Test-FileHash -Path $temporaryPath -ExpectedSha256 $ExpectedSha256)) {
            throw "SHA-256 verification failed for $Uri."
        }
        Move-Item -LiteralPath $temporaryPath -Destination $Destination -Force
    }
    finally {
        Remove-Item -LiteralPath $temporaryPath -Force -ErrorAction SilentlyContinue
    }
}

function Add-PathDirectory {
    param([Parameter(Mandatory = $true)][string]$Path)

    $entries = $env:PATH -split ';'
    if ($entries -notcontains $Path) {
        $env:PATH = "$Path;$env:PATH"
    }
}

function Test-InnoInstallation {
    param([Parameter(Mandatory = $true)][string]$Path)

    $markerPath = Join-Path $Path ".azookey-complete"
    if (-not [IO.File]::Exists($markerPath)) {
        return $false
    }

    try {
        $metadata = @{}
        foreach ($line in [IO.File]::ReadAllLines($markerPath)) {
            $parts = $line -split '=', 2
            if ($parts.Count -eq 2) {
                $metadata[$parts[0]] = $parts[1]
            }
        }
        if ($metadata.version -ne $innoVersion -or
            $metadata.installerSha256 -ne $innoSha256) {
            return $false
        }

        $requiredFiles = @(
            "ISCC.exe",
            "ISCmplr.dll",
            "ISPPBuiltins.iss",
            "Setup.e32",
            "Default.isl",
            "Languages\Japanese.isl"
        )
        foreach ($relativePath in $requiredFiles) {
            $filePath = Join-Path $Path $relativePath
            if (-not [IO.File]::Exists($filePath) -or
                ([IO.FileInfo]$filePath).Length -eq 0) {
                return $false
            }
        }

        return $metadata.isccSha256 -eq
                (Get-AzooKeyFileSha256 -Path (Join-Path $Path "ISCC.exe")) -and
            $metadata.compilerSha256 -eq
                (Get-AzooKeyFileSha256 -Path (Join-Path $Path "ISCmplr.dll"))
    }
    catch {
        return $false
    }
}

New-Item -Path $downloadsRoot -ItemType Directory -Force | Out-Null

# Prefer an already active exact Node.js version (for example actions/setup-node
# in CI); otherwise use the repository-local verified archive.
$nodePath = $null
$npmPath = $null
$systemNode = Get-Command node.exe -ErrorAction SilentlyContinue
if ($null -ne $systemNode) {
    $systemNodeVersion = & $systemNode.Source --version 2>$null
}
if ($null -ne $systemNode -and $LASTEXITCODE -eq 0 -and $systemNodeVersion -eq "v$nodeVersion") {
    $candidateNpm = Join-Path (Split-Path $systemNode.Source -Parent) "npm.cmd"
    if (Test-Path -LiteralPath $candidateNpm -PathType Leaf) {
        $nodePath = $systemNode.Source
        $npmPath = $candidateNpm
    }
}
if ($null -eq $nodePath) {
    $nodeRoot = Join-Path $toolsRoot $nodeFolderName
    $nodePath = Join-Path $nodeRoot "node.exe"
    $npmPath = Join-Path $nodeRoot "npm.cmd"
    if ($Force -or
        -not (Test-Path -LiteralPath $nodePath -PathType Leaf) -or
        -not (Test-Path -LiteralPath $npmPath -PathType Leaf)) {
        $nodeArchive = Join-Path $downloadsRoot $nodeArchiveName
        Get-VerifiedDownload -Uri $nodeUrl -Destination $nodeArchive -ExpectedSha256 $nodeSha256
        $stagingRoot = Join-Path $toolsRoot (".node-" + [guid]::NewGuid().ToString("N"))
        try {
            Expand-AzooKeyZipArchive -ArchivePath $nodeArchive -DestinationPath $stagingRoot
            $extractedRoot = Join-Path $stagingRoot $nodeFolderName
            if (-not (Test-Path -LiteralPath (Join-Path $extractedRoot "node.exe") -PathType Leaf)) {
                throw "node.exe was not found after extracting $nodeArchiveName."
            }
            if (Test-Path -LiteralPath $nodeRoot -PathType Container) {
                Remove-ToolDirectory $nodeRoot
            }
            Move-Item -LiteralPath $extractedRoot -Destination $nodeRoot
        }
        finally {
            if (Test-Path -LiteralPath $stagingRoot -PathType Container) {
                Remove-ToolDirectory $stagingRoot
            }
        }
    }
}
$actualNodeVersion = & $nodePath --version
if ($LASTEXITCODE -ne 0 -or $actualNodeVersion -ne "v$nodeVersion") {
    throw "Node.js $nodeVersion verification failed at $nodePath."
}
if (-not (Test-Path -LiteralPath $npmPath -PathType Leaf)) {
    throw "npm.cmd was not found beside Node.js at $npmPath."
}
Add-PathDirectory (Split-Path $nodePath -Parent)
$env:AZOOKEY_NPM = $npmPath

# Prefer an exact protoc already configured by CI; otherwise use the verified
# repository-local archive.
$protocPath = $null
$systemProtoc = Get-Command protoc.exe -ErrorAction SilentlyContinue
if ($null -ne $systemProtoc) {
    $systemProtocVersion = & $systemProtoc.Source --version 2>$null
}
if ($null -ne $systemProtoc -and
    $LASTEXITCODE -eq 0 -and
    $systemProtocVersion -eq "libprotoc $protocVersion") {
    $protocPath = $systemProtoc.Source
}
if ($null -eq $protocPath) {
    $protocRoot = Join-Path $toolsRoot "protoc-$protocVersion"
    $protocPath = Join-Path $protocRoot "bin\protoc.exe"
    if ($Force -or -not (Test-Path -LiteralPath $protocPath -PathType Leaf)) {
        $protocArchive = Join-Path $downloadsRoot $protocArchiveName
        Get-VerifiedDownload -Uri $protocUrl -Destination $protocArchive -ExpectedSha256 $protocSha256
        $stagingRoot = Join-Path $toolsRoot (".protoc-" + [guid]::NewGuid().ToString("N"))
        try {
            Expand-AzooKeyZipArchive -ArchivePath $protocArchive -DestinationPath $stagingRoot
            if (-not (Test-Path -LiteralPath (Join-Path $stagingRoot "bin\protoc.exe") -PathType Leaf)) {
                throw "protoc.exe was not found after extracting $protocArchiveName."
            }
            if (Test-Path -LiteralPath $protocRoot -PathType Container) {
                Remove-ToolDirectory $protocRoot
            }
            Move-Item -LiteralPath $stagingRoot -Destination $protocRoot
        }
        finally {
            if (Test-Path -LiteralPath $stagingRoot -PathType Container) {
                Remove-ToolDirectory $stagingRoot
            }
        }
    }
}
$actualProtocVersion = & $protocPath --version
if ($LASTEXITCODE -ne 0 -or $actualProtocVersion -ne "libprotoc $protocVersion") {
    throw "protoc $protocVersion verification failed at $protocPath."
}
$env:PROTOC = $protocPath
Add-PathDirectory (Split-Path $protocPath -Parent)

# Always use the pinned portable Inno compiler so local and CI installers are
# parsed and emitted by the same compiler version.
$innoRoot = Join-Path $toolsRoot "inno-$innoVersion"
$isccPath = Join-Path $innoRoot "ISCC.exe"
if ($Force -or -not (Test-InnoInstallation -Path $innoRoot)) {
    $innoInstaller = Join-Path $downloadsRoot $innoInstallerName
    Get-VerifiedDownload -Uri $innoUrl -Destination $innoInstaller -ExpectedSha256 $innoSha256
    if ($null -eq (Get-Command Get-AuthenticodeSignature -ErrorAction SilentlyContinue)) {
        $securityModule = Join-Path $PSHOME `
            "Modules\Microsoft.PowerShell.Security\Microsoft.PowerShell.Security.psd1"
        Import-Module -Name $securityModule -ErrorAction Stop
    }
    $signature = Get-AuthenticodeSignature -LiteralPath $innoInstaller
    if ($signature.Status -ne "Valid" -or
        $signature.SignerCertificate.Subject -notmatch "Pyrsys B\.V\.") {
        throw "Inno Setup Authenticode verification failed: $($signature.Status)."
    }
    $id = [guid]::NewGuid().ToString("N")
    $stagingRoot = Join-Path $toolsRoot ".inno-$id"
    $backupRoot = Join-Path $toolsRoot ".inno-backup-$id"
    try {
        $process = Start-Process -FilePath $innoInstaller `
            -ArgumentList @(
                "/PORTABLE=1",
                "/VERYSILENT",
                "/CURRENTUSER",
                "/DIR=`"$stagingRoot`"",
                "/SUPPRESSMSGBOXES",
                "/NORESTART"
            ) `
            -WindowStyle Hidden -Wait -PassThru
        $stagingIscc = Join-Path $stagingRoot "ISCC.exe"
        $stagingCompiler = Join-Path $stagingRoot "ISCmplr.dll"
        if ($process.ExitCode -ne 0 -or
            -not [IO.File]::Exists($stagingIscc) -or
            -not [IO.File]::Exists($stagingCompiler)) {
            throw "Portable Inno Setup installation failed with exit code $($process.ExitCode)."
        }

        [IO.File]::WriteAllLines(
            (Join-Path $stagingRoot ".azookey-complete"),
            @(
                "version=$innoVersion",
                "installerSha256=$innoSha256",
                "isccSha256=$(Get-AzooKeyFileSha256 -Path $stagingIscc)",
                "compilerSha256=$(Get-AzooKeyFileSha256 -Path $stagingCompiler)"
            )
        )
        if (-not (Test-InnoInstallation -Path $stagingRoot)) {
            throw "Portable Inno Setup validation failed at $stagingRoot."
        }

        if (Test-Path -LiteralPath $innoRoot -PathType Container) {
            Move-Item -LiteralPath $innoRoot -Destination $backupRoot
        }
        try {
            Move-Item -LiteralPath $stagingRoot -Destination $innoRoot
        }
        catch {
            if (-not (Test-Path -LiteralPath $innoRoot) -and
                (Test-Path -LiteralPath $backupRoot -PathType Container)) {
                Move-Item -LiteralPath $backupRoot -Destination $innoRoot
            }
            throw
        }

        if (Test-Path -LiteralPath $backupRoot -PathType Container) {
            try {
                Remove-ToolDirectory $backupRoot
            }
            catch {
                Write-Warning "The previous Inno Setup remains at $backupRoot."
            }
        }
    }
    finally {
        if (Test-Path -LiteralPath $stagingRoot -PathType Container) {
            Remove-ToolDirectory $stagingRoot
        }
        if (-not (Test-Path -LiteralPath $innoRoot) -and
            (Test-Path -LiteralPath $backupRoot -PathType Container)) {
            Move-Item -LiteralPath $backupRoot -Destination $innoRoot
        }
    }
}
$env:AZOOKEY_ISCC = $isccPath
Add-PathDirectory $innoRoot

Write-Host "Build tools are ready:"
Write-Host "  Node.js: $nodePath"
Write-Host "  protoc:  $protocPath"
Write-Host "  ISCC:    $isccPath"
