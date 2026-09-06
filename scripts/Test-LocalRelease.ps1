<#
.SYNOPSIS
Validate a public Windows ZIP on this machine using one private real rollout.
.DESCRIPTION
Downloads the release with gh, verifies checksums and portable imports, installs into
LOCALAPPDATA/Programs/CodexVault and updates the persistent User PATH. Starts a fresh
PowerShell process using the machine/user PATH. Scans real sessions read-only, then
archives, compacts, restores and indexes only an isolated copy. Raw logs stay private.
.EXAMPLE
./scripts/Test-LocalRelease.ps1 -RealRollout 'C:/private/rollout.jsonl' -Version 0.2.1
#>
param(
    [Parameter(Mandatory=$true)][string] $RealRollout,
    [string] $Version = '0.2.1',
    [string] $OutputDirectory = (Join-Path $PSScriptRoot ('../validation/local-release-' + (Get-Date -Format 'yyyyMMdd-HHmmss'))),
    [switch] $InstalledCheck
)
$ErrorActionPreference = 'Stop'
if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw 'Use a numeric release version.' }
$runRoot = [IO.Path]::GetFullPath($OutputDirectory)
$source = (Resolve-Path -LiteralPath $RealRollout).Path
$installed = Join-Path $env:LOCALAPPDATA 'Programs/CodexVault/codex-vault.exe'

if (-not $InstalledCheck) {
    if (Test-Path -LiteralPath $runRoot) { throw 'Choose a new output directory.' }
    New-Item -ItemType Directory -Path $runRoot | Out-Null
    $asset = "codex-vault-$Version-windows-x86_64.zip"
    & gh release download "v$Version" --repo nassim-arifette/codex-vault --pattern $asset --pattern SHA256SUMS.txt --dir $runRoot
    if ($LASTEXITCODE -ne 0) { throw 'Release download failed.' }
    $zip = Join-Path $runRoot $asset
    $expected = @(Get-Content -LiteralPath (Join-Path $runRoot 'SHA256SUMS.txt') | Where-Object { $_.EndsWith(" *$asset") })
    if ($expected.Count -ne 1 -or (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash -ne $expected[0].Substring(0,64)) { throw 'ZIP checksum mismatch.' }
    $unpacked = Join-Path $runRoot 'unpacked'
    Expand-Archive -LiteralPath $zip -DestinationPath $unpacked
    & (Join-Path $PSScriptRoot 'Test-Distribution.ps1') -Archive $zip *> (Join-Path $runRoot 'portable-check.log')
    # This is deliberately the real per-user install, including persistent User PATH.
    & (Join-Path $unpacked 'install.ps1') | Out-File -LiteralPath (Join-Path $runRoot 'install.log')
    $oldPath = $env:Path
    try {
        # A new shell gets only the persistent machine/user PATH, not this task's dev PATH.
        $env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
        & "$env:SystemRoot/System32/WindowsPowerShell/v1.0/powershell.exe" -NoProfile -ExecutionPolicy Bypass -File $PSCommandPath -InstalledCheck -RealRollout $source -Version $Version -OutputDirectory $runRoot
        if ($LASTEXITCODE -ne 0) { throw 'Fresh PowerShell release validation failed.' }
    }
    finally { $env:Path = $oldPath }
    Write-Host "Public v$Version installed and verified in a fresh PowerShell process. Reports remain in validation/."
    return
}

$resolved = (Get-Command codex-vault -CommandType Application).Source
if ([IO.Path]::GetFullPath($resolved) -ine [IO.Path]::GetFullPath($installed)) { throw 'PATH resolved a different executable.' }
if ((& codex-vault --version) -ne "codex-vault $Version") { throw 'Installed version mismatch.' }
& codex-vault --help | Out-File -LiteralPath (Join-Path $runRoot 'help.txt')
if ($LASTEXITCODE -ne 0) { throw 'Installed executable failed to start.' }
$realScan = & codex-vault --json scan | ConvertFrom-Json
if ($LASTEXITCODE -ne 0 -or @($realScan.sessions).Count -eq 0) { throw 'Read-only scan found no real sessions.' }
$sourceHash = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash
$sourceSize = (Get-Item -LiteralPath $source).Length
$oldCodex = $env:CODEX_HOME
$oldVault = $env:CODEX_VAULT_HOME
try {
    $env:CODEX_HOME = Join-Path $runRoot 'copy/codex'
    $env:CODEX_VAULT_HOME = Join-Path $runRoot 'copy/vault'
    $sessions = Join-Path $env:CODEX_HOME 'sessions'
    New-Item -ItemType Directory -Force -Path $sessions | Out-Null
    $copy = Join-Path $sessions ([IO.Path]::GetFileName($source))
    Copy-Item -LiteralPath $source -Destination $copy
    if ((Get-FileHash -LiteralPath $copy -Algorithm SHA256).Hash -ne $sourceHash) { throw 'Copy hash mismatch.' }
    function Invoke-Vault([string] $Name, [string[]] $Arguments) {
        $raw = & codex-vault --json --no-progress @Arguments 2> (Join-Path $runRoot "$Name.stderr")
        $code = $LASTEXITCODE
        $raw | Out-File -LiteralPath (Join-Path $runRoot "$Name.json") -Encoding utf8
        if ($code -ne 0) { throw "$Name failed (exit $code); inspect private logs." }
        return ($raw | ConvertFrom-Json)
    }
    $null = Invoke-Vault 'archive' @('archive', $copy)
    $null = Invoke-Vault 'verify-archive' @('doctor', $copy, '--deep')
    $compact = Invoke-Vault 'compact' @('compact', $copy)
    $nativeAfter = (Get-Item -LiteralPath $copy).Length
    if ($compact.status -ne 'ok' -or $nativeAfter -ge $sourceSize) { throw 'Choose a real rollout with a supported removable prefix.' }
    $null = Invoke-Vault 'verify-compact' @('doctor', $copy, '--deep')
    $index = Invoke-Vault 'index' @('index')
    # Pick a literal word from an indexable message on the untouched source. Keep it private.
    $query = $null
    foreach ($line in [IO.File]::ReadLines($source, [Text.Encoding]::UTF8)) {
        $record = $line | ConvertFrom-Json
        $texts = @()
        if ($record.type -eq 'event_msg' -and $record.payload.type -in @('user_message','agent_message')) { $texts += $record.payload.message }
        if ($record.type -eq 'response_item' -and $record.payload.type -eq 'message' -and $record.payload.role -in @('user','assistant')) {
            $texts += @($record.payload.content | Where-Object type -in @('input_text','output_text','text') | ForEach-Object text)
        }
        foreach ($text in $texts) {
            $word = [regex]::Match([string]$text, '[\p{L}\p{N}]{4,}')
            if ($word.Success) { $query = $word.Value; break }
        }
        if ($query) { break }
    }
    if (-not $query) { throw 'No searchable word found in an indexable message.' }
    $found = Invoke-Vault 'search' @('search', $query, '--limit', '1')
    if (@($found.matches).Count -ne 1) { throw 'No indexed message found.' }
    $passageId = $found.matches[0].id
    $read = Invoke-Vault 'read' @('read', $passageId)
    $null = Invoke-Vault 'restore' @('restore', $copy, '--original')
    if ((Get-FileHash -LiteralPath $copy -Algorithm SHA256).Hash -ne $sourceHash) { throw 'Restore hash mismatch.' }
    $null = Invoke-Vault 'verify-restored' @('doctor', $copy, '--deep')
    $null = Invoke-Vault 'reindex' @('index')
    $readAgain = Invoke-Vault 'read-restored' @('read', $passageId)
    if ($readAgain.text -cne $read.text) { throw 'Indexed passage changed after restore.' }
    if ((Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash -ne $sourceHash) { throw 'The original source changed during the test.' }
    $summary = [ordered]@{
        schema_version=1; version=$Version; passed=$true; zip_sha256=(Get-FileHash -LiteralPath (Join-Path $runRoot "codex-vault-$Version-windows-x86_64.zip") -Algorithm SHA256).Hash.ToLowerInvariant();
        fresh_powershell_user_path=$true; real_scan_found_sessions=$true;
        copied_input_bytes=$sourceSize; compacted_native_bytes=$nativeAfter;
        storage_after_compact=$compact.stats.storage.after; index_bytes=$index.index_bytes;
        restored_sha256_matches=$true; original_unchanged=$true; indexed_passage_verified=$true;
        authenticode_status=(Get-AuthenticodeSignature -LiteralPath $installed).Status.ToString();
        portable_import_check='Run Test-Distribution.ps1 against the same ZIP; see private portable-check.log.';
        smartscreen='Command-line install/start succeeded; interactive browser download/SmartScreen flow was not exercised.'
    }
    $summary | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $runRoot 'summary.json') -Encoding utf8
    Write-Host 'PASS: user PATH, help, real scan, copy/archive/compact/deep doctor/exact restore/index/search/read.'
}
finally {
    if ($null -eq $oldCodex) { Remove-Item Env:CODEX_HOME -ErrorAction SilentlyContinue } else { $env:CODEX_HOME=$oldCodex }
    if ($null -eq $oldVault) { Remove-Item Env:CODEX_VAULT_HOME -ErrorAction SilentlyContinue } else { $env:CODEX_VAULT_HOME=$oldVault }
}
