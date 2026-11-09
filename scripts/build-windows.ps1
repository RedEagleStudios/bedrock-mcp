# Build bedrock-mcp natively on Windows in release mode.
#
# The server (bedrock-mcp.exe), the agent (bedrock_agent.dll) and the unstable layer
# (bedrock_unstable.dll) land together in target\release, where bedrock-mcp.exe
# looks for the DLLs. The server injects in-process, so bedrock-inject.exe is not
# needed on Windows.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
cargo build --release --manifest-path (Join-Path $root 'Cargo.toml') -p bedrock-mcp -p bedrock-agent -p bedrock-unstable
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Write-Output "Built $(Join-Path $root 'target\release\bedrock-mcp.exe')"
