function Get-AzooKeyFileSha256 {
    param([Parameter(Mandatory = $true)][string]$Path)

    $sha256 = [Security.Cryptography.SHA256]::Create()
    try {
        $stream = [IO.File]::Open(
            $Path,
            [IO.FileMode]::Open,
            [IO.FileAccess]::Read,
            [IO.FileShare]::Read
        )
        try {
            return [BitConverter]::ToString($sha256.ComputeHash($stream)).Replace("-", "")
        }
        finally {
            $stream.Dispose()
        }
    }
    finally {
        $sha256.Dispose()
    }
}

function Expand-AzooKeyZipArchive {
    param(
        [Parameter(Mandatory = $true)][string]$ArchivePath,
        [Parameter(Mandatory = $true)][string]$DestinationPath
    )

    [void][Reflection.Assembly]::Load("System.IO.Compression")
    [void][Reflection.Assembly]::Load("System.IO.Compression.FileSystem")

    $destination = [IO.Path]::GetFullPath($DestinationPath).TrimEnd('\')
    $destinationPrefix = $destination + '\'
    [IO.Directory]::CreateDirectory($destination) | Out-Null

    $archive = [IO.Compression.ZipFile]::OpenRead($ArchivePath)
    try {
        foreach ($entry in $archive.Entries) {
            $relativePath = $entry.FullName.Replace('/', '\')
            $targetPath = [IO.Path]::GetFullPath((Join-Path $destination $relativePath))
            if (-not $targetPath.StartsWith(
                $destinationPrefix,
                [StringComparison]::OrdinalIgnoreCase
            )) {
                throw "Archive entry escapes its destination: $($entry.FullName)"
            }

            if ([string]::IsNullOrEmpty($entry.Name)) {
                [IO.Directory]::CreateDirectory($targetPath) | Out-Null
                continue
            }

            $parent = [IO.Directory]::GetParent($targetPath)
            if ($null -eq $parent) {
                throw "Archive entry has no parent directory: $($entry.FullName)"
            }
            [IO.Directory]::CreateDirectory($parent.FullName) | Out-Null
            $input = $entry.Open()
            try {
                $output = [IO.File]::Open(
                    $targetPath,
                    [IO.FileMode]::CreateNew,
                    [IO.FileAccess]::Write,
                    [IO.FileShare]::None
                )
                try {
                    $input.CopyTo($output)
                }
                finally {
                    $output.Dispose()
                }
            }
            finally {
                $input.Dispose()
            }
        }
    }
    finally {
        $archive.Dispose()
    }
}
