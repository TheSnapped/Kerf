# Builds dist\ and packages it as ..\releases\Kerf-<version>-windows-x64 (folder + zip).
Set-Location $PSScriptRoot
& .\build.ps1
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$name = "Kerf-$version-windows-x64"
$out = Join-Path (Split-Path $PSScriptRoot) "releases"
$dir = Join-Path $out $name
New-Item -ItemType Directory -Force $out | Out-Null
if (Test-Path $dir) { Remove-Item -Recurse -Force $dir }
New-Item -ItemType Directory $dir | Out-Null
Copy-Item dist\kerf.exe $dir
Copy-Item RELEASE-README.txt (Join-Path $dir "README.txt")
Copy-Item LICENSE (Join-Path $dir "LICENSE.txt")
# tar (built into Windows 10+) writes standard zips; PowerShell 5.1's Compress-Archive uses
# backslashes in entry names, which unzip tools on other systems mishandle.
$zip = Join-Path $out "$name.zip"
Remove-Item $zip -ErrorAction SilentlyContinue
tar.exe -a -c -f $zip -C $out $name
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Write-Host "Packaged releases\$name.zip"
