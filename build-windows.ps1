$ErrorActionPreference = "Stop"

# Cargo writes its progress to stderr. Under Windows PowerShell 5.1 with a redirected stream,
# `$ErrorActionPreference = "Stop"` turns that ordinary output into a terminating error, so each
# native command is run with the preference relaxed and its real exit code checked instead.
function Invoke-Cargo {
    param([Parameter(Mandatory = $true)][string[]] $Arguments)

    Write-Host "  cargo $($Arguments -join ' ')"
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        & cargo @Arguments
    }
    finally {
        $ErrorActionPreference = $previous
    }
    if ($LASTEXITCODE -ne 0) {
        throw "cargo $($Arguments -join ' ') failed with exit code $LASTEXITCODE"
    }
}

Push-Location $PSScriptRoot
try {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw "Rust/Cargo is not installed. Install the stable Rust toolchain from https://rustup.rs, reopen PowerShell, then rerun this script."
    }

    Write-Host "Running Codex Vault lints..."
    Invoke-Cargo @("clippy", "--all-targets", "--", "-D", "warnings")

    Write-Host "Running Codex Vault tests..."
    Invoke-Cargo @("test")

    Write-Host "Building release executable..."
    Invoke-Cargo @("build", "--release")

    New-Item -ItemType Directory -Force -Path dist | Out-Null
    Copy-Item target\release\codex-vault.exe dist\codex-vault.exe -Force
    Write-Host "Built: $PSScriptRoot\dist\codex-vault.exe"
}
finally {
    Pop-Location
}
