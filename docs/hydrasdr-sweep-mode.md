# HydraSDR RFOne: firmware sweep mode

The RFOne (serial `36B463DC324C16C7`) runs custom firmware `v1.0.2-sweep3` with an on-device sweep.
The board retunes itself, waits until the R828D reports PLL lock, and sends only blocks captured after
that. Each block is tagged with its step and frequency. This is the `native_sweep` path in DESIGN §4.
Lock-verified, it does about **673 hops/s** on average over 24–1800 MHz in 8 MHz steps, about 5.4 GHz/s
or 3 full passes per second.

SpecTrawl drives the sweep through the sweep calls in a patched libhydrasdr. An earlier version of this
doc sent vendor requests on a second USB handle; that approach is superseded and would fail on Windows.

Source, measurements and reference clients live in `~/hydrasdr`:
- `host/`: the patched library, with API docs in `host/README.md` and the `sweep_api` group in `hydrasdr.h`.
- `scripts/hydrasdr_sweep.py` and `host/tests/test_sweep_api.py`: ctypes clients.
- `experiments/2026-11-04-sweep-fw/README.md`.
- `firmware/patches/`; firmware iterations are logged in `firmware/README.md`.

## The library

The patched library is a strict superset of libhydrasdr 1.1.4, so it replaces the stock one.

| Platform | File |
|---|---|
| Linux | `~/hydrasdr/host/dist/linux-x86_64/libhydrasdr.so.1.1.4` |
| Windows x64 (MinGW) | `~/hydrasdr/host/dist/windows-x64/hydrasdr.dll` plus `libusb-1.0.dll`; untested on hardware |

Each folder has a `SHA256SUMS`. The system (AUR) library is the stock one, so point SpecTrawl at the
patched one:

```
SPECTRAWL_HYDRASDR_LIB=$HOME/hydrasdr/host/dist/linux-x86_64/libhydrasdr.so.1.1.4
```

The backend resolves the sweep calls as optional symbols. With the stock library, or stock firmware,
`Capabilities::native_sweep` is false and Sweep mode retunes from the host as before. If the firmware can
sweep but the library can't, the backend logs a warning naming the variable above.

New calls, all `ADDCALL` (cdecl):

| Call | Returns |
|---|---|
| `hydrasdr_sweep_supported(dev)` | 1 if the firmware version contains `-sweep`, 0 if not, below 0 on error |
| `hydrasdr_sweep_start(dev, const hydrasdr_sweep_params_t*)` | 0; −2 for a bad plan or unless the sample type is RAW (5) with packing off; −1003 unless streaming; −12 on stock firmware. Calling it again replaces the plan without a stop |
| `hydrasdr_sweep_stop(dev)` | 0; the stream carries on in normal mode. `hydrasdr_stop_rx` also ends a sweep |
| `hydrasdr_sweep_parse_block(const uint16_t* block, info*)` | 0 valid, 1 untagged (normal-mode data), 2 torn (drop it) |

SpecTrawl parses blocks itself (`backend-airspy/src/native.rs`), with the same checks as
`hydrasdr_sweep_parse_block`, so the tests run without the library.

## Sequence

1. Open by serial with libhydrasdr, as for normal streaming.
2. Configure the stream:

   | Call | Value |
   |---|---|
   | `set_samplerate` | 10 MSPS, so the ADC runs at 20 MSPS real |
   | `set_packing` | 0 (sweep mode refuses packed streams) |
   | `set_rf_port` | as usual |
   | gain | set it here (see below) |
   | `set_sample_type` | `HYDRASDR_SAMPLE_RAW` (5) |
   | `start_rx` | |

3. `hydrasdr_sweep_start` with the plan.
4. Consume RAW callbacks. Each callback holds a whole number of 8192-sample (16 KB) blocks. Parse each
   block and discard any that isn't valid.
5. To change gain: `hydrasdr_sweep_stop`, change it, `hydrasdr_sweep_start` again. Requests for gain, AGC,
   frequency, sample rate, packing, RF port or tuner registers are STALLed while a sweep runs.
6. To finish: `hydrasdr_sweep_stop`, then restore the sample type and packing, or `stop_rx`.

## Parameters (`hydrasdr_sweep_params_t`, 16 bytes, natural alignment, no padding)

| Offset | Type | Field | Notes |
|---|---|---|---|
| 0 | u32 | `f_start_hz` | **centre** frequency of step 0 |
| 4 | u32 | `f_step_hz` | centre spacing. 8 MHz recommended; the usable flat span is ~9.3 MHz (SpecTrawl uses 9 MHz) |
| 8 | u16 | `n_steps` | after the last step it wraps to step 0 and increments the sweep number |
| 10 | u8 | `skip_blocks` | extra 409.6 µs blocks discarded after lock. Use 0 |
| 11 | u8 | `dwell_blocks` | blocks sent per step. More blocks average down noise (SpecTrawl's default is 4) |
| 12 | u8 | `lock_reads` | consecutive locked reads of R828D reg 2 bit 6 required. **2**; 0 disables the check: 1218 hops/s, but ~40 of 222 steps came out wrong, by up to −22 dB |
| 13 | u8 | reserved | 0 |
| 14 | u16 | `lock_timeout` | maximum number of lock polls (~0.1 ms each). 200 |

For example, 24–1800 MHz in 8 MHz steps uses `f_start_hz` = 28 000 000, `f_step_hz` = 8 000 000 and
`n_steps` = 222.

## Block format

A block is 8192 `u16` words. Each word is a 12-bit ADC sample in the low bits, with offset 2048.
The first 8 and last 8 words are overwritten by tags, so **samples are words 8..8184**: 8176 samples, 408.8 µs.

| Word | Header | Trailer |
|---|---|---|
| 0 | `0xA55A` | `0x5AA5` |
| 1 | `0x5357` | lock info: bits 0–14 = polls used, bit 15 = lock timed out |
| 2 | step | step |
| 3 | block within step (0..dwell) | block within step |
| 4–5 | sweep number (u32 LE) | device DMA block counter (u32 LE, 409.6 µs per tick: a device-side timestamp) |
| 6–7 | centre frequency, Hz (u32 LE) | centre frequency, Hz |

A block is **valid** only if both magics match and the header's step, block and frequency equal the trailer's.

- A header that matches but whose trailer differs means the DMA overwrote the block while USB was still reading it. Drop it.
- Untagged blocks arrive before the sweep starts and after it stops. Ignore them.
- Sequence gaps (a `(sweep, step)` pair skipped) were never seen in testing. Count them anyway.
- If lock timed out (bit 15 set), drop or mark that step. It didn't happen in testing.
- Check each block's centre against `f_start_hz + step × f_step_hz` and drop it if it differs. SpecTrawl
  does (`misfiled` in the backend's end-of-sweep log line). On `sweep2` a restart (stop, set gain, start)
  that landed during the previous run's retune let that retune finish afterwards: the next block was
  tagged step 0 but captured at the old frequency, and since each start reset the sweep number, a
  restart in the first pass re-sent `(0, 0)` tags. Fixed in `sweep3` (2026-11-05, ~/hydrasdr
  `experiments/2026-11-05-restart-race/`); the check stays as a guard and also catches blocks left over
  from a replaced plan.
- From `sweep3` the sweep number keeps counting across restarts: the first pass is sweep 1, and
  `(sweep, step)` is unique within a boot.

## Spectrum

The data is real-valued at 20 MSPS with the IF at 5 MHz, and the **spectrum is inverted**:

```
RF = centre_hz − (f_if − 5 MHz)        f_if = real-FFT bin frequency, 0..10 MHz
```

The flat passband is about ±4.65 MHz.

SpecTrawl converts each block to IQ (`native::RawToIq`), so the engine sees ordinary 10 MS/s hops:
1. Subtract the mean.
2. Multiply by j^n, which shifts fs/4 to DC and undoes the inversion.
3. Apply a 95-tap Kaiser half-band and decimate by 2: 4088 IQ samples per block.

The scale matches libhydrasdr's float IQ (a full-scale real tone becomes complex amplitude 0.5), so
noise floors read the same as in normal streaming.

## Throughput and timing

- USB load is 16 KB per hop per dwell block: about 11 MB/s at 680 hops/s, against a 44.9 MB/s ceiling.
- Most hops take 3 blocks (1.23 ms).
- The low band, 28–60 MHz, takes 2.5–4 ms per hop.
- Crossing one of the tuner's VCO divider switches (RF ≈ 52, 106, 220, 442 and 885 MHz), in either
  direction, costs about **10 ms**. A plan pays that once per crossing per pass. No band is slow in itself:
  a sub-band plan that stays between two switch points runs at about 700–800 hops/s, depending on its own
  wrap (a 3-step 500–516 MHz plan measured 736; 3-block hops with no wrap would give ~800).
  - A small plan that straddles a switch point is slow out of proportion. The 4-step 100–124 MHz test plan
    crossed 106 MHz twice per pass and so measured only ~188 hops/s.
  - Leaving a band out of the plan doesn't avoid a switch: the hop over the gap still crosses it.
- The wrap from the top of the range back to the bottom takes **~8 ms**. A back-and-forth order would save
  only that (about 1.5%).
- The lock check absorbs all of this; the host never has to guess.
- Source: per-step hop times on sweep3 over 24–1800 MHz, in `~/hydrasdr/experiments/2026-11-05-sweep-order/`.
- The stream is **not continuous** in sweep mode. The backend's delivered-vs-clock check
  (`check_delivery`) is off while sweeping; the trailer's DMA counter times each hop instead.
- Gain is fixed for the whole pass. Linearity preset 10 is a reasonable survey default; the default
  maximum clips in FM and TV bands.

## In SpecTrawl

- `backend-airspy` sets `Capabilities::native_sweep` when the sweep calls resolve and
  `hydrasdr_sweep_supported` returns 1. `Device::start_native_sweep` and `stop_native_sweep` start and
  stop it. Each hop reaches the engine as a `Retune` at its first sample, followed by its blocks.
- The engine's Sweep mode uses the board sweep when it can (`SweepPlan::native_dwell` > 0, default 4),
  reads hops off the segment table and stitches them into passes the same way as a host-retuned sweep.
- Measured on air with 9 MHz steps, 24–1797 MHz (197 hops):

  | Dwell | Hops/s | Rate | Pass |
  |---|---|---|---|
  | 1 | ~649 | 5.8 GHz/s | 0.30 s |
  | 2 | ~522 | 4.7 GHz/s | 0.39 s |
  | 4 | ~355–375 | 3.3 GHz/s | 0.54 s |
  | host retuning | 144 | 1.3 GHz/s | 1.38 s |

- The RFOne can't be a Focus receiver while it sweeps.
- `crates/backend-airspy/examples/native_sweep.rs` exercises the backend alone:
  `SPECTRAWL_HYDRASDR_LIB=… target/release/examples/native_sweep 36B463DC324C16C7 6 1`.
