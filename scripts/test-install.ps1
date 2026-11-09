# Test scripts\install.ps1 against a local copy of a release, with a throwaway LOCALAPPDATA:
#
#   scripts\test-install.ps1 <bedrock-mcp.exe> [files to ship next to it...]
#
# Checks a fresh install, an update in place, a pinned tag, a missing release and a bad checksum.
# The user Path change the installer makes is undone at the end.
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
$Binary = (Resolve-Path $args[0]).Path
$Extra = @($args | Select-Object -Skip 1)
$Work = Join-Path ([IO.Path]::GetTempPath()) "bedrock-mcp-install-$PID"
$Asset = 'bedrock-mcp-windows-x86_64'
$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')

function Fail($Message) { throw "FAIL: $Message" }
function Install([string[]]$Arguments) {
  $script = Join-Path $Root 'scripts\install.ps1'
  $output = & powershell -NoProfile -ExecutionPolicy Bypass -File $script @Arguments 2>&1 | Out-String
  return @{ Ok = ($LASTEXITCODE -eq 0); Text = $output }
}

New-Item -ItemType Directory -Force (Join-Path $Work "dist\$Asset") | Out-Null
Copy-Item $Binary (Join-Path $Work "dist\$Asset\bedrock-mcp.exe")
foreach ($File in $Extra) { Copy-Item $File (Join-Path $Work "dist\$Asset\") }
Copy-Item (Join-Path $Root 'README.md'), (Join-Path $Root 'LICENSE') (Join-Path $Work "dist\$Asset\")
Compress-Archive -Path (Join-Path $Work "dist\$Asset") -DestinationPath (Join-Path $Work "dist\$Asset.zip")
Remove-Item -Recurse (Join-Path $Work "dist\$Asset")
Copy-Item (Join-Path $Root 'scripts\install.ps1') (Join-Path $Work 'dist\')
$Sums = Get-ChildItem (Join-Path $Work 'dist') | ForEach-Object {
  "$((Get-FileHash -Algorithm SHA256 $_.FullName).Hash.ToLower())  $($_.Name)"
}
Set-Content -Encoding ascii (Join-Path $Work 'dist\SHA256SUMS') $Sums

$Port = 20000 + ($PID % 20000)
$Server = Start-Process -PassThru -NoNewWindow python -ArgumentList (Join-Path $Root 'scripts\release-server.py'), (Join-Path $Work 'dist'), 'v9.9.9', $Port
$env:BEDROCK_MCP_RELEASES = "http://127.0.0.1:$Port/releases"
$env:LOCALAPPDATA = Join-Path $Work 'appdata'
New-Item -ItemType Directory -Force $env:LOCALAPPDATA | Out-Null
$InstallDir = Join-Path $env:LOCALAPPDATA 'bedrock-mcp\bin'
$Version = & $Binary --version
try {
  foreach ($Try in 1..50) {
    try { Invoke-WebRequest -UseBasicParsing -Method Head "$env:BEDROCK_MCP_RELEASES/latest" | Out-Null; break } catch { Start-Sleep -Milliseconds 100 }
  }

  Write-Output '1. fresh install with the documented one-liner (latest/download/install.ps1 piped to iex)'
  $output = & powershell -NoProfile -ExecutionPolicy Bypass -Command "irm '$env:BEDROCK_MCP_RELEASES/latest/download/install.ps1' | iex" 2>&1 | Out-String
  $r = @{ Ok = ($LASTEXITCODE -eq 0); Text = $output }
  if (-not $r.Ok) { Fail "install failed: $($r.Text)" }
  if ($r.Text -notmatch [regex]::Escape("Installed $($Version -replace '^bedrock-mcp ', '')")) { Fail "no install message: $($r.Text)" }
  if ((& (Join-Path $InstallDir 'bedrock-mcp.exe') --version) -ne $Version) { Fail 'the installed bedrock-mcp.exe does not run' }
  foreach ($File in $Extra) {
    if (-not (Test-Path (Join-Path $InstallDir (Split-Path -Leaf $File)))) { Fail "$(Split-Path -Leaf $File) was not installed" }
  }
  if ([Environment]::GetEnvironmentVariable('Path', 'User') -notlike "*$InstallDir*") { Fail 'the install folder was not added to Path' }

  Write-Output '2. running it again updates in place'
  $r = Install @()
  if (-not $r.Ok -or $r.Text -notmatch 'Updated') { Fail "update failed: $($r.Text)" }

  Write-Output '3. a pinned tag, with and without the v'
  if (-not (Install @('v9.9.9')).Ok) { Fail 'install v9.9.9 failed' }
  if (-not (Install @('9.9.9')).Ok) { Fail 'install 9.9.9 failed' }

  Write-Output '4. a release that does not exist'
  $r = Install @('v0.0.1')
  if ($r.Ok) { Fail 'installing a missing release succeeded' }

  Write-Output '5. a bad checksum changes nothing'
  $Before = (Get-FileHash (Join-Path $InstallDir 'bedrock-mcp.exe')).Hash
  $Bad = Get-Content (Join-Path $Work 'dist\SHA256SUMS') | ForEach-Object {
    if ($_ -like "*  $Asset.zip") { ('0' * 64) + "  $Asset.zip" } else { $_ }
  }
  Set-Content -Encoding ascii (Join-Path $Work 'dist\SHA256SUMS') $Bad
  $r = Install @()
  if ($r.Ok) { Fail 'a bad checksum was accepted' }
  if ($r.Text -notmatch 'does not match its checksum') { Fail "unclear error: $($r.Text)" }
  if ((Get-FileHash (Join-Path $InstallDir 'bedrock-mcp.exe')).Hash -ne $Before) { Fail 'a bad checksum changed the install' }

  Write-Output 'install.ps1: all checks passed'
} finally {
  Stop-Process -Id $Server.Id -ErrorAction SilentlyContinue
  [Environment]::SetEnvironmentVariable('Path', $UserPath, 'User')
  Remove-Item -Recurse -Force $Work -ErrorAction SilentlyContinue
}
