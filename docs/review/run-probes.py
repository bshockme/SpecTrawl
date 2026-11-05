"""Run standalone review probes against the local Cargo libraries.

These probes assert desired behavior. They failed on the reviewed revision
(30a37dc); after the fixes all pass except the full-span noise-floor probe,
a known limitation (BACKLOG.md). Use --long-ddc to additionally exercise
the exact DDC source over 600 s of synthetic samples. No radio or network
access is involved.
"""

import argparse
import json
import pathlib
import subprocess
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[2]
DEPS = ROOT / "target/debug/deps"


def libraries():
    """The rlibs of one consistent `cargo build`, by crate name.

    target/debug/deps can hold several builds of a crate (different feature
    sets from `cargo test` and `cargo build`); mixing them makes rustc see
    two distinct `num_complex` crates. Cargo's own artifact list avoids that.
    """
    out = subprocess.run(
        ["cargo", "build", "--workspace", "--message-format=json-render-diagnostics"],
        check=True, cwd=ROOT, capture_output=True, text=True,
    ).stdout
    libs = {}
    for line in out.splitlines():
        message = json.loads(line)
        if message.get("reason") != "compiler-artifact":
            continue
        for filename in message["filenames"]:
            if filename.endswith(".rlib"):
                libs[message["target"]["name"].replace("-", "_")] = filename
    return libs


LONG_DDC = r"""
fn main() {
    let mut d = Ddc::new(10e6, 0.0, 8000.0, 48000.0);
    let x = vec![Complex32::new(1.0, 0.0); 100000];
    let mut out = Vec::new();
    for epoch in 0..6 {
        let (mut lo, mut hi, mut sum, mut count) = (f32::MAX, f32::MIN, 0f64, 0usize);
        for _ in 0..10000 {
            out.clear();
            d.process(&x, &mut out);
            for v in &out {
                lo = lo.min(v.re);
                hi = hi.max(v.re);
                sum += (v.re as f64 - 1.0).powi(2);
                count += 1;
            }
        }
        let rms = (sum / count as f64).sqrt();
        println!("actual Rust Ddc {}..{} s: min {lo:.6}, max {hi:.6}, RMS error {rms:.6}", epoch * 100, (epoch + 1) * 100);
    }
}
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--long-ddc", action="store_true")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="spectrawl-review-") as tmp:
        tmp = pathlib.Path(tmp)
        binary = tmp / "probes"
        libs = libraries()
        command = ["rustc", "--edition=2024", "--test", "-O",
                   str(ROOT / "docs/review/algorithm-probes.rs"),
                   "-L", f"dependency={DEPS}", "-o", str(binary)]
        for name in ("detect", "engine_old", "sdr_core"):
            command += ["--extern", f"{name}={libs[name]}"]
        subprocess.run(command, check=True, cwd=ROOT)
        result = subprocess.run([str(binary), "--nocapture"], cwd=ROOT)
        if args.long_ddc:
            # Copy the unmodified implementation, adding only a main function.
            source = tmp / "ddc.rs"
            source.write_text((ROOT / "crates/dsp/src/ddc.rs").read_text() + LONG_DDC)
            binary = tmp / "ddc"
            subprocess.run(["rustc", "--edition=2024", "-O", str(source),
                            "-L", f"dependency={DEPS}", "--extern",
                            f"num_complex={libs['num_complex']}", "-o", str(binary)],
                           check=True, cwd=ROOT)
            subprocess.run([str(binary)], check=True, cwd=ROOT)
        return result.returncode


if __name__ == "__main__":
    raise SystemExit(main())
