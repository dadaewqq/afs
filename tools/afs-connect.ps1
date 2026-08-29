<#
.SYNOPSIS
  Builds and runs the AFS host.

.DESCRIPTION
  A thin convenience wrapper around `cargo run`. The port forward is no longer set
  up here: afs-host.exe finds adb itself, creates the forward on start and removes
  it on exit, so doing it out here would only mean two places to keep in step.

.PARAMETER Mount
  Drive letter to mount, e.g. Z:. Ignored when -SelfTest is given.

.PARAMETER SelfTest
  Exercise the protocol instead of mounting. Works without WinFsp installed.
#>

[CmdletBinding()]
param(
    [string]$Mount = 'Z:',
    [int]$Port = 8577,
    [string]$Adb,
    [switch]$SelfTest,
    [switch]$Release
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$hostDir  = Join-Path $repoRoot 'windows\afs-host'

$hostArgs = @('--port', $Port)
if ($SelfTest) { $hostArgs += '--selftest' } else { $hostArgs += @('--mount', $Mount) }
if ($Adb)      { $hostArgs += @('--adb', $Adb) }

Push-Location $hostDir
try {
    if ($Release) {
        Write-Host "cargo run --release -- $($hostArgs -join ' ')" -ForegroundColor Cyan
        cargo run --release -- @hostArgs
    } else {
        Write-Host "cargo run -- $($hostArgs -join ' ')" -ForegroundColor Cyan
        cargo run -- @hostArgs
    }
} finally {
    Pop-Location
}
