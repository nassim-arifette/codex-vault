param([string] $Cases, [switch] $DebugBuild)
$ErrorActionPreference = 'Stop'
Push-Location $PSScriptRoot
$previousCases = [Environment]::GetEnvironmentVariable('CODEX_VAULT_DIFF_CASES')
$previousReport = [Environment]::GetEnvironmentVariable('CODEX_VAULT_DIFF_REPORT')
try {
    if ($Cases) {
        $env:CODEX_VAULT_DIFF_CASES = (Resolve-Path -LiteralPath $Cases).Path
    }
    New-Item -ItemType Directory -Force -Path validation | Out-Null
    $log = Join-Path $PSScriptRoot ('validation/differential-' + (Get-Date -Format 'yyyyMMdd-HHmmss') + '.log')
    $env:CODEX_VAULT_DIFF_REPORT = [IO.Path]::ChangeExtension($log, '.jsonl')
    if (Test-Path -LiteralPath $env:CODEX_VAULT_DIFF_REPORT) { throw 'Report already exists; choose a new run timestamp.' }
    Write-Host 'Comparing two resumed turns before and after compaction on isolated copies.'
    Write-Host "Log: $log"
    Write-Host "Anonymous case results: $env:CODEX_VAULT_DIFF_REPORT"
    $buildArgs = @('test', '--locked')
    if (-not $DebugBuild) { $buildArgs += '--release' }
    $buildArgs += @('--test', 'differential', '--', '--ignored', '--nocapture', '--test-threads=1')
    $ErrorActionPreference = 'Continue'
    & cargo @buildArgs 2>&1 | Tee-Object -FilePath $log
    $result = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($result -ne 0) { throw "Differential validation failed (exit $result). See $log" }
    Write-Host 'Differential validation passed.'
}
finally {
    if ($null -eq $previousCases) { Remove-Item Env:CODEX_VAULT_DIFF_CASES -ErrorAction SilentlyContinue }
    else { $env:CODEX_VAULT_DIFF_CASES = $previousCases }
    if ($null -eq $previousReport) { Remove-Item Env:CODEX_VAULT_DIFF_REPORT -ErrorAction SilentlyContinue }
    else { $env:CODEX_VAULT_DIFF_REPORT = $previousReport }
    Pop-Location
}
