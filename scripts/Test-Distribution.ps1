param([Parameter(Mandatory=$true)][string] $Archive)
$ErrorActionPreference = 'Stop'
$testTempBase = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')
$testRoot = Join-Path $testTempBase ('codex-vault-install-test-' + [guid]::NewGuid().ToString('N'))
$oldCodex = $env:CODEX_HOME
$oldVault = $env:CODEX_VAULT_HOME
# A hosted Windows runner may already have the redistributable installed. Check the
# actual PE imports as well as executing the package so that cannot hide a dependency.
function Assert-PortableImports([string] $Executable) {
    $bytes = [IO.File]::ReadAllBytes($Executable)
    $pe = [BitConverter]::ToInt32($bytes, 0x3c)
    $sectionsCount = [BitConverter]::ToUInt16($bytes, $pe + 6)
    $optionalSize = [BitConverter]::ToUInt16($bytes, $pe + 20)
    $optional = $pe + 24
    if ([BitConverter]::ToUInt16($bytes, $optional) -ne 0x20b) { throw 'Expected a 64-bit PE executable' }
    $importRva = [BitConverter]::ToUInt32($bytes, $optional + 120)
    $sections = @()
    for ($i = 0; $i -lt $sectionsCount; $i++) {
        $section = $optional + $optionalSize + 40 * $i
        $sections += @{
            rva = [BitConverter]::ToUInt32($bytes, $section + 12)
            size = [Math]::Max([BitConverter]::ToUInt32($bytes, $section + 8), [BitConverter]::ToUInt32($bytes, $section + 16))
            raw = [BitConverter]::ToUInt32($bytes, $section + 20)
        }
    }
    function Convert-Rva([uint32] $Rva) {
        foreach ($s in $sections) {
            if ($Rva -ge $s.rva -and $Rva -lt $s.rva + $s.size) { return [int]($s.raw + $Rva - $s.rva) }
        }
        throw 'Invalid PE import address'
    }
    $descriptor = Convert-Rva $importRva
    while (($nameRva = [BitConverter]::ToUInt32($bytes, $descriptor + 12)) -ne 0) {
        $start = Convert-Rva $nameRva
        $end = $start
        while ($bytes[$end] -ne 0) { $end++ }
        $name = [Text.Encoding]::ASCII.GetString($bytes, $start, $end - $start)
        if ($name -match '^(vcruntime|msvcp[0-9]|concrt|sqlite3|zstd)') { throw "Unexpected external runtime dependency: $name" }
        $descriptor += 20
    }
}
try {
    $unpacked = Join-Path $testRoot 'unpacked'
    Expand-Archive -LiteralPath (Resolve-Path -LiteralPath $Archive).Path -DestinationPath $unpacked
    $installed = Join-Path $testRoot 'installed'
    & (Join-Path $unpacked 'install.ps1') -InstallDirectory $installed -NoPath
    $exe = Join-Path $installed 'codex-vault.exe'
    Assert-PortableImports $exe
    & $exe --help | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Installed executable help failed' }
    $env:CODEX_HOME = Join-Path $testRoot 'codex'
    $env:CODEX_VAULT_HOME = Join-Path $testRoot 'vault'
    $sessions = Join-Path $env:CODEX_HOME 'sessions'
    New-Item -ItemType Directory -Force -Path $sessions | Out-Null
    $fixture = @(
        @{type='session_meta';payload=@{id='distribution-test';cwd=$installed;cli_version='0.152.1'}},
        @{type='event_msg';payload=@{type='user_message';message='Synthetic distribution authentication test'}}
    ) | ForEach-Object { ConvertTo-Json -InputObject $_ -Depth 8 -Compress }
    [IO.File]::WriteAllText((Join-Path $sessions 'rollout-distribution.jsonl'), ($fixture -join "`n") + "`n", [Text.UTF8Encoding]::new($false))
    & $exe --json index | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Bundled SQLite initialization failed' }
    $found = & $exe --json search authentication | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0 -or @($found.matches).Count -ne 1) { throw 'Installed full-text search failed' }
    & $exe --json read $found.matches[0].id | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Installed passage read failed' }
    Write-Host 'Clean installation smoke test passed: help, bundled SQLite, search and verified read.'
}
finally {
    if ($null -eq $oldCodex) { Remove-Item Env:CODEX_HOME -ErrorAction SilentlyContinue } else { $env:CODEX_HOME=$oldCodex }
    if ($null -eq $oldVault) { Remove-Item Env:CODEX_VAULT_HOME -ErrorAction SilentlyContinue } else { $env:CODEX_VAULT_HOME=$oldVault }
    $resolvedTestRoot = [IO.Path]::GetFullPath($testRoot)
    if ([IO.Path]::GetDirectoryName($resolvedTestRoot) -ne $testTempBase -or [IO.Path]::GetFileName($resolvedTestRoot) -notmatch '^codex-vault-install-test-[0-9a-f]{32}$') { throw 'Unsafe test cleanup path' }
    if (Test-Path -LiteralPath $resolvedTestRoot) { Remove-Item -LiteralPath $resolvedTestRoot -Recurse -Force }
}
