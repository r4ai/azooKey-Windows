[CmdletBinding()]
param(
    [string]$PackagePath = (Join-Path $PSScriptRoot "..\server-swift"),
    [string]$CheckoutPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$expectedRevision = "80b8204f1cdfb364bb2ed355cf52c7ebb2519a0c"
$expectedVersion = "0.11.2"
$patchedFile = "Sources/KanaKanjiConverterModule/ConversionAlgorithms/Zenzai/Zenz/ZenzContext.swift"
$expectedOriginalBlob = "d268f9a2a46281600425a1fce081c062678b2acb"
$expectedPatchedBlob = "34bc2c986f35f95db8f65e913595dd255a79581f"
$expectedPatchedStatus = " M $patchedFile"
$packageRoot = (Resolve-Path -LiteralPath $PackagePath).Path
$resolvedPath = Join-Path $packageRoot "Package.resolved"
$patchPath = Join-Path $PSScriptRoot "patches\AzooKeyKanaKanjiConverter-v0.11.2-inference.patch"

if (-not (Test-Path -LiteralPath $resolvedPath -PathType Leaf)) {
    throw "Package.resolved was not found at '$resolvedPath'. Run 'swift package resolve' first."
}
if (-not (Test-Path -LiteralPath $patchPath -PathType Leaf)) {
    throw "Inference patch was not found at '$patchPath'."
}

$resolved = Get-Content -LiteralPath $resolvedPath -Raw | ConvertFrom-Json
$converterPins = @($resolved.pins | Where-Object { $_.identity -eq "azookeykanakanjiconverter" })
if ($converterPins.Count -ne 1) {
    throw "Expected exactly one AzooKeyKanaKanjiConverter pin, found $($converterPins.Count)."
}
$converterPin = $converterPins[0]
if ($converterPin.state.revision -ne $expectedRevision -or $converterPin.state.version -ne $expectedVersion) {
    throw "AzooKeyKanaKanjiConverter must resolve to v$expectedVersion ($expectedRevision), but Package.resolved contains version '$($converterPin.state.version)' revision '$($converterPin.state.revision)'."
}

if ([string]::IsNullOrWhiteSpace($CheckoutPath)) {
    $checkoutsRoot = Join-Path $packageRoot ".build\checkouts"
    if (-not (Test-Path -LiteralPath $checkoutsRoot -PathType Container)) {
        throw "SwiftPM checkout directory was not found at '$checkoutsRoot'. Run 'swift package resolve' first."
    }
    $matches = @(Get-ChildItem -LiteralPath $checkoutsRoot -Directory | Where-Object {
        $_.Name -ieq "AzooKeyKanaKanjiConverter"
    })
    if ($matches.Count -ne 1) {
        throw "Expected exactly one AzooKeyKanaKanjiConverter checkout under '$checkoutsRoot', found $($matches.Count)."
    }
    $CheckoutPath = $matches[0].FullName
}

$checkoutRoot = (Resolve-Path -LiteralPath $CheckoutPath).Path
$gitSafeDirectory = "safe.directory=$($checkoutRoot -replace '\\', '/')"
$head = (& git -c $gitSafeDirectory -C $checkoutRoot rev-parse HEAD 2>&1 | Out-String).Trim()
if ($LASTEXITCODE -ne 0) {
    throw "Could not inspect the AzooKeyKanaKanjiConverter checkout: $head"
}
if ($head -ne $expectedRevision) {
    throw "Refusing to patch unexpected AzooKeyKanaKanjiConverter revision '$head'; expected '$expectedRevision'."
}

function Test-GitPatch([bool]$Reverse) {
    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        if ($Reverse) {
            & git -c $gitSafeDirectory -C $checkoutRoot apply --unidiff-zero --reverse --check --whitespace=error-all $patchPath 2>&1 | Out-Null
        } else {
            & git -c $gitSafeDirectory -C $checkoutRoot apply --unidiff-zero --check --whitespace=error-all $patchPath 2>&1 | Out-Null
        }
        return $LASTEXITCODE -eq 0
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
}

function Get-CheckoutState {
    $statusLines = @(& git -c $gitSafeDirectory -C $checkoutRoot status --porcelain=v1 --untracked-files=all)
    if ($LASTEXITCODE -ne 0) {
        throw "Could not inspect the AzooKeyKanaKanjiConverter working tree."
    }
    $blob = (& git -c $gitSafeDirectory -C $checkoutRoot hash-object -- $patchedFile | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "Could not hash '$patchedFile' in the AzooKeyKanaKanjiConverter checkout."
    }
    return [pscustomobject]@{
        Status = $statusLines -join "`n"
        Blob = $blob
    }
}

$state = Get-CheckoutState

if ($state.Blob -eq $expectedOriginalBlob -and [string]::IsNullOrEmpty($state.Status)) {
    if (-not (Test-GitPatch -Reverse $false)) {
        throw "The pinned AzooKeyKanaKanjiConverter source does not accept the expected inference patch."
    }
    & git -c $gitSafeDirectory -C $checkoutRoot apply --unidiff-zero --whitespace=error-all $patchPath
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to apply the verified AzooKeyKanaKanjiConverter inference patch."
    }

    $state = Get-CheckoutState
    if ($state.Blob -ne $expectedPatchedBlob -or $state.Status -ne $expectedPatchedStatus) {
        throw "The AzooKeyKanaKanjiConverter checkout did not reach the exact expected patched state."
    }
    Write-Output "Applied AzooKeyKanaKanjiConverter v$expectedVersion inference patch."
    return
}

if ($state.Blob -eq $expectedPatchedBlob -and
    $state.Status -eq $expectedPatchedStatus) {
    Write-Output "AzooKeyKanaKanjiConverter v$expectedVersion inference patch is already applied."
    return
}

throw "The AzooKeyKanaKanjiConverter checkout is not exactly the clean pinned v$expectedVersion source or the expected one-file patched state (blob '$($state.Blob)', status '$($state.Status)')."
