# Discord Crasher

`media-gen` is a small, dependency-light Rust command-line tool for building two
media-parser test cases observed in Discord Desktop 1.0.9257 (Electron 42.11.1,
Chromium 148.0.7778.280). It edits existing valid media files in place; it does
not invoke FFmpeg, patch Discord, contact a server, or upload anything.

The cases are version-specific. They reproduce the behavior of the pinned
Discord/Chromium/FFmpeg build used during the investigation; a different browser
or Discord release may reject the files, handle them safely, or fail differently.

| Case | Input | Effect in the affected build | Trigger |
| --- | --- | --- | --- |
| WebM/Vorbis delayed discard | Existing WebM with an `A_VORBIS` track | Renderer terminates with `0x80000003` (`STATUS_BREAKPOINT`) in Chromium's `AudioDiscardHelper` release check | Playback/decode; the ordinary attachment view does not autoplay the original WebM |
| M4A constant `stsz` count | Existing fast-start AAC/M4A seed | Renderer transiently allocates about 6.6 GB (6.16 GiB) of private memory while loading metadata | Metadata load after the audio element changes from `preload="none"` to metadata |

These are denial-of-service/resource-consumption test cases, not demonstrated
code-execution exploits. Run them only in an isolated, bounded test environment.

## BLARE2 Binary Instrumentation

`media-gen` is the reproducibility layer, not how these cases were discovered.
The difficult part was finding and proving the behavior inside a large native
Discord executable and its bundled media libraries. blare2 made that practical
by rewriting an isolated copy of the exact runtime and allowing narrow probes to
observe execution without changing the installed application.

For the WebM case, blare2's semantic probe stopped at the exact
`AudioDiscardHelper::ProcessBuffers` check and recorded the failing state:
`discarded_frames = 129` and `decoder_delay = 128`. Without that observation, a
renderer exit with `STATUS_BREAKPOINT` would only identify a generic release
assertion; it would not establish which media invariant failed or whether the
crafted padding actually reached it.

For the M4A case, the large allocations are transient and can disappear before a
normal process sample is taken. blare2's exact-runtime coverage and targeted
instrumentation, combined with high-frequency process sampling and disassembly,
showed that the tiny file reached the MOV sample-table builder and that the
declared count scaled `AVIndexEntry` and timing-table allocations. This separated
the issue from an ordinary AAC decode failure or a misleading file-size/OOM
correlation. Broad function-entry coverage alone was not enough: the low- and
high-count files followed the same functions, while their allocation sizes were
radically different.

blare2 did not replace container analysis, source review, or controls, and it did
not prove that Discord's production upload/CDN path preserves these bytes. Its
value was runtime attribution: it turned suspicious parser behavior into a
reproducible, version-pinned finding with an exact failing state and a defensible
allocation explanation. Without binary instrumentation of this kind, finding
these two cases would have been substantially slower and much harder to validate.

## Build

From the repository root:

```sh
cargo build --release -p media-gen
```

The binary is `target/release/media-gen` (`media-gen.exe` on Windows). The
generator itself only needs Rust and the dependencies in `Cargo.toml`.

```sh
cargo run --release -p media-gen -- --help
cargo run --release -p media-gen -- webm --help
cargo run --release -p media-gen -- m4a --help
```

## WebM/Vorbis delayed-discard crash

### What causes it

The affected Chromium branch carries Matroska `DiscardPadding` metadata between
Vorbis decode buffers. A Vorbis track with a positive `CodecDelay` can make the
carried front-discard count exceed the decoder delay. The release build then
executes the `CHECK_LE` in `AudioDiscardHelper::ProcessBuffers` and terminates
the renderer.

The canonical minimal state is:

1. `CodecDelay = 128` frames.
2. The first crafted audio packet has no decoded PCM, so its front skip is carried.
3. The next packet supplies 576 frames and receives a 577-frame front skip. After
   the 128-frame decoder delay is removed, 129 frames remain carried forward.
4. The following packet applies that carry and reaches the delayed one-frame skip.
   Chromium checks `discarded_frames <= decoder_delay`, i.e. `129 <= 128`, and
   the check fails.

The exact packet sizes depend on the source file. The generator parses the Vorbis
setup headers, computes decoded packet sizes, and chooses the latest safe
three-packet window. It then:

- converts the two selected `SimpleBlock` elements to `BlockGroup` elements;
- adds negative `DiscardPadding` values to the first two target packets;
- preserves the encoded audio and video payloads; and
- replaces stale `SeekHead`, `Cues`, and affected cluster CRC elements so the
  rewritten container remains parseable.

For the checked-in five-second tail seed, the generated report contains
`expected_carry_frames: 257` and `expected_check_fails: true`; its packet skips
are `1153` and `1` frames. The 7,202-byte minimal reference uses `577` and `1`.

### Generate a candidate

The repository contains a suitable control WebM:

```sh
cargo run --release -p media-gen -- webm \
  --input tests/discord-still-audio-tail-control.webm \
  --output .build/media-gen/webm-candidate.webm \
  --manifest .build/media-gen/webm-candidate.json \
  --force
```

The input must contain:

- an `A_VORBIS` track;
- a positive `CodecDelay` (normally read from the track); and
- at least three audio packets near the end whose Vorbis modes can be decoded.

Useful options are `--second-skip`, `--codec-delay`, and `--sample-rate`. The
defaults are the values needed for the known failure state. The command prints
the JSON report to stdout and writes the same report to `--manifest` when that
option is supplied.

With the checked-in control input, the known candidate is 20,018 bytes and has
SHA-256:

```text
c0ea55978a998b405837e4482bb8f786f909647af1bfd993b792c18bcb0a741b
```

The output hash is expected to change if the input, packet window, or options
change.

## M4A constant-sample-count allocation

### What causes it

The M4A case abuses a valid MP4 sample-table shape rather than the encoded AAC
payload. The generator changes a seed as follows:

1. It replaces the explicit `stsz` sample-size array with `sample_size = 1`.
2. It sets the declared sample count in `stsz`, the first `stsc` run, and the
   first `stts` run to the same large value.
3. It removes the old explicit size entries and repairs ancestor box sizes.
4. It repairs the absolute `stco` offset so the one-byte AAC payload still points
   inside `mdat`.

The pinned FFmpeg demuxer treats the declared count as authoritative while
building its index and timing tables. The relevant structures use approximately
24 bytes per declared sample for `AVIndexEntry` and 12 bytes per sample for
timing data: 36 bytes per count entry in total. The maximum accepted count in
the tested build is `178,956,969` (`0x0AAAAAA9`), which projects to:

```text
178,956,969 * 24 = 4,294,967,256 bytes
178,956,969 * 12 = 2,147,483,628 bytes
combined         = 6,442,450,884 bytes
```

The exact Discord renderer reached a 6,614,761,472-byte private-memory peak
while loading metadata. The adjacent count `178,956,970` is rejected by the
pinned FFmpeg boundary and stayed near normal memory. This is uncontrolled
resource consumption (CWE-400), not an observed integer-wrap, negative-size
allocation, or out-of-bounds write. The actual AAC payload can be truncated;
successful playback is not required to reach the large allocation.

### Generate a candidate

The generator intentionally accepts a seed instead of embedding an AAC encoder.
Use a fast-start AAC/M4A file with one explicit `stsz` table, one `stsc` run,
one or more `stts` entries, and `stco` chunk offsets. It fails closed if the
required structure is absent.

```sh
cargo run --release -p media-gen -- m4a \
  --input path/to/base-faststart.m4a \
  --output .build/media-gen/constant-stsz.m4a \
  --manifest .build/media-gen/constant-stsz.json \
  --force
```

The default `--sample-count` is `178956969`, the maximum accepted value in the
pinned build. Use a smaller value for a low-memory smoke test, for example:

```sh
cargo run --release -p media-gen -- m4a \
  --input path/to/base-faststart.m4a \
  --output .build/media-gen/constant-stsz-32000000.m4a \
  --sample-count 32000000 \
  --force
```

`--sample-count 178956970` is a useful adjacent rejection control, not the
triggering value. The optional `--moov-at-end` flag moves `moov` after `mdat`
and updates `stco`/`co64`; it is useful when testing a file whose physical AAC
bytes occur before its metadata, but it is not required for the allocation
mechanism.

To produce that layout explicitly:

```sh
cargo run --release -p media-gen -- m4a \
  --input path/to/base-faststart.m4a \
  --output .build/media-gen/constant-stsz-moov-end.m4a \
  --moov-at-end \
  --force
```

With the reference 2,254-byte seed used in the investigation (seed SHA-256
`57dd8e1d90de7c8983d09e733c24d7a940498679ba761a2c15abd9273ee0e3ee`), the
default candidate is 2,198 bytes with SHA-256:

```text
203f03eb5333c29b5937df220e32e15235bbf6f48c5407ed85e3d4c4ca6329f4
```

Hashes differ for different AAC seeds.

## Bundled M4A sample

The crate includes the reference candidate and its generation report:

- [`samples/constant-stsz-178956969.m4a`](samples/constant-stsz-178956969.m4a)
- [`samples/constant-stsz-178956969.json`](samples/constant-stsz-178956969.json)

The following opt-in player is embedded directly in this README. The bug is
reached during metadata parsing, so playback is not required:

![](samples/short-video-vorbis-crash.webm)

> **Warning:** this deliberately requests a multi-gigabyte native allocation in
> the affected build. Use only with a disposable, memory-limited browser profile.
> GitHub may sanitize or decline to render this M4A player; a normal link still
> provides the sample for local testing.
