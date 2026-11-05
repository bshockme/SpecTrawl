# HydraSDR RFOne: firmware sweep mode

The RFOne (serial `36B463DC324C16C7`) runs custom firmware `v1.0.2-sweep2` with an on-device sweep.
The board retunes itself, waits until the R828D reports PLL lock, and sends only blocks captured after
that. Each block is tagged with its step and frequency. This is the `native_sweep` path in DESIGN §4.
Lock-verified, it does **680 hops/s**: 24–1800 MHz in 8 MHz steps at 5.4 GHz/s, about 3 full passes per
second. Retune-and-dwell is limited to one hop per USB transfer plus the lock wait.

Source, measurements and a working reference client live in `~/hydrasdr`:
- `scripts/hydrasdr_sweep.py`: Python, about 100 lines that matter.
- `experiments/2026-11-04-sweep-fw/README.md`.
- `firmware/patches/`.

Port that client rather than re-deriving anything here.

## Normal mode is untouched

Sweep mode exists only between SWEEP_START and SWEEP_STOP. Otherwise the board behaves exactly like
stock v1.0.2, so the existing `airspy` backend works unchanged.

Detect support from the version string (`hydrasdr_version_string_read`): it contains `sweep`, currently
`v1.0.2-sweep2`. Stock firmware STALLs request 0x70.

## Sequence

1. Open by serial with libhydrasdr, as today.
2. Configure the stream:

   | Call | Value |
   |---|---|
   | `set_samplerate` | 10 MSPS, so the ADC runs at 20 MSPS real |
   | `set_packing` | 0 (required, because sweep mode refuses packed streams) |
   | `set_rf_port` | as usual |
   | gain | set it here (see the warning below) |
   | `set_sample_type` | `HYDRASDR_SAMPLE_RAW` (5) |
   | `start_rx` | |

   Gain can't change during a sweep. Requests for gain, AGC, frequency, sample rate, packing, RF port or tuner registers are STALLed while a sweep runs.
3. Send **SWEEP_START** as a vendor control request on a *second* USB handle to the same device.
   libhydrasdr has no raw-request API. Find the second handle by VID:PID `38af:0001` and the serial
   string descriptor `HYDRASDR SN:36B463DC324C16C7`.
   - Request: `bmRequestType 0x40` (vendor, host→device, recipient device), `bRequest 0x70`, `wValue 0`,
     `wIndex 0`, followed by the 16-byte parameter block below.
   - A device-recipient request needs no interface claim. That's why it coexists with libhydrasdr's
     claimed interface. This is verified on Linux. On Windows (WinUSB) a second open will fail, and
     libhydrasdr would need a passthrough.
   - In Rust, use `rusb` (`DeviceHandle::write_control`), or dlopen `libusb-1.0` the way
     `backend-airspy` loads libhydrasdr.
4. Consume RAW callbacks. Each callback holds a whole number of 8192-sample (16 KB) blocks. Parse each
   block; discard any block that fails the tag check.
5. To change gain or the plan: send SWEEP_STOP (`bRequest 0x71`, no data), make the change, then
   SWEEP_START again. To finish: SWEEP_STOP, then `stop_rx`. `stop_rx` alone also ends the sweep.

SWEEP_START is acknowledged even when it doesn't start a sweep. That happens when the stream isn't
running, `n_steps` is 0, or `dwell_blocks` is 0. Confirm the sweep started by seeing tagged blocks.

## Parameters (16 bytes, little-endian, `<IIHBBBBH`)

| Offset | Type | Field | Notes |
|---|---|---|---|
| 0 | u32 | `f_start_hz` | **centre** frequency of step 0 |
| 4 | u32 | `f_step_hz` | centre spacing. 8 MHz recommended; usable flat span is ~9.3 MHz |
| 8 | u16 | `n_steps` | after the last step it wraps to step 0 and increments the sweep number |
| 10 | u8 | `skip_blocks` | extra 409.6 µs blocks discarded after lock. Use 0 |
| 11 | u8 | `dwell_blocks` | blocks sent per step. Use 1; more blocks average down noise |
| 12 | u8 | `lock_reads` | consecutive locked reads of R828D reg 2 bit 6 required. **2**; 0 disables the check: 1218 hops/s, but ~40 of 222 steps came out wrong, by up to −22 dB |
| 13 | u8 | reserved | 0 |
| 14 | u16 | `lock_timeout` | maximum number of lock polls (~0.1 ms each). 200 |

For example, 24–1800 MHz uses `f_start_hz` = 28 000 000, `f_step_hz` = 8 000 000 and `n_steps` = 222.

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
- Untagged blocks arrive before START and after STOP. Ignore them.
- Sequence gaps (an `(sweep, step)` pair skipped) were never seen in testing. Count them anyway.
- If lock timed out (bit 15 set), mark that step's bins suspect. It didn't happen in testing.

## Spectrum

The data is real-valued at 20 MSPS with the IF at 5 MHz, and the **spectrum is inverted**:

```
RF = centre_hz − (f_if − 5 MHz)        f_if = real-FFT bin frequency, 0..10 MHz
```

The reference client does the following:
1. Subtract the mean.
2. Apply a Hann window.
3. Run `rfft` with n = 8192. Bins are 2.44 kHz apart.
4. Keep bins with |RF − centre| ≤ step/2.
5. Reorder them into ascending RF.

The flat passband is about ±4.65 MHz. With 8 MHz steps, each hop's edges stay inside it.
To use the analytic-signal path instead, multiply by e^(−jπn/2), which shifts fs/4 to DC, then low-pass and decimate by 2. Remember the inversion.

## Throughput and timing

- USB load is 16 KB per hop per dwell block: about 11 MB/s at 680 hops/s, against a 44.9 MB/s ceiling.
- Most hops take 3 blocks (1.23 ms).
- Hops just above VCO divider changes (RF ≈ 50, 106, 216, 438 and 880 MHz) take longer, as does the low band.
- The wrap from the top of the range back to the bottom takes **~7 ms**. The lock check absorbs all of this; the host never has to guess.
- The stream is **not continuous** in sweep mode. Turn off the backend's delivered-vs-clock check
  (`check_delivery`, "LOSING n% OF SAMPLES") while sweeping, or it will report a huge shortfall.
  Use the trailer's DMA counter for timing instead.
- Gain is fixed for the whole pass. Linearity preset 10 is a reasonable survey default; the default maximum clips in FM and TV bands.

## Mapping onto SpecTrawl

- Set `Capabilities::native_sweep = true` when the version string contains `sweep`.
- A native sweep run produces finished per-hop power rows keyed by `(sweep, step, centre_hz)`.
  The Survey role's row assembly (`sweep_partial`, segments, spur blanking) can consume them directly, with no guard time or retune logic.
- **Segmented plans:** the firmware steps a uniform grid. Either sweep the full grid and drop hops
  outside the segments, or restart with a new `f_start_hz`/`n_steps` for each segment.
  A restart costs one 375 µs control request plus a retune.
- The RFOne can't be a Focus receiver while it sweeps. It is a Survey device, or the other.

Contact for firmware changes: the `~/hydrasdr` repo. New sweep firmware iterations get logged in `firmware/README.md` there.
