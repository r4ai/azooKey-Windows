[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$TaskXmlTemplatePath,

    [Parameter(Mandatory = $true)]
    [string]$LaunchVbsPath,

    [Parameter(Mandatory = $true)]
    [string]$LauncherPath,

    [Parameter(Mandatory = $true)]
    [string]$TaskName
)

$ErrorActionPreference = "Stop"
$temporaryTaskXml = Join-Path $env:TEMP ("azookey-startup-{0}.xml" -f [Guid]::NewGuid().ToString("N"))

try {
    $launcherFullPath = [IO.Path]::GetFullPath($LauncherPath)
    if (-not (Test-Path -LiteralPath $launcherFullPath -PathType Leaf)) {
        throw "launcher.exe was not found at $launcherFullPath."
    }

    $launchVbsFullPath = [IO.Path]::GetFullPath($LaunchVbsPath)
    $escapedLauncherPath = $launcherFullPath.Replace('"', '""')
    $vbsContent = @(
        'Set objShell = CreateObject("WScript.Shell")'
        ('objShell.Run """{0}""", 0, False' -f $escapedLauncherPath)
    ) -join "`r`n"
    [IO.File]::WriteAllText(
        $launchVbsFullPath,
        $vbsContent + "`r`n",
        [Text.UnicodeEncoding]::new($false, $true)
    )

    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    if ($null -eq $identity.User) {
        throw "The current user's SID could not be determined."
    }

    $utf8 = [Text.UTF8Encoding]::new($false, $true)
    $taskXml = [IO.File]::ReadAllText($TaskXmlTemplatePath, $utf8)
    if (-not $taskXml.Contains("CURRENT_USER_SID") -or
        -not $taskXml.Contains("PATH_TO_VBS") -or
        -not $taskXml.Contains("PATH_TO_WSCRIPT")) {
        throw "The startup task template is missing a required placeholder."
    }

    $escapedLaunchPath = [Security.SecurityElement]::Escape('"' + $launchVbsFullPath + '"')
    $wscriptPath = Join-Path $env:WINDIR "System32\wscript.exe"
    if (-not (Test-Path -LiteralPath $wscriptPath -PathType Leaf)) {
        throw "wscript.exe was not found at $wscriptPath."
    }
    $escapedWscriptPath = [Security.SecurityElement]::Escape($wscriptPath)
    $taskXml = $taskXml.Replace("CURRENT_USER_SID", $identity.User.Value)
    $taskXml = $taskXml.Replace("PATH_TO_VBS", $escapedLaunchPath)
    $taskXml = $taskXml.Replace("PATH_TO_WSCRIPT", $escapedWscriptPath)
    [IO.File]::WriteAllText($temporaryTaskXml, $taskXml, [Text.UTF8Encoding]::new($false))

    $schtasks = Join-Path $env:WINDIR "System32\schtasks.exe"
    & $schtasks /Create /F /TN $TaskName /XML $temporaryTaskXml
    if ($LASTEXITCODE -ne 0) {
        throw "schtasks /Create failed with exit code $LASTEXITCODE."
    }

    & $schtasks /Run /TN $TaskName
    if ($LASTEXITCODE -ne 0) {
        throw "schtasks /Run failed with exit code $LASTEXITCODE."
    }
}
catch {
    Write-Error $_
    exit 1
}
finally {
    Remove-Item -LiteralPath $temporaryTaskXml -Force -ErrorAction SilentlyContinue
}

exit 0
