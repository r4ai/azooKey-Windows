[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$TaskXmlTemplatePath,

    [Parameter(Mandatory = $true)]
    [string]$LaunchVbsPath,

    [Parameter(Mandatory = $true)]
    [string]$LauncherPath,

    [Parameter(Mandatory = $true)]
    [string]$TaskName,

    [switch]$ValidateOnly
)

$ErrorActionPreference = "Stop"
$temporaryTaskXml = Join-Path $env:TEMP ("azookey-startup-{0}.xml" -f [Guid]::NewGuid().ToString("N"))

try {
    $launcherFullPath = [IO.Path]::GetFullPath($LauncherPath)
    if (-not (Test-Path -LiteralPath $launcherFullPath -PathType Leaf)) {
        throw "launcher.exe was not found at $launcherFullPath."
    }
    $launcherDirectory = [IO.Path]::GetDirectoryName($launcherFullPath)
    if ([string]::IsNullOrWhiteSpace($launcherDirectory) -or
        -not (Test-Path -LiteralPath $launcherDirectory -PathType Container)) {
        throw "The launcher directory was not found for $launcherFullPath."
    }

    $launchVbsFullPath = [IO.Path]::GetFullPath($LaunchVbsPath)
    $escapedLauncherPath = $launcherFullPath.Replace('"', '""')
    $escapedLauncherDirectory = $launcherDirectory.Replace('"', '""')
    $vbsContent = @(
        'Set objShell = CreateObject("WScript.Shell")'
        ('objShell.CurrentDirectory = "{0}"' -f $escapedLauncherDirectory)
        ('exitCode = objShell.Run("""{0}""", 0, True)' -f $escapedLauncherPath)
        'WScript.Quit exitCode'
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
        -not $taskXml.Contains("PATH_TO_WSCRIPT") -or
        -not $taskXml.Contains("PATH_TO_APP_DIR") -or
        -not $taskXml.Contains('encoding="UTF-8"')) {
        throw "The startup task template is missing a required placeholder or UTF-8 declaration."
    }

    $escapedLaunchPath = [Security.SecurityElement]::Escape('"' + $launchVbsFullPath + '"')
    $wscriptPath = Join-Path $env:WINDIR "System32\wscript.exe"
    if (-not (Test-Path -LiteralPath $wscriptPath -PathType Leaf)) {
        throw "wscript.exe was not found at $wscriptPath."
    }
    $escapedWscriptPath = [Security.SecurityElement]::Escape($wscriptPath)
    $escapedLauncherDirectoryXml = [Security.SecurityElement]::Escape($launcherDirectory)
    $taskXml = $taskXml.Replace("CURRENT_USER_SID", $identity.User.Value)
    $taskXml = $taskXml.Replace("PATH_TO_VBS", $escapedLaunchPath)
    $taskXml = $taskXml.Replace("PATH_TO_WSCRIPT", $escapedWscriptPath)
    $taskXml = $taskXml.Replace("PATH_TO_APP_DIR", $escapedLauncherDirectoryXml)

    $taskXml = $taskXml.Replace('encoding="UTF-8"', 'encoding="UTF-16"')
    [IO.File]::WriteAllText(
        $temporaryTaskXml,
        $taskXml,
        [Text.UnicodeEncoding]::new($false, $true)
    )

    if ($ValidateOnly) {
        $taskXmlBytes = [IO.File]::ReadAllBytes($temporaryTaskXml)
        if ($taskXmlBytes.Length -lt 2 -or
            $taskXmlBytes[0] -ne 0xFF -or
            $taskXmlBytes[1] -ne 0xFE) {
            throw "The generated startup task XML is not UTF-16 LE with a BOM."
        }

        $validatedTaskXml = [Xml.XmlDocument]::new()
        $validatedTaskXml.PreserveWhitespace = $true
        $validatedTaskXml.Load($temporaryTaskXml)

        $scheduler = New-Object -ComObject "Schedule.Service"
        $scheduler.Connect()
        $definition = $scheduler.NewTask(0)
        $definition.XmlText = $validatedTaskXml.OuterXml
    }
    else {
        $schtasks = Join-Path $env:WINDIR "System32\schtasks.exe"
        & $schtasks /Create /F /TN $TaskName /XML $temporaryTaskXml
        $createExitCode = $LASTEXITCODE
        if ($createExitCode -ne 0) {
            throw "schtasks /Create failed with exit code $createExitCode."
        }

        & $schtasks /Run /TN $TaskName
        $runExitCode = $LASTEXITCODE
        if ($runExitCode -ne 0) {
            throw "schtasks /Run failed with exit code $runExitCode."
        }
    }
}
catch {
    $errorText = ($_ | Out-String).Trim()
    [Console]::Error.WriteLine($errorText)
    exit 1
}
finally {
    Remove-Item -LiteralPath $temporaryTaskXml -Force -ErrorAction SilentlyContinue
}

exit 0
