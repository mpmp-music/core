[中文](../zh-CN/design/loudness-volume.md)

# Loudness and volume

The implementation spans core's `gain.rs` and `loudness_scan.rs`, the engine's gain and analysis paths, and the library's loudness cache. See the [contract schema](../contract.schema.json) for the current interface.

By default, background analysis backfills missing or stale loudness data. Playback can start before the whole library has been measured. There are no routine scan, clear, or recalculate controls. Playback can start with valid file tags and prefers valid derived cache entries. Results go only into the database and leave the music file tags intact. Changes to the file, CUE sheet, range, or version invalidate the cache. The file size and mtime model cannot detect a replacement that deliberately preserves both values.

Automatic traversal reads batches by ID. Waveform and loudness analysis share a single analysis decoder gate. The actor schedules work and publishes events. Track results are committed in batches. Album results wait for valid statistics for every currently available and visible member, then aggregate the stored gating statistics without averaging LUFS. Silence or an insufficient number of valid blocks is recorded as measured without an integrated value. Permanent decoding errors are remembered by version and retried only after the version changes. Transient errors use backoff. Background events have `origin=null`. An automatic LoudnessScan has `total=null` and `done` counts cache hits as well as new results. JobId values are scoped to a single core run.

## Gain

Software volume is on by default. The slider takes `volume∈[0,1]` and the actual multiplier is `volume³`. Mute is independent. System volume mode currently returns `unsupported/not_implemented`. ReplayGain has off, track, album, and smart modes, with smart as the default. When shuffle is off and playback is in an album context or adjacent tracks are from the same album, smart uses album gain. Otherwise it uses track gain. If the chosen gain is missing, it falls back to the other mode.

```text
With data: dB = selected gain + preamp_db
Without data: dB = fallback_preamp_db
Clipping prevention: when a peak is available, cap dB at -20 log10(peak)
Final multiplier = 10^(dB/20) × volume³
```

SetReplayGain updates only the supplied fields. Missing ReplayGain data, valid zero gain, and disabled ReplayGain are distinct states. The signal path shows each state and preserves the source of the data even when the final multiplier is 1. Volume and gain transitions are smoothed over about 10 ms. Buffered audio can still use the old gain. For adjacent tracks, transition to lower gain at the end of the preceding track and to higher gain at the start of the next track. This avoids amplifying the next track's attack.

A multiplier of 1 can bypass multiplication. Determine `bit_perfect` from the complete signal path. Resampling, mixing, quantization, dither, backends, and unverified device paths all affect the result. For 16-bit output, TPDF dither depends on actual processing and source precision. I24 occupies three bytes. For 24-in-32, use I32 and `valid_bits_per_sample=24`. The `verified` and `exclusive_verified` results cover only the endpoint driver boundary and cannot prove bit-for-bit identity inside the DAC. The current CPAL path does not gain exclusive-mode support from the contract's exclusive enum. Runtime device failures preserve user preferences and are reported in OutputState.
