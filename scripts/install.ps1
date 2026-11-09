# Install or update Bedrock MCP for Minecraft by Red Eagle Studios on Windows x86_64:
#
#   irm https://github.com/RedEagleStudios/bedrock-mcp/releases/latest/download/install.ps1 | iex
#   & ([scriptblock]::Create((irm https://github.com/RedEagleStudios/bedrock-mcp/releases/latest/download/install.ps1))) v0.2.0
#
# Running it again updates in place. Windows cannot overwrite a running bedrock-mcp.exe, so the old
# files are renamed aside and deleted once nothing uses them, at the latest by the next update.
# $env:BEDROCK_MCP_RELEASES points it at another release location with the same layout (a mirror,
# or a local copy for testing).

# A script block keeps the settings and variables below out of the caller's session, and a
# download cut short runs nothing.
& {
  $ErrorActionPreference = 'Stop'
  # Windows PowerShell draws a progress bar for every downloaded chunk, which makes downloads crawl.
  $ProgressPreference = 'SilentlyContinue'
  [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

  $Releases = if ($env:BEDROCK_MCP_RELEASES) { $env:BEDROCK_MCP_RELEASES } else { 'https://github.com/RedEagleStudios/bedrock-mcp/releases' }
  $Asset = 'bedrock-mcp-windows-x86_64'

  if (-not [Environment]::Is64BitOperatingSystem) {
    throw 'bedrock-mcp: releases cover 64-bit Windows only.'
  }

  if ($args.Count -gt 0) {
    $Tag = [string]$args[0]
    if (-not $Tag.StartsWith('v')) { $Tag = "v$Tag" }
  } else {
    # releases/latest redirects to releases/tag/<newest tag>.
    $Request = [Net.WebRequest]::Create("$Releases/latest")
    $Request.Method = 'HEAD'
    $Response = $Request.GetResponse()
    $Tag = $Response.ResponseUri.Segments[-1]
    $Response.Close()
    if ($Tag -notmatch '^v\d') { throw "bedrock-mcp: $Releases has no release yet." }
  }

  $Root = Join-Path $env:LOCALAPPDATA 'bedrock-mcp'
  $Dir = Join-Path $Root 'bin'
  $Exe = Join-Path $Dir 'bedrock-mcp.exe'
  New-Item -ItemType Directory -Force $Dir | Out-Null
  # Files a previous update renamed aside; the ones still in use stay until next time.
  Get-ChildItem $Dir -Filter '*.old' | Remove-Item -ErrorAction SilentlyContinue

  $Tmp = Join-Path $Root ".update-$PID"
  Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
  New-Item -ItemType Directory $Tmp | Out-Null
  try {
    Write-Output "Downloading bedrock-mcp $Tag"
    $Zip = Join-Path $Tmp "$Asset.zip"
    $Sums = Join-Path $Tmp 'SHA256SUMS'
    Invoke-WebRequest -UseBasicParsing "$Releases/download/$Tag/$Asset.zip" -OutFile $Zip
    Invoke-WebRequest -UseBasicParsing "$Releases/download/$Tag/SHA256SUMS" -OutFile $Sums
    $Pattern = '^([0-9a-f]{64}) [ *]' + [regex]::Escape("$Asset.zip") + '$'
    $Line = Get-Content $Sums | Where-Object { $_ -match $Pattern } | Select-Object -First 1
    if (-not $Line) { throw "bedrock-mcp: SHA256SUMS of release $Tag does not list $Asset.zip." }
    if ((Get-FileHash -Algorithm SHA256 $Zip).Hash -ne $Line.Substring(0, 64)) {
      throw "bedrock-mcp: $Asset.zip does not match its checksum; nothing was changed."
    }
    Expand-Archive $Zip -DestinationPath $Tmp
    $New = Join-Path $Tmp $Asset
    $Installed = & (Join-Path $New 'bedrock-mcp.exe') --version
    if ($LASTEXITCODE -ne 0) { throw 'bedrock-mcp: the downloaded bedrock-mcp.exe does not run; nothing was changed.' }

    $Previous = $null
    if (Test-Path $Exe) {
      try { $Previous = $null | & $Exe --version } catch { $Previous = $null }
      if ($Previous -notlike 'bedrock-mcp *') { $Previous = 'bedrock-mcp an unknown version' }
    }
    foreach ($File in Get-ChildItem $New) {
      $Target = Join-Path $Dir $File.Name
      if (Test-Path $Target) {
        # A running executable or a loaded DLL can be renamed but not overwritten.
        $Old = "$Target.$PID.old"
        Move-Item $Target $Old
        Remove-Item $Old -ErrorAction SilentlyContinue
      }
      Move-Item $File.FullName $Target
    }
  } finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
  }

  $User = [EnvironmentVariableTarget]::User
  $Path = [Environment]::GetEnvironmentVariable('Path', $User)
  if (";$Path;".ToLower() -notlike "*;$($Dir.ToLower());*") {
    [Environment]::SetEnvironmentVariable('Path', "$Path;$Dir".TrimStart(';'), $User)
    $env:Path += ";$Dir"
  }

  Write-Output ''
  $Version = $Installed -replace '^bedrock-mcp ', ''
  if ($Previous) {
    Write-Output "Updated $Version (was $($Previous -replace '^bedrock-mcp ', '')) in $Dir."
    Write-Output 'Restart your MCP client to load it.'
  } else {
    Write-Output "Installed $Version in $Dir."
    Write-Output ''
    Write-Output 'MCP client configuration:'
    Write-Output ''
    Write-Output "  { `"mcpServers`": { `"minecraft`": { `"command`": `"$($Exe.Replace('\', '\\'))`" } } }"
  }
  Write-Output 'Run this installer again to update.'
} @args
