#!/bin/sh
# Download the captures listed in testdata/captures/MANIFEST.toml from the
# GitHub release and verify their sha256. Run from anywhere; needs gh and python3.
set -eu
root="$(cd "$(dirname "$0")/.." && pwd)"
manifest="$root/testdata/captures/MANIFEST.toml"
dest="$root/testdata/captures"

lines="$(python3 -c '
import sys, tomllib
m = tomllib.load(open(sys.argv[1], "rb"))
for c in m.get("capture", []):
    print(m["release"], c["name"], c["sha256"], sep="\t")
' "$manifest")"

if [ -z "$lines" ]; then
    echo "manifest lists no captures"
    exit 0
fi

sha() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
    else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

status=0
tab="$(printf '\t')"
printf '%s\n' "$lines" | while IFS="$tab" read -r release name want; do
    data="$dest/$name.sigmf-data"
    if [ -f "$data" ] && [ -f "$dest/$name.sigmf-meta" ] && [ "$(sha "$data")" = "$want" ]; then
        echo "$name: up to date"
        continue
    fi
    echo "$name: downloading"
    gh release download "$release" -R auggiesoft/SpecTrawl -p "$name.*" -D "$dest" --clobber || { echo "$name: download failed" >&2; exit 1; }
    if [ ! -f "$data" ] || [ ! -f "$dest/$name.sigmf-meta" ]; then echo "$name: asset missing" >&2; exit 1; fi
    got="$(sha "$data")"
    if [ "$got" != "$want" ]; then echo "$name: sha256 mismatch (got $got, want $want)" >&2; exit 1; fi
    echo "$name: ok"
done || status=1
exit $status
