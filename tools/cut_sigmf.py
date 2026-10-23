#!/usr/bin/env python3
"""Cut a sample range out of a SigMF pair.

usage: cut_sigmf.py <in-stem> <out-stem> <start_s> <seconds>

Stems are paths without the .sigmf-data/.sigmf-meta suffix (absolute or
relative). Keeps datatype, sample rate and frequency, and records
`core:description = "cut from <in-stem> at <start_s> s"`. Supports cf32_le
and ci16_le.
"""

import json
import os
import sys

BYTES_PER_SAMPLE = {"cf32_le": 8, "ci16_le": 4}


def main(argv: list[str]) -> int:
    if len(argv) != 5:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    in_stem, out_stem, start_s, seconds = argv[1], argv[2], float(argv[3]), float(argv[4])
    with open(in_stem + ".sigmf-meta", encoding="utf-8") as f:
        meta = json.load(f)
    g = meta["global"]
    datatype = g["core:datatype"]
    if datatype not in BYTES_PER_SAMPLE:
        print(f"unsupported datatype {datatype}", file=sys.stderr)
        return 1
    bps = BYTES_PER_SAMPLE[datatype]
    fs = float(g["core:sample_rate"])
    total = os.path.getsize(in_stem + ".sigmf-data") // bps
    first = int(round(start_s * fs))
    count = int(round(seconds * fs))
    if first < 0 or count <= 0 or first + count > total:
        print(f"range {start_s} s + {seconds} s outside the {total / fs:.3f} s capture", file=sys.stderr)
        return 1
    with open(in_stem + ".sigmf-data", "rb") as f:
        f.seek(first * bps)
        data = f.read(count * bps)
    freq = None
    for c in meta.get("captures", []):
        if "core:frequency" in c:
            freq = c["core:frequency"]
            break
    out_global = {
        "core:datatype": datatype,
        "core:sample_rate": fs,
        "core:version": g.get("core:version", "1.2.0"),
        "core:description": f"cut from {in_stem} at {argv[3]} s",
    }
    if "core:recorder" in g:
        out_global["core:recorder"] = g["core:recorder"]
    capture = {"core:sample_start": 0}
    if freq is not None:
        capture["core:frequency"] = freq
    out_meta = {"global": out_global, "captures": [capture], "annotations": []}
    os.makedirs(os.path.dirname(os.path.abspath(out_stem)), exist_ok=True)
    with open(out_stem + ".sigmf-data", "wb") as f:
        f.write(data)
    with open(out_stem + ".sigmf-meta", "w", encoding="utf-8", newline="\n") as f:
        json.dump(out_meta, f, indent=2, sort_keys=True)
        f.write("\n")
    print(f"{out_stem}.sigmf-data: {len(data)} bytes ({count} samples, {count / fs:.3f} s)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
