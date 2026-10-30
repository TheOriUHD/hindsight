# Builds the signed installer and puts it on the update server (Cloudflare R2).
#
#   scripts\release.ps1                         build only
#   scripts\release.ps1 -Publish test           publish to the test channel
#   scripts\release.ps1 -Publish stable         offer it to everyone
#   -Notes "..."                                shown in the app with the update
#   -NoBuild                                    publish what's already built
#
# The test channel is only seen by a build started with
#   HINDSIGHT_UPDATE_URL=https://api.hindsighthq.app/update/latest.json?channel=test
#
# Needs the signing key at %USERPROFILE%\.hindsight\updater.key. It stays out of
# the repo. Without it (or with another one) installed copies reject the update.
param(
    [ValidateSet('', 'test', 'stable')][string]$Publish = '',
    [string]$Notes = '',
    [switch]$NoBuild
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$key = Join-Path $env:USERPROFILE '.hindsight\updater.key'

$version = (Get-Content (Join-Path $root 'app\tauri.conf.json') -Raw | ConvertFrom-Json).version
$bundle = Join-Path $root 'target\release\bundle\nsis'
$name = "Hindsight_${version}_x64-setup.exe"

if (-not $NoBuild) {
    if (-not (Test-Path $key)) { throw "Signing key not found at $key" }
    $env:TAURI_SIGNING_PRIVATE_KEY = (Get-Content $key -Raw).Trim()
    $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ''
    Push-Location $root
    try {
        npx tauri build
        if ($LASTEXITCODE -ne 0) { throw 'build failed' }
    } finally {
        Pop-Location
    }
}

$exe = Join-Path $bundle $name
$sig = "$exe.sig"
if (-not (Test-Path $sig)) { throw "$sig is missing; the build didn't sign the installer" }
Write-Host "Built $exe"
if (-not $Publish) { return }

$prefix = if ($Publish -eq 'test') { 'test/' } else { '' }
$query = if ($Publish -eq 'test') { '?channel=test' } else { '' }
$manifest = [ordered]@{
    version   = $version
    notes     = $Notes
    pub_date  = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    platforms = @{
        'windows-x86_64' = @{
            signature = (Get-Content $sig -Raw).Trim()
            url       = "https://api.hindsighthq.app/update/$name$query"
        }
    }
}
$json = Join-Path $env:TEMP 'hindsight-latest.json'
[IO.File]::WriteAllText($json, ($manifest | ConvertTo-Json -Depth 5), (New-Object Text.UTF8Encoding $false))

# The manifest goes last, so nobody is offered a file that isn't there yet.
Push-Location (Join-Path $root 'cloud')
try {
    npx wrangler r2 object put "hindsight-updates/$prefix$name" --file $exe --content-type application/octet-stream --remote
    if ($LASTEXITCODE -ne 0) { throw 'upload failed' }
    npx wrangler r2 object put "hindsight-updates/${prefix}latest.json" --file $json --content-type application/json --remote
    if ($LASTEXITCODE -ne 0) { throw 'upload failed' }
} finally {
    Pop-Location
}
Write-Host "Published $version to $Publish"
