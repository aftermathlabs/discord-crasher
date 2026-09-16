# Media parser test-case generator

`media-gen` is a small, dependency-light Rust command-line tool for building two
media-parser test cases. It edits existing valid media files in place; it does
not invoke FFmpeg, patch a browser, contact a server, or upload anything.

The WebM case includes a bridge layout for both the older one-buffer-delayed
Vorbis path and the current immediate-discard path. The checked-in short sample
was validated with Discord Desktop 1.0.9257 (Electron 42.11.1 / Chromium
148.0.7778.280) and Chrome 153.0.8010.48. The M4A case remains specific to the
pinned Discord/FFmpeg build. Other releases may reject these files, handle them
safely, or fail differently.

| Case | Input | Effect in the affected build | Trigger |
| --- | --- | --- | --- |
| WebM/Vorbis discard bridge | Existing WebM with an `A_VORBIS` track | Renderer terminates with `0x80000003` (`STATUS_BREAKPOINT`) in Chromium's `AudioDiscardHelper` release check on both tested discard modes | Playback/decode; metadata loading alone is not sufficient |
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

## WebM/Vorbis dual-version discard crash

### What causes it

Matroska negative `DiscardPadding` becomes a Vorbis front skip. Chromium's older
Vorbis path delays that metadata by one encoded packet; current Chromium applies
it to the current decoded output and drops packet 0's metadata when that priming
packet emits no PCM. Repeating one large skip across packets 0 and 1 bridges the
two behaviors:

| Audio packet | Decoded PCM | Front skip |
| ---: | ---: | ---: |
| 0 | none (Vorbis priming) | 577 frames |
| 1 | 576 frames | 577 frames |
| 2 | 1,024 frames | 1 frame |

The track has a 128-frame `CodecDelay`. In the older delayed mode, packet 0's
577-frame skip is applied to packet 1. In the current mode, packet 0's skip is
dropped and packet 1's identical skip is applied directly. Either way, only 448
frames can be removed after the decoder-delay offset, so 129 frames carry into
packet 2. Its positive front skip reaches Chromium's release check after those
129 frames have already been removed. The required invariant is
`discarded_frames <= decoder_delay`; `129 <= 128` fails and terminates the
renderer.

The generator parses the Vorbis setup headers, computes the decoded sizes of
packets 1 and 2, and fails closed unless the bridge is viable. It then:

- converts the first three audio `SimpleBlock` elements to `BlockGroup` elements;
- writes shared negative `DiscardPadding` on packets 0 and 1 and a positive
  one-frame trigger on packet 2;
- preserves the encoded audio and video payloads; and
- replaces stale `SeekHead`, `Cues`, and affected cluster CRC elements so the
  rewritten container remains parseable.

The manifest reports the immediate and delayed paths separately. For the
checked-in sample, both paths name packet 2 as the check packet and report
`expected_carry_frames: 129` and `expected_check_fails: true`.

### Generate a candidate

The repository contains a 43 ms, 4,185-byte control WebM:

```sh
cargo run --release -- webm \
  --input samples/short-vorbis-dual-control.webm \
  --output .build/short-vorbis-dual-crash.webm \
  --manifest .build/short-vorbis-dual-crash.json \
  --force
```

The input must contain:

- an `A_VORBIS` track;
- a positive `CodecDelay` (normally read from the track); and
- at least three audio packets whose Vorbis modes can be decoded;
- packet 1 output larger than the codec delay; and
- packet 2 output larger than the computed carry.

Useful options are `--trigger-skip`, `--codec-delay`, and `--sample-rate`.
`--second-skip` remains an alias for the renamed `--trigger-skip` option. The
defaults are the values needed for the known failure state. The command prints
the JSON report to stdout and writes the same report to `--manifest` when that
option is supplied.

The checked-in candidate is 43 ms and 4,206 bytes. Its SHA-256 is:

```text
a0ab9e146c629f037b86612addc1ab6fff9200711d45aa5f5edbb9576cc206ac
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

The matching artifacts are
[`samples/short-vorbis-dual-control.webm`](samples/short-vorbis-dual-control.webm),
[`samples/short-vorbis-dual-crash.webm`](samples/short-vorbis-dual-crash.webm),
and [`samples/short-vorbis-dual-crash.json`](samples/short-vorbis-dual-crash.json).
