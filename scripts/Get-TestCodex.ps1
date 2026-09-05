param(
    [Parameter(Mandatory=$true)][ValidatePattern('^\d+\.\d+\.\d+$')][string] $Version,
    [string] $OutputDirectory = (Join-Path $PSScriptRoot '../validation/codex')
)
$ErrorActionPreference = 'Stop'
$destination = Join-Path ([IO.Path]::GetFullPath($OutputDirectory)) $Version
New-Item -ItemType Directory -Force -Path $destination | Out-Null
$assetName = 'codex-x86_64-pc-windows-msvc.exe.zip'
$release = gh api "repos/openai/codex/releases/tags/rust-v$Version" | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'Cannot read the official Codex release' }
$asset = @($release.assets | Where-Object name -eq $assetName)
if ($asset.Count -ne 1 -or $asset[0].digest -notmatch '^sha256:[0-9a-fA-F]{64}$') { throw 'Missing official SHA-256 digest' }
$archive = Join-Path $destination $assetName
if (-not (Test-Path -LiteralPath $archive)) {
    gh release download "rust-v$Version" --repo openai/codex --pattern $assetName --dir $destination
    if ($LASTEXITCODE -ne 0) { throw 'Codex download failed' }
}
if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ne $asset[0].digest.Substring(7)) { throw 'Official Codex checksum mismatch' }
Expand-Archive -LiteralPath $archive -DestinationPath $destination -Force
$executables = @(Get-ChildItem -LiteralPath $destination -File -Recurse | Where-Object { $_.Name -match '^codex(?:-x86_64-pc-windows-msvc)?\.exe$' })
if ($executables.Count -ne 1) { throw 'Expected exactly one Codex executable' }
$reportedVersion = & $executables[0].FullName --version
if ($LASTEXITCODE -ne 0 -or $reportedVersion -notmatch ('\b' + [regex]::Escape($Version) + '$')) { throw 'Unexpected Codex version' }
Write-Output $executables[0].FullName
