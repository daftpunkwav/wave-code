# wavecode installer for Windows: fetches the latest release binary
# for x86_64-pc-windows-msvc from GitHub Releases, verifies its SHA-256
# against the published checksum file, and installs it into
# $env:USERPROFILE\.cargo\bin (or $env:WAVECODE_INSTALL_DIR). Releases
# carry bare binaries, so no archive tooling is involved.
#
# Usage:
#   irm https://raw.githubusercontent.com/daftpunkwav/wave-code/main/scripts/install.ps1 | iex
# (pipe with care: review the script first, or download and run it.)

# Status messages target the console host on purpose: the script is
# documented for `irm | iex`, where Write-Output would inject strings
# into the pipeline iex executes.
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '',
    Justification = 'Console status output for a pipe-executed script; Write-Output would pollute the iex pipeline.')]
param()

$ErrorActionPreference = "Stop"

$repo = "daftpunkwav/wave-code"
$bin = "wavecode.exe"
$dest = if ($env:WAVECODE_INSTALL_DIR) { $env:WAVECODE_INSTALL_DIR } else { Join-Path $env:USERPROFILE ".cargo\bin" }
New-Item -ItemType Directory -Force -Path $dest | Out-Null

$target = "x86_64-pc-windows-msvc"

# Resolve the latest tag once so both downloads come from the same
# release even if one lands mid-publish.
$release = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/latest" `
    -Headers @{ "User-Agent" = "wavecode-install" }
$tag = $release.tag_name
if (-not $tag) {
    throw "could not resolve the latest release tag"
}

$base = "https://github.com/$repo/releases/download/$tag"
$asset = "wavecode-$($tag.TrimStart('v'))-$target-bin"
$tmp = New-Item -ItemType Directory -Force -Path (Join-Path $env:TEMP "wavecode-install-$PID")

try {
    Write-Host "downloading $tag ($target)..."
    Invoke-WebRequest -Uri "$base/$asset" -OutFile "$tmp\$bin" -UserAgent "wavecode-install"
    Invoke-WebRequest -Uri "$base/$asset.sha256" -OutFile "$tmp\$bin.sha256" -UserAgent "wavecode-install"

    # The checksum file is "<hex>  <filename>"; compare the downloaded
    # bytes against the published hex only.
    $expected = (Get-Content "$tmp\$bin.sha256").Split(" ")[0]
    $actual = (Get-FileHash -Algorithm SHA256 "$tmp\$bin").Hash.ToLower()
    if ($expected -ne $actual) {
        throw "checksum mismatch: expected $expected, got $actual"
    }

    Move-Item -Force "$tmp\$bin" (Join-Path $dest $bin)
    Write-Host "installed $tag to $(Join-Path $dest $bin)"
    if (($env:PATH -split ";") -notcontains $dest) {
        Write-Host "note: $dest is not on your PATH"
    }
    & (Join-Path $dest $bin) --version
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
