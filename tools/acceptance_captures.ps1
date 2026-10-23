# Records the rebuild's acceptance captures (design doc §10.4) with the Airspy.
# Scheduled for the 08:00–10:00 window; retries once a minute while the device is in use.
#
# Round 1 (default gain) at start, round 2 (manual gain 12/14/9) 45 minutes later.
# Each round: 60 s of air band centred 124.9 MHz, then 30 s of 851–860 MHz P25.
# Output: D:\SpecTrawl\testdata\captures\wideband_<date>_<round>_<band>.sigmf-{data,meta}

# Built from crates/engine-old/examples/capture_wideband.rs (cargo build --release -p engine-old --examples).
$exe      = "D:\SpecTrawl\target\release\examples\capture_wideband.exe"
$serial   = "735863DC2F650AE3"        # Airspy R2 that was receiver A
$outDir   = "D:\SpecTrawl\testdata\captures"
$deadline = (Get-Date).Date.AddHours(10)
$log      = Join-Path $outDir "acceptance_captures.log"
New-Item -ItemType Directory -Force $outDir | Out-Null

function Log($m) { $line = "{0:yyyy-MM-dd HH:mm:ss} {1}" -f (Get-Date), $m; $line | Tee-Object -FilePath $log -Append }

function Capture($stem, $mhz, $secs, $gain) {
    $args = @()
    if ($gain) { $args += @("--gain", $gain) }
    $args += @($serial, $mhz, $secs, (Join-Path $outDir $stem))
    while ((Get-Date) -lt $deadline) {
        Log "start $stem ($mhz MHz, $secs s, gain=$(if ($gain) {$gain} else {'default'}))"
        $out = & $exe @args 2>&1
        $out | ForEach-Object { Log "  $_" }
        if ($LASTEXITCODE -eq 0) { return $true }
        Log "device busy or failed; retrying in 60 s"
        Start-Sleep -Seconds 60
    }
    Log "deadline passed, $stem not recorded"
    return $false
}

$date = Get-Date -Format "yyyyMMdd"
Log "=== round 1 ==="
Capture "wideband_${date}_r1_airband" 124.9   60 $null | Out-Null
Capture "wideband_${date}_r1_p25"     855.436 30 $null | Out-Null

$round2 = (Get-Date).AddMinutes(45)
if ($round2 -lt $deadline) {
    Log "waiting for round 2 at $($round2.ToString('HH:mm'))"
    while ((Get-Date) -lt $round2) { Start-Sleep -Seconds 30 }
    Log "=== round 2 ==="
    Capture "wideband_${date}_r2_airband" 124.9   60 "12,14,9" | Out-Null
    Capture "wideband_${date}_r2_p25"     855.436 30 "12,14,9" | Out-Null
}
Log "done"
