[CmdletBinding()]
param([switch]$Force)

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "build-utils.ps1")

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$llamaVersion = "b4846"
$llamaReleaseBase = "https://github.com/fkunn1326/llama.cpp/releases/download/$llamaVersion"
$zenzRevision = "d48369e21adb9f49903eb7c54be1a1d9723eb805"
$zenzUrl = "https://huggingface.co/Miwa-Keita/zenz-v3-small-gguf/resolve/$zenzRevision/ggml-model-Q5_K_M.gguf"
$zenzSha256 = "501F605D088F5B988791A00AE19ED46985ED7C48144F364B2F3F1F951C9B2083"

function Test-UsableFile {
    param([Parameter(Mandatory = $true)][string]$Path)

    return (Test-Path -LiteralPath $Path -PathType Leaf) -and
        ((Get-Item -LiteralPath $Path).Length -gt 0)
}

function Test-FileHash {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ExpectedSha256
    )

    if (-not (Test-UsableFile $Path)) {
        return $false
    }

    $actualSha256 = Get-AzooKeyFileSha256 -Path $Path
    return $actualSha256 -eq $ExpectedSha256
}

function Remove-GeneratedDirectory {
    param([Parameter(Mandatory = $true)][string]$Path)

    $fullPath = [System.IO.Path]::GetFullPath($Path).TrimEnd('\')
    $parentPath = [System.IO.Directory]::GetParent($fullPath).FullName.TrimEnd('\')
    if (-not $parentPath.Equals($repoRoot.TrimEnd('\'), [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to remove a directory outside the repository root: $fullPath"
    }

    if ([System.IO.Directory]::Exists($fullPath)) {
        [System.IO.Directory]::Delete($fullPath, $true)
    }
}

function Test-LlamaInstallation {
    param(
        [Parameter(Mandatory = $true)][string]$DestinationPath,
        [Parameter(Mandatory = $true)][string]$ArchiveSha256,
        [Parameter(Mandatory = $true)][string[]]$ExpectedFiles
    )

    $manifestPath = Join-Path $DestinationPath ".azookey-manifest.json"
    if (-not (Test-UsableFile $manifestPath)) {
        return $false
    }

    try {
        $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
        if ($manifest.archiveSha256 -ne $ArchiveSha256) {
            return $false
        }

        foreach ($expectedFile in $ExpectedFiles) {
            $fileProperty = $manifest.files.PSObject.Properties[$expectedFile]
            if ($null -eq $fileProperty -or
                -not (Test-FileHash `
                    -Path (Join-Path $DestinationPath $expectedFile) `
                    -ExpectedSha256 $fileProperty.Value)) {
                return $false
            }
        }
        return $true
    }
    catch {
        return $false
    }
}

function Write-LlamaManifest {
    param(
        [Parameter(Mandatory = $true)][string]$DestinationPath,
        [Parameter(Mandatory = $true)][string]$ArchiveSha256,
        [Parameter(Mandatory = $true)][string[]]$ExpectedFiles
    )

    $fileHashes = [ordered]@{}
    foreach ($expectedFile in $ExpectedFiles) {
        $fileHashes[$expectedFile] = Get-AzooKeyFileSha256 `
            -Path (Join-Path $DestinationPath $expectedFile)
    }
    $manifest = [ordered]@{
        archiveSha256 = $ArchiveSha256
        files = $fileHashes
    } | ConvertTo-Json -Depth 3
    [System.IO.File]::WriteAllText(
        (Join-Path $DestinationPath ".azookey-manifest.json"),
        $manifest
    )
}

function Install-LlamaArchive {
    param(
        [Parameter(Mandatory = $true)][string]$ArchiveName,
        [Parameter(Mandatory = $true)][string]$ArchiveSha256,
        [Parameter(Mandatory = $true)][string]$Destination,
        [Parameter(Mandatory = $true)][string[]]$ExpectedFiles
    )

    $destinationPath = Join-Path $repoRoot $Destination
    if (-not $Force -and (Test-LlamaInstallation `
        -DestinationPath $destinationPath `
        -ArchiveSha256 $ArchiveSha256 `
        -ExpectedFiles $ExpectedFiles)) {
        Write-Host "Using verified $Destination assets."
        return
    }

    $id = [guid]::NewGuid().ToString("N")
    $temporaryZip = Join-Path ([System.IO.Path]::GetTempPath()) "azookey-$id.zip"
    $stagingPath = Join-Path $repoRoot ".azookey-assets-$id"
    $backupPath = Join-Path $repoRoot ".azookey-backup-$id"
    try {
        Write-Host "Downloading $ArchiveName..."
        Invoke-WebRequest -UseBasicParsing -Uri "$llamaReleaseBase/$ArchiveName" -OutFile $temporaryZip
        if (-not (Test-FileHash -Path $temporaryZip -ExpectedSha256 $ArchiveSha256)) {
            throw "SHA-256 verification failed for $ArchiveName."
        }

        New-Item -Path $stagingPath -ItemType Directory | Out-Null
        Expand-AzooKeyZipArchive -ArchivePath $temporaryZip -DestinationPath $stagingPath
        foreach ($expectedFile in $ExpectedFiles) {
            if (-not (Test-UsableFile (Join-Path $stagingPath $expectedFile))) {
                throw "$expectedFile was not found after extracting $ArchiveName."
            }
        }
        Write-LlamaManifest `
            -DestinationPath $stagingPath `
            -ArchiveSha256 $ArchiveSha256 `
            -ExpectedFiles $ExpectedFiles

        if (Test-Path -LiteralPath $destinationPath -PathType Container) {
            Move-Item -LiteralPath $destinationPath -Destination $backupPath
        }
        try {
            Move-Item -LiteralPath $stagingPath -Destination $destinationPath
        }
        catch {
            if (-not (Test-Path -LiteralPath $destinationPath) -and
                (Test-Path -LiteralPath $backupPath -PathType Container)) {
                Move-Item -LiteralPath $backupPath -Destination $destinationPath
            }
            throw
        }

        if (Test-Path -LiteralPath $backupPath -PathType Container) {
            try {
                Remove-GeneratedDirectory $backupPath
            }
            catch {
                Write-Warning "The old $Destination assets remain at $backupPath and can be removed after closing processes that use them."
            }
        }
    }
    finally {
        Remove-Item -LiteralPath $temporaryZip -Force -ErrorAction SilentlyContinue
        if (Test-Path -LiteralPath $stagingPath -PathType Container) {
            Remove-GeneratedDirectory $stagingPath
        }
    }
}

Install-LlamaArchive `
    -ArchiveName "llama-$llamaVersion-bin-win-avx-x64.zip" `
    -ArchiveSha256 "AF34E4DFC7154DF650ED705421F26B3A56C685F2E35ABFDD6DF0CC0CB5C4B259" `
    -Destination "llama_cpu" `
    -ExpectedFiles @("llama.lib", "llama.dll", "ggml.dll", "ggml-base.dll", "ggml-cpu.dll", "ggml-rpc.dll")
Install-LlamaArchive `
    -ArchiveName "llama-$llamaVersion-bin-win-cuda-cu12.4-x64.zip" `
    -ArchiveSha256 "5307B89AAE1F076E031D6AAB95E63DF5CB96B8D3B1FA7828F412E283A0663019" `
    -Destination "llama_cuda" `
    -ExpectedFiles @("llama.dll", "ggml.dll", "ggml-base.dll", "ggml-cpu.dll", "ggml-cuda.dll", "ggml-rpc.dll")
Install-LlamaArchive `
    -ArchiveName "llama-$llamaVersion-bin-win-vulkan-x64.zip" `
    -ArchiveSha256 "ED1B7E6AA70CDB2A93EAAAD8586539DCE83F4FA8A44F4DDEE5A1AB09FFB1189B" `
    -Destination "llama_vulkan" `
    -ExpectedFiles @("llama.dll", "ggml.dll", "ggml-base.dll", "ggml-cpu.dll", "ggml-vulkan.dll", "ggml-rpc.dll")

Copy-Item -LiteralPath (Join-Path $repoRoot "llama_cpu\llama.lib") `
    -Destination (Join-Path $repoRoot "server-swift\llama.lib") -Force

$zenzPath = Join-Path $repoRoot "zenz.gguf"
if (-not $Force -and (Test-FileHash -Path $zenzPath -ExpectedSha256 $zenzSha256)) {
    Write-Host "Using verified zenz.gguf."
}
else {
    $temporaryModel = Join-Path ([System.IO.Path]::GetTempPath()) ("azookey-zenz-" + [guid]::NewGuid().ToString("N") + ".gguf")
    Write-Host "Downloading Zenz model..."
    try {
        Invoke-WebRequest -UseBasicParsing -Uri $zenzUrl -OutFile $temporaryModel
        if (-not (Test-FileHash -Path $temporaryModel -ExpectedSha256 $zenzSha256)) {
            throw "SHA-256 verification failed for the Zenz model."
        }
        Move-Item -LiteralPath $temporaryModel -Destination $zenzPath -Force
    }
    finally {
        Remove-Item -LiteralPath $temporaryModel -Force -ErrorAction SilentlyContinue
    }
}

Write-Host "Build assets are ready."
