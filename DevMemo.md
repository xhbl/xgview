# DevMemo

Development notes: camera quirks, decoding pitfalls, and the diagnostics that
found them. Everything here was observed on real hardware and the numbers are
measurements, not estimates.

---

## 1. An access unit can arrive inside a single FU-A sequence

*FOSCAM MCS2020, both `/videoSub` and `/videoMain` — 2026-09-30*

**Symptom.** The channel showed a slideshow: the picture advanced roughly once
every two seconds and the burned in timecode jumped. The stream window read
`units=1-2  dropped≈30` per two seconds while `fps` sat at a healthy 25.

**Ruled out, and how:**

| Question | Answer |
| --- | --- |
| Are packets being lost? | No. The RTP sequence numbers ran consecutively over 250 packets, zero gaps and zero missing. |
| Is the camera serving us a degraded stream? | No. `ffmpeg` against the same URL pulls the same order of bitrate (2.3 Mbps) and decodes 143 frames in 5.7 s. |
| Is it the session setup? | No. Matching the `PLAY`/`Content-Base` URI, setting up the audio track, the user agent, the preemptive `Authorization` header and RTCP receiver reports each changed nothing. |

**The cause.** The camera packs a *whole access unit* into one FU-A sequence.
The fragment header names only the leading NAL, an `SEI`; the slices follow
inside the same payload, behind Annex-B start codes:

```
[0x06 SEI (47 bytes)] [00 00 01][slice] [00 00 01][slice]
```

Reassembling one such unit and dumping it made the shape plain:

```
unit: size=11428  firstType=6  startCodes=2  types=[1,1]  firstStartCodeAt=48
```

`H264Depacketizer::append_nal` inspected only the first byte, so every picture
looked like a bare parameter set: `frame_has_slice` stayed false and `finish`
threw the picture away. Key frames survived because their blob happens to open
with an IDR.

`ffmpeg` is unaffected because it runs the H.264 **parser** over the
depacketized buffer, and the parser splits it on those start codes.

**The rule.** A payload is not necessarily one NAL unit. Split it on start codes
and classify every unit it carries. Slicing on start codes is safe inside a
coded picture: H.264 inserts emulation prevention bytes precisely so the
sequence cannot occur in slice data.

Implemented by `split_nal_units` + `H264Depacketizer::classify` in
`crates/monitor_core/src/h264.rs`, locked by the regression test
`picks_the_slices_out_of_an_access_unit_packed_into_one_fragment_sequence`.

**After the fix**, on the same hardware:

| | before | after |
| --- | --- | --- |
| sub stream | `units=0-1  dropped≈30` | `units=30  dropped=0` |
| main stream | `units=1-2  dropped≈50` | `units=50  dropped=0` |

`units` now tracks `fps` exactly on every channel.

---

## 2. Diagnostics that paid off

**Never conclude "the camera is at fault" from our own counters alone.** The
episode above cost a long detour because a slice-less stream *looked* like a
property of the camera. Run a reference client against the same URL and compare
before blaming the device.

Three checks settled it, in this order:

1. **RTP sequence numbers.** Consecutive numbers mean nothing was lost in
   transit and the sender really is not producing the packets. Gaps mean the
   client is dropping them. This one fact splits the problem in half.
2. **A reference client.**
   `ffmpeg -rtsp_transport tcp -i <url> -t 10 -c copy -f h264 dump.h264`, then
   tally the NAL types in `dump.h264`. If the reference decodes pictures and we
   do not, the difference is in our code or our session — not the camera.
3. **The raw bytes.** Dumping the first few RTP payloads and reading them in hex
   beats any amount of theorising. A repeated four byte prefix that first looked
   like corruption turned out to be nothing more than a static scene.

`ffprobe -hide_banner -rtsp_transport tcp -i <url>` is worth running first: it
prints the codec, profile, pixel format and resolution the decoder will see.

Keep a per session trace of the first N packets behind a counter while chasing
something like this, then delete it. The traces in `pipeline.rs` that name the
interleaved channel, the NAL type, the marker bit and the RTP timestamp found
this bug; they are noise once the session is understood.

---

## 3. Camera quirks seen so far

| Camera | Quirk |
| --- | --- |
| FOSCAM MCS2020, C2 | `Content-Base` names a *different port* than the one asked for (`…:65534/videoSub/` against `:88`). `PLAY` has to target the announced aggregate URL, as RFC 2326 §10.6 requires. |
| FOSCAM MCS2020 | The SDP carries no `a=fmtp` / `sprop-parameter-sets`, so the parameter sets have to be cached from the stream and injected by hand. |
| FOSCAM MCS2020 | An `SEI` precedes every picture, and one FU-A sequence carries the whole access unit — see §1. |
| Reolink doorbell | Requires HTTP Digest and answers `401` to the first `DESCRIBE`. |
| Several | Set the RTP marker bit on the packets carrying parameter sets rather than on the last slice, so pictures also have to be closed by a change of RTP timestamp. |

---

## 4. The FFmpeg decoder backend

**Building.** `ffmpeg-next` binds through `ffmpeg-sys-next`, which runs
`bindgen` at build time. That needs two things:

- the FFmpeg development libraries, found through `VCPKG_ROOT`
  (`vcpkg install ffmpeg:x64-windows`);
- **libclang**, for bindgen — `winget install LLVM.LLVM`, then
  `setx LIBCLANG_PATH "C:\Program Files\LLVM\bin"`.

Visual Studio's `VC\Tools\Llvm` may hold only `clang-format.exe` and
`clang-tidy.exe`; the component that brings `libclang.dll` is *C++ Clang
Compiler for Windows*.

`ffmpeg-sys-next` has its own vcpkg code path, so `VCPKG_ROOT` is enough and
`FFMPEG_DIR` is not needed.

**Runtime.** The sys crate copies the DLLs into its own build directory, not
next to the executable, so the FFmpeg `bin` directory has to be on `PATH` — or
the DLLs copied beside the shipped binary.

**Low latency.** The wrapper's defaults hold pictures back for frame threading
and for reordering. Open the decoder with `AV_CODEC_FLAG_LOW_DELAY`,
`thread_count = 1` and `thread_type = 0`, otherwise a live view carries a delay
of several frames.

**Version pairing.** `ffmpeg-next`'s major version tracks the FFmpeg release it
binds to (9 ↔ `avcodec-63`). A mismatch shows up as a wall of missing symbols at
link time.

---

## 5. H.264 parameter sets and decoder state

**A picture must carry both halves of the pair.** A decoder rejects an access
unit that holds only an SPS or only a PPS. Cache the sets keyed by *kind and
id*, newest copy wins, and inject the cached pair in front of every picture that
does not carry both halves itself.

Keeping every distinct set keyed by content instead lets two SPS with the same
id into the cache. The decoder then ends the access unit it is reading and
starts a new sequence, which costs far more than it saves.

**Software decoders latch on error.** Cisco OpenH264 opened with
`ERROR_CON_DISABLE` raises its internal "parameter sets lost" flag after any
decode failure, and from then on discards *every* picture without an IDR until
the next key frame — and each dropped picture raises the flag again, so it never
recovers. The symptom is a stream that only ever shows key frames while the
console fills with `Native:16` (`dsNoParamSets`) and `Native:18`. Passing
`ERROR_CON_SLICE_COPY_CROSS_IDR_FREEZE_RES_CHANGE` to the raw API fixes it, and
the concealed pictures then have to be taken straight from the raw decoder
because the wrapper discards frames reported with an error state.

**An access unit that holds no slice is not a picture.** Cameras send SPS and
PPS as marker terminated units of their own; handing those to the decoder earns
a `dsNoParamSets` error. Filter them out, but remember the sets first.

---

## 6. Putting a picture on screen: NV12 planes and egui's texture convention

**Both backends hand over NV12.** OpenH264 is reordered from I420 and FFmpeg
scales to `Pixel::NV12`, so the frame crossing into the UI carries luma and one
interleaved chroma plane. Uploading those two planes costs 1.5 bytes per pixel
against 4 for RGBA, and the colour matrix moves into a fragment shader, where it
used to be spent on the CPU once per pixel per channel per frame.

**wgpu demands 256 byte rows.** `bytes_per_row` of a `write_texture` must be a
multiple of 256, and a 1920 wide luma row is not. Both planes are copied into a
padded scratch buffer first. They can share one row length: a chroma pair covers
two luma samples, so a row of interleaved U and V bytes is as long as a luma row.

**egui takes a sampled texture for a gamma value.** Its shader says so itself:

```wgsl
// We expect "normal" textures that are NOT sRGB-aware.
let tex_gamma = textureSample(r_tex_color, r_tex_sampler, in.tex_coord);
```

That one comment is the whole rule. The texture handed to
`register_native_texture` must **not** use an sRGB format, and the shader must
write the BT.601 result out as it is — no `srgb_to_linear`, no other gamma
compensation. An sRGB format makes the hardware decode on sample, on top of the
decode egui's own shader does for a linear framebuffer, and the picture lands one
gamma step too dark.

The mistake is worth recording because the wrong reasoning is convincing: an
sRGB render target *does* encode what is written to it, so compensating for that
looks correct in isolation. What it misses is that egui decodes again on the way
in, and the two do not cancel unless the format is non-sRGB. The symptom was
unmistakable the moment the two windows were put side by side — mid grey came out
at 55 instead of 128 — and neither the counters (`units == decoded`, `errors=0`)
nor the wgpu validation layer said a word.

`crates/monitor_gui/src/video.rs` holds all of it.
