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
| FOSCAM R2 V4 | Sends around thirty pictures before its first `IDR`. A decoder rejects every one of them for want of a reference frame, so the first second of a session is a burst of failures that clears itself — see §4 for what that does to libavcodec's output. |
| Reolink doorbell | Does not really serve four concurrent sessions. With four open the stream degrades to a picture every six to eight seconds where it runs at fifteen, and the fourth suffers first. Nothing is refused and nothing is reported — see §8. |

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
binds to (9 ↔ `avcodec-63`), but the crate is not tied to that one release: it
carries version branches from 5.1 up. The same source therefore builds against
vcpkg's 9.0.2 on Windows and against Ubuntu 24.04's 6.1 on Linux. Worth checking
before spending a build of FFmpeg from source to match.

**Building on Linux.** The libraries come from the distribution:
`libavcodec-dev`, `libavformat-dev`, `libavutil-dev`, `libswscale-dev`,
`libswresample-dev`, and `libavfilter-dev` together with `libavdevice-dev` for
the features the crate enables by default. A missing one stops the build naming
the `.pc` file it went looking for, which is the whole diagnosis.

**Building on Android.** Neither FFmpeg nor OpenH264 is needed there, and the
manifests say so: both are declared for targets other than Android, so enabling
the crate's default features pulls neither into the Android dependency graph.
Android decodes through `AMediaCodec`.

**libavcodec logs to `stderr` on its own.** It writes there directly unless it is
given a callback, with no timestamp, no target and no filter that can reach it,
and it reports at error level what a live camera produces routinely. The camera
in §3 is the example: thirty pictures rejected for want of a reference frame,
six lines each, which is nearly two hundred unformatted lines for one camera in
one session. The backend installs `av_log_set_callback` and hands the messages to
`tracing` at debug level instead, which leaves the console the one line that says
what is going on — the pipeline's own warning — and brings the detail back under
`RUST_LOG=xgview=debug`.

That callback is the one place where the platforms differ in a *type* rather than
in a value: a `va_list` is a plain pointer on Windows and a pointer to
`__va_list_tag` on Linux, so its signature is spelled once per platform behind an
alias.

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

---

## 7. Decoding on the GPU

**One shape, several devices.** Asking libavcodec to decode on the GPU is three
steps wherever it runs:

```text
av_hwdevice_ctx_create    ->  hw_device_ctx
get_format callback       ->  the pixel format that device produces
av_hwframe_transfer_data  ->  the picture back in system memory, as NV12
```

Only two values change between devices: the `AVHWDeviceType` and the matching
`AVPixelFormat`. That is the whole argument for keeping the hardware path inside
the FFmpeg backend rather than beside it, one copy per operating system.
`crates/monitor_codec/src/ffmpeg.rs` holds the steps once and a table of devices,
a `Device` each: the type, its format in both vocabularies, and the name it
reports. `prefer_hardware`, the negotiation callback, needs no state - a decoder
only offers the formats of the device it was given, so whichever entry of the
table appears in the list is the one to take.

**The devices, best first.** They are tried in order and the first that opens is
used; a machine with none of them falls through to the CPU.

| Platform | Order | Why |
| --- | --- | --- |
| Linux | `cuda`, `vaapi` | The NVIDIA driver does not implement VAAPI. Reaching it takes a bridging package, so on an NVIDIA machine VAAPI is the second hand path. |
| Windows | `d3d11va`, `cuda` | Direct3D 11 serves every vendor including NVIDIA, drives the same decoding hardware, and asks for nothing beyond the graphics driver. CUDA has nothing to win in front of it. |

A device that turns us down is logged at `debug` and the next is tried; only the
whole table failing is a `warn`. A machine with one device out of two is not a
problem to report - it is the reason for the table.

**Requested is not in use.** Two facts are kept apart, and it is worth keeping
them apart: the `Device` the decoder was handed is what `name()` reports, while
`hardware` is set only when a picture arrives back in that device's pixel format.
Accepting the device is the decoder's decision, and the first picture is where it
shows. The grid shows `hardware`.

**What it is worth, measured.** Four 640x480 sub streams:

| | hardware | software |
| --- | --- | --- |
| Windows, Direct3D 11 | 2-3 ms per picture | 4-5 ms |
| WSL, CUDA | 10-15 ms | 4-19 ms |

The Windows row is the point of the feature: on a native GPU path the decoding
really does leave the CPU. The WSL row is a warning about the environment, not
about the code - see below. The first pictures of a session cost 100-300 ms while
the CUDA context comes up, and FFmpeg reuses the process-wide primary CUDA
context, so a grid of channels shares one rather than creating a context each.

### Hardware decoding under WSL, and what it can and cannot tell you

WSL2 has no `/dev/dri`, so the DRM render node VAAPI needs is simply not there.
`LIBVA_DRIVER_NAME=d3d12 vainfo` prints the libva version and exits: Mesa does
ship `d3d12_drv_video.so`, so the driver is present, but it is the *display* that
cannot be opened, and there is no way round that from inside the distribution.

What WSL2 does have is `/dev/dxg`, and the NVIDIA driver for it brings
`libnvcuvid.so`, `libnvidia-encode.so` and `libcuda.so`. With those on the linker
path and a libavutil built with the CUDA hwcontext, `cuda` opens and decodes for
real. Two checks before believing it: `nvidia-smi -L` answering, and
`strings libavutil.so | grep nvcuda` finding the loader.

`h264_cuvid` living in libavcodec is not the same fact. That is a decoder in its
own right; `cuda` is the hwaccel, and a build can carry either without the other.

**Do not benchmark hardware decoding in WSL.** The GPU is reached through
`/dev/dxg` and a paravirtualised driver stack, so the copy back into system
memory - the step no hardware decoder in this design avoids - costs far more than
it does natively. On a stream small enough that the CPU decoder was never the
bottleneck, the result is a hardware path that measures no faster than the
software one and sometimes slower. Use the environment to prove the path opens,
the fallback is reached when it does not, and the pictures are right. Take the
numbers from the machine the program will actually run on.

---

## 8. What a full grid costs

Sixteen channels — four cameras pulled four times each, 640x480 sub streams,
hardware decoding — measured over a minute of steady state:

| | |
| --- | --- |
| CPU | 0.67 of a core |
| Working set | 725 MB |
| Threads | 391 |
| Per picture | 1-4 ms to decode and hand it over |
| Decode failures, units dropped | none, beyond the session-start burst of §3 |

This side is not what runs out first; the cameras are. The doorbell of §3 does
not really serve four sessions at once, which is why a grid built from a few
cameras repeated tests the cameras rather than the wall. Sixteen *different*
cameras would put the same load here.

**A session can go quiet without going away**, which is the failure the run was
worth having. Timing a session by its packets only catches a stream that stops
entirely; the doorbell kept talking, at one access unit every few seconds, so
every counter looked calm while the tile had stopped moving. The pipeline now
times a session by its *pictures*, and the deadline it allows is learned from the
stream rather than fixed: twice the shortest interval between two of its key
frames, floored at five seconds and capped at fifteen, with a longer floor before
the first picture arrives. Measured afterwards on the same four-by-four: four
verdicts, one per instance of the doorbell, and none on the other twelve
channels.

Learned rather than set, because the only legitimate reason for a long gap
between pictures is a wait for the next key frame, and a group of pictures is one
second on one camera and ten on another. It is the *shortest* interval that is
kept, never the most recent one: an estimate that followed the stream's recent
behaviour would be fed by the very condition the deadline exists to detect, and a
stream that had begun to dribble would widen its own deadline until it never
fired. On the cameras to hand it reads five seconds for three of them and six for
the MCS2020, whose group is three seconds - the camera that a fixed five second
deadline was quietly cutting off whenever it waited for a key frame.
