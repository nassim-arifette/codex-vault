param(
    [string] $InstallDirectory = (Join-Path $env:LOCALAPPDATA 'Programs/CodexVault'),
    [switch] $NoPath
)
$ErrorActionPreference = 'Stop'
$binary = Join-Path $PSScriptRoot 'codex-vault.exe'
$checksums = Join-Path $PSScriptRoot 'SHA256SUMS.txt'
if (-not (Test-Path -LiteralPath $binary) -or -not (Test-Path -LiteralPath $checksums)) { throw 'Extract the complete release ZIP before running install.ps1.' }
$expected = @(Get-Content -LiteralPath $checksums | Where-Object { $_ -match '^[0-9a-fA-F]{64} \*codex-vault\.exe$' })
if ($expected.Count -ne 1) { throw 'Missing executable checksum' }
if ((Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash -ne $expected[0].Substring(0,64)) { throw 'Executable checksum mismatch' }
$destination = [IO.Path]::GetFullPath($InstallDirectory)
New-Item -ItemType Directory -Force -Path $destination | Out-Null
Copy-Item -LiteralPath $binary -Destination (Join-Path $destination 'codex-vault.exe') -Force
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'LICENSE') -Destination (Join-Path $destination 'LICENSE') -Force
if (-not $NoPath) {
    $existingPath = [Environment]::GetEnvironmentVariable('Path','User')
    $entries = @($existingPath -split ';' | Where-Object { $_ })
    if (-not ($entries | Where-Object { $_.TrimEnd('\') -ieq $destination.TrimEnd('\') })) {
        [Environment]::SetEnvironmentVariable('Path', (($entries + $destination) -join ';'), 'User')
    }
    $env:Path = $destination + ';' + $env:Path
}
Write-Host "Installed in $destination. Open a new terminal and run codex-vault --help."
