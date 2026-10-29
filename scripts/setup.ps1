# Fetches build dependencies into vendor/ (git-ignored):
#  - FFmpeg 8.1 LGPL shared dev build (headers, import libs, DLLs) from BtbN
#  - libclang.dll for bindgen (from the official libclang wheel), so no system LLVM is needed
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$vendor = Join-Path $root 'vendor'
New-Item -ItemType Directory -Force $vendor | Out-Null

if (-not (Test-Path "$vendor\ffmpeg\lib\avcodec.lib")) {
    $zip = "$vendor\ffmpeg.zip"
    Invoke-WebRequest 'https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n8.1-latest-win64-lgpl-shared-8.1.zip' -OutFile $zip
    $tmp = "$vendor\ffmpeg-extract"
    Expand-Archive $zip $tmp -Force
    Move-Item (Get-ChildItem $tmp)[0].FullName "$vendor\ffmpeg"
    Remove-Item $zip, $tmp -Recurse -Force
}

if (-not (Test-Path "$vendor\libclang\libclang.dll")) {
    $whl = "$vendor\libclang.zip"
    Invoke-WebRequest 'https://files.pythonhosted.org/packages/0b/2d/3f480b1e1d31eb3d6de5e3ef641954e5c67430d5ac93b7fa7e07589576c7/libclang-18.1.1-py2.py3-none-win_amd64.whl' -OutFile $whl
    $tmp = "$vendor\libclang-extract"
    Expand-Archive $whl $tmp -Force
    New-Item -ItemType Directory -Force "$vendor\libclang" | Out-Null
    Copy-Item (Get-ChildItem $tmp -Recurse -Filter libclang.dll)[0].FullName "$vendor\libclang\"
    Remove-Item $whl, $tmp -Recurse -Force
}
Write-Host 'vendor/ ready. Copy vendor\ffmpeg\bin\*.dll next to built binaries to run them.'
