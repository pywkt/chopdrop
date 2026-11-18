# Audio fixtures

The three formats that cannot reasonably be written by hand, as the smallest
files that still exercise their decoders.
WAV and AIFF are not here: those are built in code inside the tests, where
the expected samples are arithmetic rather than recorded.

`tiny_mono.flac` — 400 frames, mono, 44100 Hz, 16-bit. Samples are exactly
`(n - 200) * 80` for `n` in `0..400`, so the FLAC test — lossless — asserts
every value.

`tiny_mono.mp3` — 0.1 s (4410 frames), mono, 44100 Hz: a half-scale 440 Hz
sine. The test asserts rate, channels, rough length, peak and RMS.

`tiny_stereo.ogg` — 0.1 s (4410 frames), stereo, 44100 Hz, Vorbis: the same
sine on the left and its inverse on the right, so a swapped or merged channel
fails the test.

They are consumed by the unit tests in `chopdrop/src/sample.rs` through
`include_bytes!`, so there is nothing to find at runtime and no path to get
wrong.

## Regenerating them

A one-off, with the container's `uvx`. Re-run only if a fixture has to
change — the FLAC test recomputes the pattern below, so it will tell you if
the file and the expectation drift apart.

```sh
uvx --with soundfile --with numpy python3 -I - chopdrop/tests/fixtures <<'PY'
import math
import sys

import numpy as np
import soundfile as sf

out = sys.argv[1]

flac = np.array([(n - 200) * 80 for n in range(400)], dtype=np.int16)
sf.write(f"{out}/tiny_mono.flac", flac, 44100, subtype="PCM_16", format="FLAC")

n = 4410  # 0.1 s; shorter lossy files decode badly
sine = np.sin(2 * math.pi * 440 * np.arange(n) / 44100.0) * 0.5

sf.write(f"{out}/tiny_mono.mp3", sine.astype(np.float32), 44100, format="MP3")

stereo = np.stack([sine, -sine], axis=1)
sf.write(f"{out}/tiny_stereo.ogg", stereo.astype(np.float32), 44100,
         format="OGG", subtype="VORBIS")
PY
```

## The v1 project

`v1_project.json` — a project as chopdrop v1 saved it (snapshot version 1):
two lanes, a one-shot kick with an embedded 6-frame mono 16-bit WAV and a
path, and a gate open hat with no sample, both in choke group 1, with the
"bitstep Sample pad" switch on. It carries none of the v2 lane fields. The
unit test in `chopdrop/src/state.rs` (through `include_str!`) loads it and
pins that every lane keeps v1's behaviour: no cutting itself, a 300 ms
release, Out 1, time off. Written once; never regenerate or edit it — it
stands for projects users already have.
