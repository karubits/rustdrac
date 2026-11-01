<#
.SYNOPSIS
  Render the winget manifests for a release and submit them to microsoft/winget-pkgs.

  scripts/winget-submit.ps1 -Version 0.4.0 -InstallerSha256 <sha256> -Token <github PAT>
  scripts/winget-submit.ps1 -Version 0.4.0 -InstallerSha256 <sha256> -Submit   # log in interactively
  scripts/winget-submit.ps1 -Version 0.4.0 -InstallerSha256 <sha256>           # render only

  The token is a classic GitHub PAT with the public_repo scope (CI keeps it in the
  WINGET_TOKEN secret); without one, -Submit lets wingetcreate sign you in to GitHub in the
  browser. wingetcreate pushes a branch to your fork of microsoft/winget-pkgs (created on
  first use) and opens the pull request. The release assets must already be public: winget
  validation downloads them.
#>
param(
  [Parameter(Mandatory)][string]$Version,
  [Parameter(Mandatory)][string]$InstallerSha256,
  [string]$Token = "",
  [switch]$Submit,
  [string]$OutDir = "build/winget"
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$Version = $Version -replace '^v', ''
$date = (Get-Date).ToUniversalTime().ToString("yyyy-MM-dd")

$dir = if ([IO.Path]::IsPathRooted($OutDir)) { $OutDir } else { Join-Path $root $OutDir }
New-Item -ItemType Directory -Force -Path $dir | Out-Null
Get-ChildItem (Join-Path $root "packaging/winget/*.yaml") | ForEach-Object {
  $text = (Get-Content $_.FullName -Raw) -replace '__VERSION__', $Version `
    -replace '__SHA256__', $InstallerSha256.ToUpper() -replace '__DATE__', $date
  Set-Content -Path (Join-Path $dir $_.Name) -Value $text -NoNewline -Encoding utf8
}
Write-Host "Rendered manifests for karubits.RustDrac $Version in $dir"

# wingetcreate, pinned by version and checksum (like scripts/install-tools.sh).
$wcVersion = "1.12.13.0"
$wcSha256 = "24042bd37915805615e6cf969ac57c6439124c3fe85823327f5f3fb24bd9ffea"
$wc = Join-Path $root "bin/wingetcreate.exe"
if (-not (Test-Path $wc) -or (Get-FileHash $wc -Algorithm SHA256).Hash -ne $wcSha256.ToUpper()) {
  New-Item -ItemType Directory -Force -Path (Split-Path $wc) | Out-Null
  Invoke-WebRequest -Uri "https://github.com/microsoft/winget-create/releases/download/v$wcVersion/wingetcreate.exe" -OutFile $wc
  $got = (Get-FileHash $wc -Algorithm SHA256).Hash
  if ($got -ne $wcSha256.ToUpper()) { throw "wingetcreate checksum mismatch: $got" }
}

if ($Token -eq "" -and -not $Submit) {
  Write-Host "Not submitting (no -Token or -Submit). Validate with: winget validate --manifest $dir"
  exit 0
}
$auth = if ($Token -ne "") { @("--token", $Token) } else { @() }
& $wc submit @auth --prtitle "New version: karubits.RustDrac version $Version" $dir
if ($LASTEXITCODE -ne 0) { throw "wingetcreate submit failed with exit code $LASTEXITCODE" }
