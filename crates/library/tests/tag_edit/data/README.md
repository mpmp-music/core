All three fixtures use a generated 440 Hz sine wave at 16 kHz.

- `tone.aac` uses FFmpeg's AAC encoder with a 24 kb/s target and a 0.2 s input.
- `tone-faststart.m4a` remuxes `tone.aac` with `-c:a copy -movflags faststart`.
- `tone-speex.ogg` uses libspeex's wideband encoder on 3,200 generated mono samples.
  It has separate identification, comment and audio pages.
- `tone-faststart-co64.m4a` widens the fast-start fixture's `stco` entries to
  `co64` and adjusts the box sizes and chunk offsets by the resulting displacement.
