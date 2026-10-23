# Download the captures listed in testdata/captures/MANIFEST.toml from the
# GitHub release and verify their sha256. Needs the gh CLI.
# Works in Windows PowerShell 5.1 and pwsh 7.
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$dest = Join-Path $root 'testdata/captures'
$text = Get-Content -Raw -Path (Join-Path $dest 'MANIFEST.toml')

$release = $null
$captures = @()
$cur = $null
foreach ($line in ($text -split "`r?`n")) {
    $l = $line.Trim()
    if ($l -match '^\[\[capture\]\]') {
        $cur = @{ name = $null; sha256 = $null }
        $captures += , $cur
    } elseif ($l -match '^\[') {
        $cur = $null
    } elseif ($l -match '^(\w+)\s*=\s*"([^"]*)"') {
        if ($null -eq $cur -and $Matches[1] -eq 'release') { $release = $Matches[2] }
        elseif ($null -ne $cur) { $cur[$Matches[1]] = $Matches[2] }
    }
}

if ($captures.Count -eq 0) {
    Write-Output 'manifest lists no captures'
    exit 0
}
if (-not $release) { Write-Error 'manifest has no release'; exit 1 }

foreach ($c in $captures) {
    $name = $c.name
    $data = Join-Path $dest "$name.sigmf-data"
    $meta = Join-Path $dest "$name.sigmf-meta"
    if ((Test-Path $data) -and (Test-Path $meta) -and ((Get-FileHash -Algorithm SHA256 $data).Hash -eq $c.sha256)) {
        Write-Output "${name}: up to date"
        continue
    }
    Write-Output "${name}: downloading"
    & gh release download $release -R auggiesoft/SpecTrawl -p "$name.*" -D $dest --clobber
    if ($LASTEXITCODE -ne 0) { [Console]::Error.WriteLine("${name}: download failed"); exit 1 }
    if (-not ((Test-Path $data) -and (Test-Path $meta))) { [Console]::Error.WriteLine("${name}: asset missing"); exit 1 }
    $got = (Get-FileHash -Algorithm SHA256 $data).Hash
    if ($got -ne $c.sha256) { [Console]::Error.WriteLine("${name}: sha256 mismatch (got $got, want $($c.sha256))"); exit 1 }
    Write-Output "${name}: ok"
}
