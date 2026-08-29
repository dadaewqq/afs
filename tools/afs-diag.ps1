<#
.SYNOPSIS
  Pre-flight checks for the AFS host.

.DESCRIPTION
  Answers, in order, the questions that determine whether any of this is worth it:

    1. Is a device connected and authorised?
    2. What USB speed did the link actually negotiate?   <-- the important one
    3. Is the app installed, running, and holding all-files access?
    4. Is the port forwarded and the server answering?

  Question 2 is the one that decides expectations. Many phones wire only USB 2.0
  to the connector even when the connector is USB-C, which caps the link around
  40 MB/s no matter what protocol runs over it. If this reports high-speed, do not
  expect large-file copies to beat MTP by much — the wins will be in small files,
  random access and having a real drive letter.
#>

[CmdletBinding()]
param(
    [int]$Port = 8577
)

$ErrorActionPreference = 'Stop'

function Section($t) { Write-Host "`n== $t ==" -ForegroundColor Cyan }
function Ok($m)      { Write-Host "  [ok]   $m" -ForegroundColor Green }
function Warn($m)    { Write-Host "  [warn] $m" -ForegroundColor Yellow }
function Fail($m)    { Write-Host "  [FAIL] $m" -ForegroundColor Red }

Section "adb"
$adb = Get-Command adb -ErrorAction SilentlyContinue
if (-not $adb) { Fail "adb is not on PATH"; exit 1 }
Ok "adb at $($adb.Source)"

Section "device"
$devices = @(adb devices | Select-Object -Skip 1 | Where-Object { $_ -match '\S' })
if ($devices.Count -eq 0) { Fail "no device connected"; exit 1 }
foreach ($d in $devices) {
    if ($d -match 'unauthorized') { Fail "device present but UNAUTHORISED - accept the USB debugging prompt on the phone"; exit 1 }
    Ok $d.Trim()
}

$model   = (adb shell getprop ro.product.model 2>$null).Trim()
$release = (adb shell getprop ro.build.version.release 2>$null).Trim()
$sdk     = (adb shell getprop ro.build.version.sdk 2>$null).Trim()
Ok "$model - Android $release (API $sdk)"

if ([int]$sdk -lt 30) { Fail "API $sdk is below the minSdk of 30 (Android 11)" }

Section "USB link speed"
# The UDC (USB device controller) reports the negotiated speed. This is the phone's
# own view of the link, which is what matters - not what the PC's port is capable of.
$speed = (adb shell "cat /sys/class/udc/*/current_speed 2>/dev/null" | Select-Object -First 1)
if ($speed) { $speed = $speed.Trim() }

switch -Regex ($speed) {
    'super-speed-plus' { Ok "$speed  (USB 3.1+, ~1000 MB/s ceiling - storage will be the bottleneck)" }
    'super-speed'      { Ok "$speed  (USB 3.x, ~400 MB/s ceiling - lots of headroom over MTP)" }
    'high-speed'       {
        Warn "$speed  (USB 2.0, ~40 MB/s ceiling)"
        Warn "Large-file copies will NOT be dramatically faster than MTP on this link."
        Warn "The wins here are small files, random access, and a real drive letter."
    }
    'full-speed'       { Warn "$speed  (USB 1.1 - something is very wrong with the cable or port)" }
    default            { Warn "could not read the negotiated speed (path differs on some vendors)" }
}

Section "app"
$pkg = 'dev.androidfiles.afs'
$installed = adb shell pm list packages $pkg 2>$null
if (-not $installed) {
    Fail "$pkg is not installed"
} else {
    Ok "$pkg installed"

    $running = adb shell pidof $pkg 2>$null
    if ($running) { Ok "running (pid $($running.Trim()))" } else { Warn "not running - open the app and press Start" }

    $token = (adb shell cat /sdcard/.afs/token 2>$null)
    if ($token -and $token -notmatch 'No such file') {
        Ok "token readable ($($token.Trim().Substring(0,8))...)"
    } else {
        Warn "no token at /sdcard/.afs/token - open the app once to generate it"
    }
}

Section "port forward"
$fwd = adb forward --list 2>$null | Where-Object { $_ -match "tcp:$Port" }
if ($fwd) {
    Ok "forward active: $($fwd -join ', ')"
} else {
    Warn "no forward for tcp:$Port - setting one up"
    adb forward "tcp:$Port" "tcp:$Port" | Out-Null
    Ok "adb forward tcp:$Port tcp:$Port"
}

Section "server reachability"
$probe = Test-NetConnection -ComputerName 127.0.0.1 -Port $Port -WarningAction SilentlyContinue
if ($probe.TcpTestSucceeded) {
    Ok "127.0.0.1:$Port is accepting connections"
    Write-Host "`nNext: afs-host.exe --selftest" -ForegroundColor Cyan
} else {
    Fail "nothing listening on 127.0.0.1:$Port"
    Write-Host "  Start the server from the app, then re-run this script." -ForegroundColor Yellow
}
