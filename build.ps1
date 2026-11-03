# Builds the release exe into dist\. Pass -Clean to also delete the ~0.5 GB
# cargo build cache afterwards (the next build then takes about a minute).
param([switch]$Clean)
# Errors are detected via $LASTEXITCODE: with "Stop", PowerShell 5.1 aborts on
# cargo's ordinary progress output to stderr whenever output is redirected.
Set-Location $PSScriptRoot

# Release exes embed source file paths (for panic messages). Map the cargo and project
# folders to neutral names so no personal path, such as the Windows user name, ends up
# in the exe. This replaces the rustflags in .cargo\config.toml, so crt-static is repeated.
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE ".cargo" }
$env:CARGO_ENCODED_RUSTFLAGS = @(
    "-Ctarget-feature=+crt-static",
    "--remap-path-prefix=$cargoHome=cargo",
    "--remap-path-prefix=$PSScriptRoot=src"
) -join [char]0x1f

cargo build --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
New-Item -ItemType Directory -Force dist | Out-Null
Copy-Item target\release\kerf.exe dist\kerf.exe -Force
Write-Host "Built dist\kerf.exe"

if ($Clean) {
    cargo clean
    Write-Host "Removed build cache (target\)"
}
