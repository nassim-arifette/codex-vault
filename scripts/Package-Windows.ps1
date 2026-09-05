param([switch] $SkipBuild)
$ErrorActionPreference = 'Stop'
$workspace = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
Push-Location $workspace
try {
    if (-not $SkipBuild) { & (Join-Path $workspace 'build-windows.ps1') }
    $metadata = cargo metadata --offline --no-deps --format-version 1 | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Cannot read package version' }
    $version = @($metadata.packages | Where-Object name -eq 'codex-vault')[0].version
    $releaseRoot = Join-Path $workspace 'dist/release'
    New-Item -ItemType Directory -Force -Path $releaseRoot | Out-Null
    $stage = Join-Path $releaseRoot ('stage-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $stage | Out-Null
    try {
        Copy-Item -LiteralPath (Join-Path $workspace 'target/release/codex-vault.exe') -Destination (Join-Path $stage 'codex-vault.exe')
        Copy-Item -LiteralPath (Join-Path $workspace 'LICENSE') -Destination $stage
        Copy-Item -LiteralPath (Join-Path $workspace 'README.md') -Destination $stage
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'install.ps1') -Destination $stage
        $executableHash = (Get-FileHash -LiteralPath (Join-Path $stage 'codex-vault.exe') -Algorithm SHA256).Hash.ToLowerInvariant()
        [IO.File]::WriteAllText((Join-Path $stage 'SHA256SUMS.txt'), "$executableHash *codex-vault.exe`n", [Text.UTF8Encoding]::new($false))
        $zipName = "codex-vault-$version-windows-x86_64.zip"
        $zipPath = Join-Path $releaseRoot $zipName
        Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zipPath -Force
        $zipHash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
        [IO.File]::WriteAllText((Join-Path $releaseRoot 'SHA256SUMS.txt'), "$zipHash *$zipName`n", [Text.UTF8Encoding]::new($false))
        Write-Output $zipPath
    }
    finally {
        $resolvedStage = [IO.Path]::GetFullPath($stage)
        if ([IO.Path]::GetDirectoryName($resolvedStage) -ne $releaseRoot -or [IO.Path]::GetFileName($resolvedStage) -notmatch '^stage-[0-9a-f]{32}$') { throw 'Unsafe packaging cleanup path' }
        Remove-Item -LiteralPath $resolvedStage -Recurse -Force
    }
}
finally { Pop-Location }
