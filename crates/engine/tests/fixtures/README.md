# Encoded audio fixtures

Fixtures for playback seams, seeks, metadata, and unsupported-format tests. In desktop, this source record accompanies the two FLAC files listed in the adjacent `SOURCE.md`; the full corpus and regeneration tools live in core.
The signals are deterministic. To regenerate the corpus from the core repository root, run the following command.

    FFMPEG=/path/to/ffmpeg scripts/gen-fixtures.sh      # from the repo root

Generated with `ffmpeg version n8.1.3-18-g46d899d7a0-20261020` (BtbN static build
`ffmpeg-n8.1-latest-linux64-gpl-8.1.tar.xz`; encoders libmp3lame, libvorbis, aac, libopus, flac).
The exact version line is also written to `FFMPEG_VERSION.txt` by the script.
Source PCM is made by `scripts/gen_signal.py` (stdlib only), 16-bit stereo, rounded to nearest:

    L[n] = 0.5 * sin(2*pi*997*n/rate)
    R[n] = 0.5 * sin(2*pi*1499*n/rate + 0.3)

n is the frame index in the *unsplit* signal, so concatenating parts a+b+c reproduces `sine44` exactly
(phase continuous across seams).

## Authorship and license

Copyright (c) 2026 CocytusChen. The source PCM and encoded samples are original,
synthetic sine signals created by the author, with no recordings from real songs.
They are released with the project under **MIT OR Apache-2.0**, at your option
(see the root LICENSE-MIT and LICENSE-APACHE). FFmpeg is only an external generation
tool; no FFmpeg executable is distributed in this repository. Its recorded build
license describes that tool, not the license declaration for these original signals.

## Signals and split points

- `sine44`: 44100 Hz, 132300 frames (3.0 s). Parts: a = [0, 37123) (37123 frames),
  b = [37123, 81457) (44334 frames), c = [81457, 132300) (50843 frames). Split points are deliberately
  not aligned to any codec frame size (1152 / 1024 / 2048-ish / 960).
- `sine48`: 48000 Hz, 48000 frames (1.0 s), single track, for 44.1k -> 48k cross-rate seams.

## Files

| Files | Format |
|---|---|
| `sine44_{a,b,c}.flac`, `sine44_full.flac`, `sine48.flac` | FLAC 16-bit (lossless reference) |
| `sine44_{a,b,c}.wav` | WAV PCM s16le |
| `sine44_{a,b,c}.mp3`, `sine44_full.mp3`, `sine48.mp3` | MP3 libmp3lame CBR 192k, Xing/Info + LAME tag (delay 576, padding per file) |
| `sine44_{a,b,c}.ogg`, `sine44_full.ogg` | Ogg Vorbis libvorbis -q:a 5 (gapless via final granule position) |
| `sine44_{a,b,c}.m4a`, `sine44_full.m4a` | AAC-LC in MP4, native `aac` 192k (has an `elst` edit list, no iTunSMPB) |
| `sine44_{a,b,c}.opus` | Opus in Ogg, libopus 128k. FFmpeg resamples the source to 48 kHz. The parts are about 40,407 / 48,255 / 55,340 frames, with pre-skip 312. The player has no Opus decoder. These files also serve metadata and tag-editing tests. |

Each part is encoded separately from its own WAV, so gapless metadata is per file.

## Commands

Common flags: `ffmpeg -hide_banner -loglevel error -y -bitexact -i SRC.wav -map_metadata -1 ...`

- FLAC: `-c:a flac -sample_fmt s16`
- WAV: `-c:a pcm_s16le -write_bext 0`
- MP3: `-c:a libmp3lame -b:a 192k -write_xing 1`
- Vorbis: `-c:a libvorbis -q:a 5`
- M4A: `-c:a aac -b:a 192k`
- Opus: `-c:a libopus -b:a 128k`

## Known ffmpeg behaviour (for reference decoders)

- ffmpeg trims MP3 (LAME tag), FLAC, WAV exactly; Vorbis via granule position (see caveat below);
  Opus via pre-skip; M4A decode via `ffmpeg` CLI is NOT trimmed (whole 1024-sample AAC frames).
- Ogg `sine44_a.ogg`: the file's last granule is correct (37123) but the ffmpeg CLI decodes 37248
  (a multiple of 128); treat the granule position, not the ffmpeg output, as truth.
- M4A has `edts/elst` (media_time 1024) and no `iTunSMPB`; ffmpeg's mov muxer does not write it.

## SHA-256

Digests identify the committed files; regeneration may depend on the recorded FFmpeg build.

| File | SHA-256 |
|---|---|
| `FFMPEG_VERSION.txt` | `647356b4dfa2291dcc6e94a0ef4db3808680f1895fea7ee8116d2086e202b6ec` |
| `sine44_a.flac` | `3b6202cf4217b69a75c052d188cdcd6eb9a43d2b4933c94ce4587757144a220b` |
| `sine44_a.m4a` | `4aeca5f3c290304131279c178e35a9d327f98bb93122e0f0b75f9f0c1435b1ba` |
| `sine44_a.mp3` | `859d6de7625ed9b965e64e5ceecf65c2bb92f2a230427627cd933e2ac3cd98d8` |
| `sine44_a.ogg` | `7e7d2310f2c12b258c7b6baa857f8eb2cfbcb9e079a709c369956817b7d13e76` |
| `sine44_a.opus` | `ad932db4a3300b8f4b7d34b44c851b53f1e977ee78d61a240038a8551411ae29` |
| `sine44_a.wav` | `c43b121eaacebf126bb035c5ca8cc4f3bb3c4860d924542de7bdf37b7611e334` |
| `sine44_b.flac` | `278554df9a5cb8a8df053bc0f86a57ad2a5f548e57327fe8d37950011c652309` |
| `sine44_b.m4a` | `e21085856c932a47d5d9c5d0e45e7b75f6a776fa96850d7e2d1d07ddf7726c80` |
| `sine44_b.mp3` | `e53c82400eef10d7a3ada0b04787cfc1f513ae5a4aba7771b4174c153ce0379d` |
| `sine44_b.ogg` | `bd383eae4bda805a3308a7c382a04179d59dfb1fcc6b64ea10a9d3ac2d844fc1` |
| `sine44_b.opus` | `ab2097cc8ce63b618068c856b4531309bbf27f85c5ba9a4186ec0def6c2142ad` |
| `sine44_b.wav` | `087e97eca459de154d74f833c4ca06455e835ced90936fd8d5bea34a485065b1` |
| `sine44_c.flac` | `f7dfa5ee4e7e035c4d3c60e9ad91acfbe788ca2f662640645ef2d67b7611c944` |
| `sine44_c.m4a` | `b6535ce7da05ae06ca970c100f6f3683dd5563bbb7987f1989635142a0a64f5f` |
| `sine44_c.mp3` | `81d480cd0702c9562ca42db47385c68a2eb3997fc25c4d569fde767f7420df9a` |
| `sine44_c.ogg` | `a19da5da56c18025107f9ee8a0cb976ca569d6062a9f7208a9fc841ccdfd9948` |
| `sine44_c.opus` | `e6bf4d4aea191884fda3a4a3c7f8ba4ea6a65261ce7b0bb2e4287e79a2bd345d` |
| `sine44_c.wav` | `d984ca369347fe6ec103815cebebb1de38104ce23d8d72dadbf2dea323a3d05b` |
| `sine44_full.flac` | `f6c47cc247845d649c8912e9c647d19d71318ebcdf3fb8234e99f37913fe2f8a` |
| `sine44_full.m4a` | `570d91714446248775d358b44e99f092eab11481c0b6edb5f1434f4d818b1555` |
| `sine44_full.mp3` | `0664d0ef08d91d12f6dfc7292dea55d0ca5209d9206cb7780ea237bf9e0e15f3` |
| `sine44_full.ogg` | `614b4550074934d11143d6e27b0d2fe4ba872ab7f12c1e78c26fafd64d10a8dc` |
| `sine48.flac` | `945111baaf63289fd98924b9dd5977c2c1117bb6566f87fee7524d6f644777e3` |
| `sine48.mp3` | `70673b8c1b63dbae59fa900b43ce2c7ee1d37cc1035471da5f1df9d8ddc8d1f1` |
