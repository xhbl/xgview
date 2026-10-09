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

**Check what your own log forwarding drops.** Every libavcodec message goes
through one callback, and mapping its `AV_LOG_ERROR` down to `debug` is enough
to hide the only line that names a hardware-decoder refusal: a `WARN | ERROR`
grep comes back empty, and the search moves on to the wrong suspect. Ours did
exactly that for a whole afternoon.

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
| FOSCAM MCS2021, and one unbranded `IP-Camera` | Declare the Baseline profile *without* `constraint_set1_flag`, a declaration the Direct3D 11 decoder has no mode for. Both streams of both cameras decode on the CPU until the flag is set for them — see §7. |

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

**Packaging for Linux.** A distribution's FFmpeg is linked against the whole
world - x264, x265, dav1d, rav1e, libplacebo, librsvg - so `ldd` on a binary
built against it names close to two hundred libraries, none of which a machine
that never installed them can load, and some of which (`libGL`, `libvulkan`,
`libX11`) must not travel in a package at all. `scripts/build-linux.sh` builds
its own trimmed FFmpeg instead - the decoders the pipeline asks for, and no
third party dependency - and stages it with the executable, which finds the
seven shared libraries beside it through an `$ORIGIN` rpath:
`target/xgview-<version>-linux-x86_64.tar.gz` unpacks and runs. The FFmpeg
release is pinned by `FFMPEG_VERSION` (8.0.3, so `avcodec-62`; Windows' vcpkg
builds `avcodec-63` - the crate carries a branch for each). The trim costs two
things, both visible in the script: the x86 SIMD paths need `nasm` on the build
machine, and hardware decoding needs `--enable-vaapi` and its `libva` headers -
without either, the package decodes on the CPU alone.

The glibc floor is the build machine's, which is what `--docker` is for: a
release is built by running the same script inside a `rockylinux:8` container,
whose 2.28 lets the package run from Ubuntu 20.04 / Debian 10 / RHEL 8 on,
where a build on an Ubuntu 24.04 host asks for 2.39. The container's toolchain
is baked into an image (`scripts/Dockerfile.linux-build`) and its working tree
is kept in a docker volume, so only the first run pays for either. Every build
ends by printing the floor it measured.

The package also carries its icons and an `install.sh`. Running it puts the
bundle in `~/.local/opt/xgview` (or `/opt/xgview` with `sudo ./install.sh
--system`), the icons into the hicolor theme and a desktop entry into the
application grid, with a symlink on `PATH` beside them; `--uninstall` takes the
same four things back out and leaves the configuration in `~/.config/xgview`
alone. The folder stays whole because the `$ORIGIN` rpath is what finds the
libraries - there is no `bin/` and `lib/` split to install into, which is why
`/opt` is the shape it fits.

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

### A stream the hardware decoder will not take

Nine channels on an N5105: seven decoded on the GPU and two on the CPU, with
nothing about the two pictures to say why. The trail, because it is the one to
walk again:

1. **Not a device limit.** Nine `av_hwdevice_ctx_create` calls, nine successes;
   `no device for the hardware decoder` never logged, every decoder configured.
   Whatever was happening was inside libavcodec, not at our `open_device`.
2. **Not concurrency.** One of the two channels run alone in an otherwise empty
   grid still came up on the CPU, and so did both its streams. A limit on
   concurrent sessions would have cleared.
3. **Not the fields a decoder screens.** Read out of the sequence parameter
   set: profile, level, chroma, bit depth, frame_num period, POC type,
   reference count, VUI. A working camera matched a failing one on all of them
   but `pic_order_cnt_type` - and a second working camera shared that too.
4. **The bytes.** The two sets differed in their third byte, the constraint
   flags, and that was the whole of it.

`ff_h264_get_profile` (`libavcodec/h264_parse.c`) reads `profile_idc` 66 as
`CONSTRAINED_BASELINE` **only when `constraint_set1_flag` is set**; without it
the stream is plain `BASELINE`. And the Direct3D 11 / DXVA mode table
(`libavcodec/dxva2.c`, `prof_h264_high[]`) matches three profiles and no more:
`CONSTRAINED_BASELINE`, `MAIN`, `HIGH`. So a camera declaring plain Baseline
finds no mode at all, `dxva_get_decoder_guid` fails, the hwaccel initialisation
fails, and libavcodec falls back to the software decoder - reporting it at
*error* level as `Failed setup for format d3d11: hwaccel initialisation returned
error`, which our log forwarding was dropping to `debug` (see §2).

**What we do about it.** `monitor_codec::sps::constrain_baseline` sets that one
bit in the sequence parameter sets handed to the decoder, decided once per
session from the first parameter set the stream carries and applied to every
access unit after it. The flag declares that the stream obeys the constraints of
the Main profile - no arbitrary slice order, no flexible macroblock ordering, no
redundant slices - which is true of every stream here and is what the camera
should have said itself. The pictures are untouched. A stream that did use those
features could not be hardware decoded whatever the flag says, which is why they
are not in the mode list to begin with; and the healthy majority of channels is
never touched at all. The change is announced at `info`, because decoding from
bytes the camera did not send is not something to do quietly.

**Two facts about where the failure lands.** The profile comes from the
*stream*, not from the configuration, and `ff_get_format` runs after the
sequence parameter set is parsed (`h264_slice.c`) - so nothing the SDP says, a
size or a profile or a hint, has any bearing on whether the hardware path is
taken. When a channel decodes on the CPU for no visible reason, run with
`RUST_LOG=xgview::pipeline=debug,xgview::codec=debug` and read the `stream
parameter set` line first.

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

---

## 9. Android: the `AMediaCodec` path, and a vendor abort that blocks it

Measured on a SHIELD Android TV (Android 11), against the same eight cameras.
Everything below was found by running on the device; the Android target had
never been built before, so the build chain came first.

**The target had never been built.** Two blockers, both silent until someone
tried:

- eframe's `accesskit` feature refuses to build with `android-native-activity`,
  which is the backend the manifest's `NativeActivity` needs (`compile_error!`
  in eframe's `lib.rs`). eframe's default features are therefore not taken and
  are split per target: `wgpu` + `default_fonts` everywhere, `accesskit` +
  `glow` + `wayland` + `x11` on the desktop, `android-native-activity` on
  Android.
- `scripts/build-android.sh` passes the API level as `-p`, which cargo-ndk read
  as "package" once it reached 4.x, where the platform is `-P`.

**A codec built per packet never reaches steady state.** The backend created an
`AMediaCodec`, configured it, started it, ran one access unit and deleted it
again - every frame. The session is opened once now, from the first access unit,
and closed when the channel ends.

**`AImageReader_getWindow` does not return the window.** The NDK signature is
`(AImageReader*, /*out*/ ANativeWindow**)`. Declaring it as returning
`ANativeWindow*` and calling it with one argument writes the window through the
second register's garbage address: a SIGSEGV on one run, a null window on the
next. Every FFI signature was then checked against the NDK headers rather than
recalled.

**The usage decides whether the planes can be read at all.** A reader built with
`GPU_SAMPLED_IMAGE | CPU_READ_OFTEN` hands out buffers owned by the GPU;
`AImage_getPlaneData` fails with `-30003 AMEDIA_IMGREADER_CANNOT_LOCK_IMAGE`,
and the channel shows nothing while `units` climbs and `errors` stays at zero.
With `CPU_READ_OFTEN` alone the planes are readable. The decoder takes the
surface either way.

**The reader has to be the stream's own size.** `AMediaCodec` renders the
picture at its **native size whatever the format's `width`/`height` say** - it
does not scale to the surface - and an image reader of any other size produces
`-30003` on every picture. The SDP is not a source for that size: `a=framesize`
is optional, and for two of these cameras its `sprop-parameter-sets` describe a
size the stream does not send. The size is taken from the stream's own sequence
parameter set instead (`monitor_codec::sps`, read on the first access unit that
carries one, which is why the codec is opened lazily). *Known gap:* the
depacketizer prepends the sets it has cached from the SDP to a picture that
arrives without its own, so those two cameras still get the SDP's size; sizing
from the codec's reported output and reopening would close it.

**The vendor abort.** As soon as a decoder renders into an image reader while
the process also drives the wgpu surface, the process aborts within seconds:

```text
Abort message: 'fdsan: attempted to close file descriptor N, expected to be
                unowned, actually owned by unique_fd 0x...'
#01 libc.so (android_fdsan_close_with_tag)
#02 libc.so (close)
#03 /vendor/lib64/libnvrm_sync.so (NvRmSyncFdLegacyClose)
```

Three ruled out, each by experiment rather than by reading:

| Not the cause | How it was ruled out |
| --- | --- |
| The image reader's configuration | Five variants - CPU-only, GPU-only, both, `AImageReader_new`, `acquireNextImage` - all abort identically |
| Vulkan on its own | Pinning wgpu to the GL backend fails to start at all: `WGPU error: Parent device is lost` |
| `MediaCodec` into an image reader | A control app running the same path with **no GPU surface** in the process decodes and hands images over normally |

What is left is the combination: this process's GPU surface and MediaCodec's
surface rendering, meeting in NVIDIA's sync fence handling. It is the driver
closing a descriptor the framework still owns.

**The NDK leaves no way around it, and no zero copy.** `AMediaCodec_getOutputImage`
does not exist in the C API - it is Java only - so a hardware buffer can only be
reached through an image reader, which is the path that aborts. Byte-buffer mode
(`AMediaCodec_getOutputBuffer`) is the only surface-free way to get pictures out
of the codec, and it costs the copy a zero copy design exists to avoid. On this
device the choice is: the surface path with zero copy and an abort, or byte
buffers and a copy.

**Rust's logging is invisible on Android unless it is carried to logcat.** A
`NativeActivity` throws stdout away, so every `tracing` line the viewer emits
was lost, and a start that failed looked exactly like a start that never
happened. `monitor_gui::run_android` installs a subscriber that writes to
logcat (`adb logcat -s xgview`), and `monitor_android` no longer drops
`run_android`'s error: the GL attempt above failed silently for exactly that
reason before the logger existed.

**A codec that will not take `low-latency` does not ignore it, it fails the whole
configuration.** Measured on a Huawei GRL-AL10 (Kirin 970), against cameras the
SHIELD decodes without complaint: every channel sat at `decoded=0 opening=N` with

```text
no picture yet, waiting for the first key frame index=0
  err=decoder configuration failed: AMediaCodec_configure failed with status -10000
```

`-10000` is `AMEDIA_ERROR_UNKNOWN`, what the NDK reports for *any* refusal, so it
names nothing - the failure had to be read out of the framework instead, with
`log.tag.MediaCodec` and `log.tag.ACodec` set to `VERBOSE`:

```text
E OMXNodeInstance: setConfig(...:hisi.decoder.avc, ??(0x6f800006)) ERROR: Undefined(0x80001001)
E ACodec  : decoder can not set low-latency to 1 (err -2147483648)
E ACodec  : [OMX.hisi.video.decoder.avc] configureCodec returning error -2147483648
```

`ACodec` treats the option as part of the configuration rather than as a hint it
may drop, and HiSilicon's decoder answers `OMX_ErrorUndefined` to it - so asking
for low latency costs the whole session. The public NDK cannot be asked whether a
codec supports it (`AMediaCodecInfo` is not in it; checked against NDK r27c's
`media/` headers), so `monitor_codec::amediacodec` asks first and, when the
configuration comes back refused, re-opens the session without the option - one
extra failed configure per session on the devices that will not take it, logged as
"the decoder refused low latency; opening without it". The SHIELD, whose decoder
accepts it, keeps it: nine sessions opened with `low_latency=true` and no fallback.

*Note for the next time a codec configure fails:* the verbose framework tags flood
the logcat ring buffer within seconds, and the ordinary `tracing` lines that carry
each channel's state are rolled out first. Set the tags back to `INFO` when done.

---

## 10. What the copies between the decoder and the GPU cost

Measured on the desktop (Windows, Direct3D 11 decoding), eight cameras in a
three-by-three grid, per picture, steady state:

| Stream | `copy_back` | `convert` | `pack` | upload (CPU side) |
| --- | --- | --- | --- | --- |
| 640x480 sub | ~0.6 ms | 0 | ~0.05 ms | ~0.25 ms |
| 2560x1920 main | ~4.8 ms | 0 | ~1.4 ms | ~0.25 ms |

The numbers come from the code itself, every two seconds at debug level:
`xgview::codec` ("where a picture's cost goes") for the first three,
`xgview::video` ("where an upload's cost goes") for the last. The decoder's
`copy_back` is the `av_hwframe_transfer_data` of §7, `convert` is libswscale,
`pack` is the row walk into the padded plane layout of §6, and the upload is the
CPU side of `write_texture` - the transfer itself happens after the call
returns.

**`convert` is always zero on the hardware path.** Every device hands its
picture back as NV12 already, so libswscale never runs; the step is there for
the software path and for a device that answers in another format. Worth knowing
before blaming the converter for anything.

**`copy_back` is the only step that costs**, and it scales with the picture:
0.6 ms at 640x480, 4.8 ms at 2560x1920. At 20 fps that is about 1.2% of a core
per 640x480 channel, and about 10% per 2560x1920 one. `pack` is 0.05 ms at
640x480 and 1.4 ms only at 2560x1920, where the same row walk crosses six and a
half times as many bytes. The upload's CPU side is a quarter of a millisecond
either way.

**The measurement is what rules the two copy-removing designs out**, rather than
an opinion about them:

- Writing the transfer straight into the padded layout, and pooling the planes
  so the per-picture allocation and its zero fill go away, would remove `pack`:
  0.05 ms at the sizes that matter. The copy it removes is not the cost.
- True zero copy - decoding into the very texture the shader samples - is what
  would remove the 0.6-4.8 ms, and it is not reachable from here:
  - `wgpu::Device::create_texture_from_hal` is public, but the `wgpu_hal::dx12`
    `Texture` it takes has private fields and no constructor from a raw
    resource, and wgpu's Direct3D 12 backend contains no `OpenSharedHandle` at
    all. wgpu cannot be handed an outside texture without patching it.
  - the decoder's textures come from FFmpeg's own Direct3D 11 device while wgpu
    runs on Direct3D 12, and FFmpeg neither creates its pool with the shared
    flags cross-device sharing needs nor accepts a texture we made.
  - the two devices would have to synchronise through a fence wgpu offers no way
    to wait on.

  That is a patched `wgpu-hal`, a change to FFmpeg's hardware path and a
  hand-rolled cross-device fence, against about a tenth of a core per 2560x1920
  channel - and the software path, which is what a machine without a device
  falls back to, would not gain from it at all.

## 11. MJPEG streams the NAS resets, and the delay that hid it

On the Android box over Wi-Fi the two Synology MJPEG substreams (cameras 13 and
14, `multipart/x-mixed-replace` over `http://192.168.17.88:5000`) reconnect
every few seconds, while the desktop - same eight cameras, same configuration,
wired - never does. The report was "the picture is unstable", and the first
thing to fix was that the log did not say why: `reqwest` names only the stage
an HTTP request failed at, so the session failures read `http error: error
decoding response body`. It keeps the reason in the source chain, and walking
that is what turned the stage into the answer:

```text
error decoding response body
  <- error reading a body from connection
    <- Connection reset by peer (os error 104)
```

The peer resets the connection, sometimes before the response starts
(`error sending request ... <- connection error <- Connection reset by peer`).
The NAS's nginx answers the MJPEG request with `transfer-encoding: chunked`,
`connection: keep-alive` and `keep-alive: timeout=20`, and the sessions end
after 7-55 s, so the timeout in that header is not what ends them.

**It is not the client.** Each of these was measured, not assumed:

| Ruled out | How |
| --- | --- |
| Our MJPEG client | The same `MjpegClient` held one session for 120 s on the desktop, and 100 s in a standalone process on the same device (a temporary `examples/mjpeg_soak.rs`, since deleted, which also reported the longest gap between pictures) |
| A server-side session lifetime or an expired `StmKey` | `nc` pulling both URLs from the device ran 90 s in one session, twice, and two concurrent pulls ran 90 s |
| A reader too slow for the server | On the failing sessions: `longest gap 1.22s, quiet for 0.00s before the reset` - the reset arrives immediately after a picture, so the peer kills a live stream, not a stalled one |
| Worker starvation in the runtime | Two MJPEG channels with four CPU-burning loops on the device: 357 windows, zero failures |
| Duplicate sessions for one channel | `/proc/net/tcp` during a failing run shows exactly one connection per channel to port 5000, plus the `TIME_WAIT` of the previous process |

**What does reproduce it** is the app streaming RTSP from `192.168.27.x` at the
same time. The device is on Wi-Fi at `192.168.17.104` (the NAS's own subnet) and
reaches those cameras through the gateway `192.168.17.1`:

| Channels running | Result |
| --- | --- |
| MJPEG only | stable |
| MJPEG + two RTSP from `192.168.17.88:8554` | stable |
| MJPEG + two RTSP from `192.168.27.x` | reset every 7-38 s |
| Two RTSP from `192.168.27.x`, MJPEG pulled by a separate process | that process is reset too; the same pulls with `nc` are not |

So the trigger is the Wi-Fi path carrying routed camera traffic while the app
holds HTTP streams to the NAS, and only `reqwest`-shaped connections feel it.
The mechanism - which of the AP, the router or the NAS decides to reset - was
never pinned down, and three things make it stop: a wired connection (the
desktop, and the box with a cable: 150 s, zero failures), moving the two
cameras to the NAS's own RTSP (`rtsp://192.168.17.88:554/Sms=13.unicast`), or
living with it. It is an environment problem, not one the client can fix, and
it is not specific to Android: what is specific is that the box is the only
device on Wi-Fi.

**The reconnection delay made it worse than it had to be.** `run_channel` had
one `attempt` counter for both jobs - the retry budget and the index into the
backoff - and it was only ever raised, for the life of the channel:

```rust
loop {
    attempt += 1;                       // and never reset
    ...
    let delay = policy.delay_for(attempt);
```

With the default policy (1 s initial, ×1.8, 30 s ceiling) a stream that is cut
every ten seconds walks 1 s, 1.8 s, 3.2 s, 5.8 s, 10.5 s, 18.9 s, 30 s - so
after a minute the picture freezes for up to half a minute between two frames,
and never returns to the short delay. That is the opposite of what the delay is
for: it exists to space out attempts on a stream that will not come up, and
these sessions had just been running.

A session that published at least one picture now resets the counter, which is
the distinction that matters - a time-based rule cannot tell a stream that ran
and dropped from one that connected and never sent anything, because both can
last longer than any threshold. `FrameStore` therefore counts the pictures each
channel has published (`published`), since the sequence number a frame carries
restarts with every session and cannot answer "did this one deliver". A session
that delivered nothing keeps backing off exactly as before, so a camera that is
down is still left alone.

After the change, over Wi-Fi with the same eight cameras: every failure logged
`attempt=1`, and the gaps between failures match a delay of about one second on
top of the session's own lifetime (`16:57:10.891 → 16:57:18.600` is 7.7 s = a
6.76 s session + ~0.9 s). The tiles flicker instead of freezing.

## 12. An Android 5.1 box: installable, but not drawable

The question was whether the Mi Box 3 Pro (Android TV 5.1, MediaTek MT8693)
could run the viewer. Measured first, because most of the answer is in the
device rather than in the code:

| | |
| --- | --- |
| `ro.build.version.sdk` | 22 (Android 5.1) |
| `ro.product.cpu.abi` | `arm64-v8a` - the artifact's ABI, no second build needed |
| Memory / display | 1.9 GB / 1920x1080 |
| `dumpsys SurfaceFlinger` | `PowerVR Rogue GX6250, OpenGL ES 3.1 build 1.4@3443629` |
| `ro.opengles.version` | `0x30000` - the legacy property lags the driver |
| `libvulkan.so` | absent from `/system/lib{,64}` and `/vendor/lib{,64}` |

`readelf -d` on `libmonitor_android.so` shows only `liblog`, `libandroid`,
`libdl`, `libmediandk`, `libm` and `libc` as `NEEDED`, all of which API 21
ships, and no reference to `AImageReader` or any other API 24 symbol. Lowering
`minSdk` to 22 is therefore enough to install it, and the activity does start:
`winit: App Resumed - is running`. It then aborts:

```text
panicked: called glObjectLabel but it was not loaded
  location=glow-0.16.0/src/gl46.rs:4515:5
panicked: panic in a function that cannot unwind
```

With no Vulkan, wgpu takes its GL backend, which calls `glObjectLabel` on every
resource that carries a label - `glObjectLabel` is core in GLES 3.2 and comes
with `GL_KHR_debug` in 3.1, and the driver offers neither. wgpu asks EGL for a
3.0 context by default (`gles_context_attributes.push(3)`, with
`Gles3MinorVersion::Automatic`); asking for 3.1 was tried and returned the same
panic (`WGPU_GLES_MINOR_VERSION=1`, honoured through
`BackendOptions::from_env_or_default`), so the entry point is missing from the
driver's contexts outright.

**So the limit is the driver's GLES feature set, not the Android version.** An
upgrade to Android 6 changes neither half of the problem: API 23 is still below
the floor the APK installs at, and the feature set travels with the firmware's
GPU driver, so whether a newer one offers `GL_KHR_debug` is a coin flip that
can only be settled by upgrading and measuring again. Making this box work
would mean a second renderer (`egui_glow` plus a GL path for the video
textures), which is the thing the device is not worth, so the floor was set at
28 rather than letting a device install an app that aborts on its first frame
(see section 18 for why it later moved to 27).

**The panic hook earned its place here.** Without it the only trace was
`SIGABRT` with a backtrace ending inside our own `.so`: a Rust panic cannot
unwind out of `android_main`, an `extern "C"` entry point, and its message goes
to stderr, which a `NativeActivity` throws away. `install_panic_hook` sends the
message and the source location to logcat, and is what produced the three lines
above.

## 13. Two bugs behind "the first and the eighth tile stay black"

On a Galaxy S22+ (Android 16, Qualcomm) two of eight channels never showed a
picture, and their tiles restarted the session every ~21 s. Both causes were
ours, and neither was where the symptom pointed.

**A read cancelled part way through a packet loses the framing.** `run_session`
bounded its wait for media with `tokio::time::timeout(wait, client.read_media())`,
and `wait` collapses towards zero as a session goes without a picture. A timeout
that fires while `read_exact` is half way through an interleaved payload takes the
bytes it has already consumed with it: the stream is then read at the wrong
offset, the next `next_packet` finds a byte that is not `$`, tries to parse the
payload as a status line and reports

```text
rtsp error: read status line: stream did not contain valid UTF-8
```

which is why the session restarted at ~21 s - the pre-first-picture stall
deadline - for as long as the channel had no decoder to show a picture. The
fix separates waiting from reading: `RtspClient::wait_for_media` peeks
(`fill_buf`, and the RTP socket's `readable` on the datagram transport), which is
cancel-safe, and only then is a packet read, with no timeout around it. Nothing
is ever half consumed, so the wait can be cut wherever it lands. (The session
read stays unbounded on purpose — a deadline that fires mid packet is the bug
this section is about; what is bounded is the handshake before it, where the
client can be dropped whole. See §16.)

**A codec configured without parameter sets is refused.** The other half was
that the two streams carried no `sprop-parameter-sets` in their `SDP`, and the
decoder was configured from the session rather than from the stream:

| | |
| --- | --- |
| the two that failed | `session negotiated ... sets=[]` |
| the ones that worked | `session negotiated ... sets=[(7, 11), (8, 4)]` |
| the refusal | `AMediaCodec_configure failed with status -10000` |

With the format reduced to a mime, `low-latency` and `color-format` - no size,
no `csd-0`/`csd-1` - Qualcomm refuses to configure at all rather than reading
either from the bitstream; NVIDIA answers the same format fine, which is why the
same two streams are healthy on the Shield TV and were never the camera's fault.
The codec is now opened from the first access unit instead, which is what the
pipeline has prepended the cached sets to: `csd-0` and `csd-1` come from the
NAL units it carries, and the size from `sps::picture_size`, so the geometry
reported for the channel is the stream's own rather than a guess.

**The measurement that mattered was the reduced one.** The full grid looked like
a device refusing eight hardware decoders - two of eight channels failing is
exactly what a concurrent-instance limit looks like, and that was written down
here as the cause before it was checked. Re-running with *only* those two
channels enabled failed identically, in a process with no other decoder in it,
which is what ruled the limit out and pointed at the configuration instead. A
symptom that scales with the grid is not evidence that the grid is the cause;
narrow the input until it stops.

After both fixes, on the phone over Wi-Fi: 150 s with no session failure (eight
before), and with all eight cameras the grid shows eight pictures - `cannot
configure` none, `session failed` none, every channel decoding 27-49 pictures
per two second window, plus the two MJPEG substreams.

## 14. What egui remembers under a widget's id

Two bugs on a SHIELD TV, both from the same property: the state egui keeps per
widget is keyed by the widget's `Id`, and an `Id` here is a *position in the Ui
tree*, not an identity. A form that is drawn again for other data - the camera
edit dialog, once per camera - gives the same positions to different values.

**The soft keyboard came up on a button.** It was raised by asking "is the
focused widget a text field?" of the state egui exposes for exactly that: a
`TextEditState` under the focused id means the widget is one -
`TextEdit::load_state(ctx, focused).is_some()`, and a text field is the only
widget that keeps one. But `TextEditState::store` uses
`Data::insert_persisted`, so that state outlives the field for the whole
process. Once a dialog with fields had been opened, the first camera row's
transport button - an ordinary `small_button`, drawn at a position that answer
had been stored for - said yes, and the remote raised the keyboard over the wall.

It is asked of the navigation layer now, which is the application's own
registry: a control is a field when it registered as `Kind::Text`. `Kind` is
already the thing the arrow walk reads to know which keys a control keeps for
itself, so the two agree by construction, and nothing else in the application
can answer "yes" by accident. A `DragValue` has to count as well: egui hands its
keyboard-edit mode a `TextEdit` (`widgets/drag_value.rs`), and on a device with
no keyboard of its own that is where a number is typed.

**The caret started in the middle of the next camera's name.** Same state, other
field of it: `TextEditState` carries the cursor. egui places a caret only when
the state has none - `default_cursor_range = CCursorRange::one(galley.end())`
in `widgets/text_edit/builder.rs` - so the *first* dialog opened in a process
put the caret after the last character, and every one after it inherited the
character index the previous camera's editing had left behind: 2, in the middle
of a longer name.

The dialog drops the field's state as it appears, before the first frame the
field is drawn with the focus. The request is made a frame earlier than that
(the hand-off asks for the focus after the window has been drawn), so egui reads
a clean state and applies its own default, with no frame in which the caret is
seen to jump.

The lesson both bugs share: anything egui keys on a widget's `Id` - text edit
state, the cursor, a grid's column widths, a scroll area's offset - is shared
between every use of that position. Reusing a form for other data reuses all of
it, and `insert_persisted` means it is never cleaned up on its own.

---

## 15. OSD font sizing: DPI-independent and tile-proportional

The on-screen display burned into each tile corner has to look the same on a
1080p Android TV with a large system scale and a 1080p desktop with none, and it
has to scale with the tile it sits on rather than with the screen. egui's font
unit is a logical point, and the renderer multiplies it by `pixels_per_point`
(`ppp`) to reach physical pixels, so a fixed logical size drifts with the system
DPI setting - exactly the inconsistency that was showing up.

**The rule.** Do the whole calculation in physical pixels, then divide by `ppp`
to hand egui logical points. The rendered physical size is then independent of
`ppp`:

```rust
fn of(body: Rect, ppp: f32, screen_min: f32) -> Self {
    let min = body.width().min(body.height()) * ppp;
    let f = (screen_min / 1080.0).max(1.0);
    Self {
        font: (min * 0.045).clamp(7.0 * f, 28.0 * f) / ppp,
        inset: (min * 0.0125).clamp(1.0 * f, 10.0 * f) / ppp,
    }
}
```

Three layers, each in `crates/monitor_gui/src/grid.rs`:

1. **Base size** (`OsdMetrics::of`). `min` is the tile's physical short side
   (`body` is logical, so multiply by `ppp`). The proportional part `min * 0.045`
   is already DPI-independent on its own - `min_logical * ppp = min_physical` -
   and only the `clamp` would have reintroduced the dependency, had its bounds
   stayed in logical points. Putting the bounds in physical pixels and dividing
   by `ppp` afterwards keeps the clamped value DPI-independent too.
2. **Type scale** (`osd`). `NAME_SCALE = 1.2` for channel names, `MEASURE_SCALE
   = 0.8` for bitrate / fps / format.
3. **Final size** (`osd_corner`). `size = m.font * scale`, handed to
   `FontId::monospace`; line height is `size * 1.25`.

**The screen factor `f`.** A tile-proportional size alone leaves 2K and 4K
looking small: a 28 px ceiling is 28 physical pixels whatever the screen, and on
a denser 4K panel that is visually smaller. `f = (screen_min / 1080).max(1.0)`
scales the clamp bounds linearly above 1080p and leaves smaller screens alone -
no fixed multiplier, no step. The bounds move with it, so the whole OSD grows on
a larger screen rather than only the tiles that hit the ceiling:

| Screen | `f` | font clamp (physical px) | 1×1 tile | 2×2 tile | 4×4 tile |
| --- | --- | --- | --- | --- | --- |
| 1080p | 1.00 | 7 - 28 | 28 | 24 | 12 |
| 1440p | 1.33 | 9.3 - 37.3 | 37.3 | 37.3 | 16.2 |
| 2160p (4K) | 2.00 | 14 - 56 | 56 | 56 | 24.3 |

**Why the bounds are 7 and 28.** The ceiling sets the 1×1 tile size and the
gradient above it; the floor keeps a small tile legible. The values were walked
to, not chosen:

| Ceiling tried | Result |
| --- | --- |
| 12 (logical points, the original) | 1×1 through 4×4 all hit the ceiling - no visible gradient |
| 12 (physical, DPI-independent) | 1080p right, 2K / 4K too small |
| 64.8 (a 1440p tile reference) | far too large |
| 32 | slightly large |
| 28 | right on 1080p; with the screen factor, right on 2K / 4K too |

Implemented in `OsdMetrics::of` and `osd` in `crates/monitor_gui/src/grid.rs`.

---

## 16. Establishing a session: the part that had no deadline

**The gap hid under the two failures that always worked.** Only the TCP `connect`
was bounded (10 s). A refused connection and an unreachable host were therefore
always handled — `ECONNREFUSED` at once, the 10 s connect timeout when the SYN is
dropped — and that is probably why nothing looked wrong for as long as it did.
The third case was left open: the peer accepts the connection and then stops
answering. `OPTIONS` / `DESCRIBE` / `SETUP` / `PLAY` each write a request and wait
for the answer, and every one of those waits was unbounded. A camera at its limit
of concurrent sessions answers the connection and stays quiet; a middlebox can
swallow the flow the same way. MJPEG had the identical hole: `connect_timeout(10 s)`
covered the TCP connect, and the response headers had no deadline at all.

The stall detector of §8 does not cover any of this. It lives inside the packet
loop, which is reached only once `PLAY` has returned, so a channel that never got
that far sat on `Connecting` for good — no `retry in N ms`, no `Stalled`, no
`session failed` in the log — and `run_channel` never got the chance to retry,
because its backoff sits after the call that would not return.

**One deadline around the whole exchange.** `SessionSettings` carries it
(`handshake_timeout_ms` in the panel, default 15 s, beside the retry timings) and
bounds the four requests *together* rather than each of them. When it fires the
client is dropped rather than patched, which is what makes an overall deadline
safe here where a deadline mid packet is not (§13). The budget is generous in the
only unit that matters, round trips: four of them at 300 ms RTT is 1.2 s, so
latency alone almost never needs it raised. What needs it is loss — one TCP
retransmission timeout is a second or more — or a camera that is slow to answer
at all, which is why it is configurable rather than fixed.

**The deadline names the request it was waiting on.** `RtspStage` is stamped
before each step of `start_with_transport`, so the failure reads

```text
the session handshake timed out after 15 s during SETUP
```

and the `warn` line carries `stage = Some(Setup)`. That is not decoration: the
first half of the handshake and the second half mean different things, and only
one of them is worth answering with the other transport. It is also the one part
of this that can be tested cheaply — a listener that accepts and never answers,
given an explicit 100 ms budget rather than 15 s, because the budget is a
parameter.

**A silence is an answer too, once it comes late enough.** The fallback to TCP
existed for exactly one case: a `461` to `SETUP`, which go2rtc answers on purpose
so that a client asking for UDP first falls back. A UDP handshake that goes quiet
from `SETUP` onwards is the same kind of answer, and on a routed or NATed path it
usually means the datagrams are being dropped while the TCP connection the
handshake is running over is plainly working. It now falls back as well. A
silence at `OPTIONS` or `DESCRIBE` does not: that is the control connection
itself, and the other transport would only repeat it at the price of a second
deadline. The cost is real and paid where it is incurred — a genuinely dead UDP
camera now spends two deadlines per attempt before the backoff starts, where it
used to spend one.

**The settings are read, not remembered.** The reconnect timings used to be
written to the config and never looked at again: each channel kept the copy its
supervisor was spawned with, so the sliders did nothing until a restart. The
policy and the handshake budget now live in one shared `SessionSettings`, read
afresh at the top of every attempt — a channel in backoff takes the new timing at
its next retry, a channel stuck handshaking takes the new budget at its next
attempt, and neither is reopened for it. The decoder preference is deliberately
not treated this way: a decoder is chosen when a session opens (§7), so that one
still reopens the channels. The panel note that claimed a change "applies to the
connections opened afterwards" was wrong the moment this landed, and has been
reworded.

**The receiver report carries the numbers it always had names for.**
`packets_lost` and `jitter` went out as `0, 0` whatever the link was doing, which
tells a camera that adapts to receiver reports that everything is fine exactly
when it is not. They are now the cumulative count of the gaps the sequence
numbers showed — zero on TCP, which delivers in order — and RFC 3550's
interarrival jitter, `J += (|D| - J) / 16`, taken between consecutive packets so
that the RTP timestamp's 13 hour wrap is not read as one enormous jitter, scaled
by the clock rate the SDP announced (90000 when it says nothing). Two caveats
worth keeping: the lost count is the gaps *observed*, not RFC 3550's
`expected - received`, so duplicates and reordering make it read a little high;
and the arrival times are taken in the async loop rather than at the socket, so
our own scheduling delay is inside the jitter — a busy sixteen-channel wall
reports more jitter than the wire carries.

**Still unbounded, on purpose.** Two things this did not change. A reordered UDP
packet is still written off as lost rather than held for its neighbours: a
reorder buffer would buy back the occasional whole picture at the price of
adding latency to every picture, and that trade was not taken. And the write
path has no deadline — a send buffer that fills blocks the pump — which is left
as the back-pressure it is.

---

## 17. H.265: the decoder half is already there, the ingest half is not

*Nothing in this section is implemented, and nothing was measured: no H.265
camera has been in front of the viewer. It is what a reading of the code says
the work would be, written down while the question was open so that the next
person does not have to walk it again.*

**The decoder half is already in place**, which is not what was expected. `Codec`
has carried `H265` for a while — `from_encoding` answers both `H265` and `HEVC`,
`mime()` answers `video/hevc` — and the FFmpeg backend already maps it:

```rust
let id = match config.codec {
    Codec::H264 => Id::H264,
    Codec::H265 => Id::HEVC,
```

So `create_decoder(Codec::H265)` already returns the FFmpeg decoder on the
desktop and `AMediaCodec` on Android. The hardware path of §7 needs no change
either: its `Device` table is keyed on the device and its pixel format, not on
the codec, and libavcodec picks the HEVC mode of DXVA / VAAPI / CUDA itself.
FFmpeg's software H.265 decoder is in the build, so hardware-refused-software-
retried works as it does for H.264. NV12, the texture path of §6, the stall
detector of §8 and everything above the decoder are untouched.

**What is missing is entirely on the ingest side.** Five pieces, largest first:

1. **An RFC 7798 depacketizer.** `H264Depacketizer` is the only one and
   `run_session` sends everything that is not MJPEG through it, so an H.265
   stream is misread rather than refused. HEVC over RTP is not H.264's shape: a
   two byte NAL header with the type in `(byte0 >> 1) & 0x3f`, VPS / SPS / PPS
   as three parameter sets (32 / 33 / 34) rather than two, an aggregation packet
   (48) whose NAL units carry a two byte length prefix, and key frames found by
   IRAP type 16-23 rather than by one IDR type. The bones of `h264.rs` carry
   over — splitting on start codes, closing a picture on a timestamp change or a
   marker, caching parameter sets by (type, id), `invalidate_current` on a
   sequence gap — so it is a sibling module, not a new design.
2. **An H.265 SPS parser, for the size.** `monitor_codec::sps` is H.264
   throughout: `picture_size`, `sps_info` and `needs_constrained_baseline` all
   index the H.264 layout. The desktop can do without it, because FFmpeg reads
   the size from the bitstream; Android cannot, and §9 is why — a reader whose
   size is not the stream's refuses every picture with `-30003`.
3. **Three CSD buffers on Android.** The mime is already right, but H.265 wants
   `csd-0` = VPS, `csd-1` = SPS, `csd-2` = PPS where `amediacodec.rs` writes
   `csd-0` = SPS and `csd-1` = PPS.
4. **The SDP.** `sprop-parameter-sets` is one comma separated field; H.265
   advertises `sprop-vps`, `sprop-sps` and `sprop-pps` separately.
5. **The guards.** `video_decoder` refuses everything but H.264 and MJPEG, and
   the H.264-only calls in `run_session` (`access_unit_sps`,
   `needs_constrained_baseline`, `constrain_baseline`) have to be skipped for
   H.265. They are harmless as they stand — none of them finds an H.264 NAL type
   inside an H.265 access unit, so they quietly do nothing.

**Two ways the picture would still be wrong once it decodes.**

- **The colour matrix.** The fragment shader of §6 writes BT.601 and nothing
  else, while H.265 streams are normally tagged BT.709, often in the limited
  range. The picture would arrive with the colours a step off. `ColorSpace.matrix`
  is already read from the stream — it is the shader that ignores it — so this
  is a uniform and a second matrix in the WGSL.
- **Ten bits.** `Main 10` is a different problem: the whole path is NV12, eight
  bits per sample, one layout. A ten bit stream would fail at decode or at the
  copy back, not at the depacketizer.

**What happens today if one is plugged in.** Nothing breaks, which is worth
knowing before buying one: the channel connects, negotiates, keeps reporting its
statistics and shows a placeholder, with a single `no decoder for this codec`
warning to say why. That is deliberate — `video_decoder` documents it as what an
`H265` camera *should* look like, rather than a channel that never stops
reconnecting.

**How the work would be cut.** The first step needs neither the SPS parser nor
Android: the depacketizer plus the guards, with FFmpeg left to learn the size
from the stream, is enough to put a picture on a desktop. The second is Android
and the polish — SPS parser, CSD layout, `sprop-*`, colour matrix.

**For whoever buys the cameras.** `Main` (eight bit) rather than `Main 10`, and
a model that still offers an H.264 profile on another stream, so that the camera
is usable before any of this lands.

## 18. The floor moves to 27: a Mi Pad 4 that could not be installed

A Mi Pad 4 (Snapdragon 660, Adreno 512) refused the APK with the installer's
generic "there was a problem parsing the package". The package itself was
sound: the cause was in `AndroidManifest.xml`, whose `minSdkVersion` was 28
while the tablet runs MIUI 10.3.2 on **Android 8.1, which is API 27**. The
PackageManager rejects a manifest asking for a newer platform than the device
offers (`INSTALL_FAILED_OLDER_SDK`), and the installer reports that as the same
parse failure it shows for a truncated download, which is what makes this worth
writing down. The ABI was never in question: the tablet is `arm64-v8a`, the one
the build produces.

The floor of 28 came from §12, and that reasoning does not cover this device. It
was about a driver - the MT8693's PowerVR has no Vulkan and no `GL_KHR_debug`,
so wgpu's GL backend finds no `glObjectLabel` and the process aborts on its
first frame. An Adreno 512 has Vulkan, so wgpu should take its Vulkan backend
and never reach that GL path at all. That is the assumption to confirm on the
device; if it does not hold, §12's panic hook is what will name the failure in
`adb logcat -s xgview` rather than leaving a `SIGABRT` with no message.

Nothing else in the tree needs API 28, which is why lowering the floor is the
whole fix:

- The native library's `NEEDED` list stays what §12 measured - `liblog`,
  `libandroid`, `libdl`, `libmediandk`, `libm`, `libc`, all API 21.
- The Android decoder asks `AMediaCodec` and `AMediaFormat` only (API 21), and
  reads pictures out in byte-buffer mode rather than through an `AImageReader`
  (API 24); see §9 for why that path was chosen.
- The configuration export already branches below Android 10: the
  `MediaStore` route is `Q` and up, and the plain write below it needs
  `WRITE_EXTERNAL_STORAGE`, which the manifest declares with
  `android:maxSdkVersion="28"` - so API 27 still holds the permission that
  route depends on.

`minSdk` is 27 now, and both build scripts were moved with it - the API level
the library is compiled against should not sit above the one the manifest
installs on (`-Api` in `scripts/build-android.ps1`, `API` in
`scripts/build-android.sh`).

## 19. Vulkan 1.0, caught: a version gate, and a restart that has to terminate

*OPPO PBAM00 (Snapdragon 450, Adreno 506), Android 8.1 / API 27 — 2026-10-07*

§12 left this device family on a marker: a launch that loses its Vulkan device
writes a file, the next launch reads it and takes the GL backend. It works, but
the first launch always dies - and it dies the loudest way there is, `SIGABRT`
with a backtrace ending inside our own `.so`. The work here is making that first
launch survivable, on the same package, without giving up the marker.

**The device, measured.** `ro.build.version.sdk` is 27, and `pm list features`
carries `android.hardware.vulkan.version=4194307` - `VK_MAKE_VERSION(1, 0, 3)`.
So wgpu does take its Vulkan backend (an Adreno 506 has one), enumerates an
adapter, is handed a device, and loses that device while the first
`create_render_pipeline` is built.

**Which of the two endings it is decides everything.** The logcat line

```text
E xgview: panicked wgpu error: Validation Error
  In Device::create_render_pipeline, label = 'egui_pipeline'
    Parent device is lost
```

is emitted by our own panic hook, so the path is a Rust panic, not a driver
calling `abort()` - a distinction that cannot be made from the `SIGABRT` alone,
because both end in one. It also is not a caller's `unwrap`: wgpu 25's
`create_render_pipeline` is infallible to its caller and the panic comes from
wgpu's own uncaptured-error handler. That matters because a panic is catchable
and an `abort()` is not.

**The catch has to sit inside `run_android`, not around `android_main`.**
`android-activity` 0.6.1 already wraps its call to `android_main` in a
`catch_unwind` (`native_activity/glue.rs`, `rust_glue_entry`). It cannot help:
our entry point is `extern "C"`, a nounwind boundary, so Rust aborts *inside* it
before the panic ever reaches the glue. That is the `panic in a function that
cannot unwind` §12 recorded, and the `SIGABRT` on `name: android_main`. A
`catch_unwind` around `eframe::run_native` is inside that boundary, which is what
makes it the first place the panic can actually be intercepted.

**The fallback is a new process, because a retry is not available.** winit
allows one event loop per process on Android - `EventLoop can't be recreated`,
which this project has hit before - so a caught panic cannot be answered by
re-running the loop on the GL backend. The activity schedules
`AlarmManager` + `PendingIntent` before the process goes away instead; the alarm
is held by `system_server`, so it outlives the exit and brings the wall back
without a remote. Measured: process gone at `19:59:53.192`, the next one up at
`19:59:58.520`. Five seconds, and the 700 ms alarm is not what costs - cold
starting the 13 MB `.so` is.

**The gate is the main path; the catch is the net under it.** A driver's
behaviour cannot be asked about in advance, but its version can:
`PackageManager.hasSystemFeature(FEATURE_VULKAN_HARDWARE_VERSION, 0x401000)` is
"Vulkan 1.1 or better", and this device reports `4194307`, so it answers
`false`. Being sent to GL up front is worth more than recovering well, so the
gate is asked first and the catch exists for the drivers it cannot name. The
gate lives in Java because `PackageManager` is a Java API - and the handoff to
the native side is a marker file written from `onCreate` **before**
`super.onCreate`, because `android-activity` starts the native thread inside
`super.onCreate` and anything written after that races it. The ordering, not the
file, is the reason the split is where it is. The backend itself is asked for in
egui-wgpu's setup - `instance_descriptor.backends` - and not through the
`WGPU_BACKEND` variable the first version set: that meant `std::env::set_var` in
a process which, by then, already had threads, which the standard library does
not promise is safe. Overriding the one field keeps the rest of egui-wgpu's
descriptor, including the `WGPU_GLES_MINOR_VERSION` knob inside
`BackendOptions::from_env_or_default`.

**Two shapes of the state machine were wrong, and neither was visible from a
single run.** Both were found by walking the states before trusting them:

- `force-vulkan` - the switch that overrides the gate so the fallback can be
  exercised on the very device the gate keeps off Vulkan - first overrode the
  attempt marker as well. The restart it triggered then forced Vulkan again, so
  the device restarted in a loop. The switch overrides the *gate* only; a
  previous abort always wins.
- a launch that comes back on GL and panics there would restart forever, and a
  device that can draw on neither backend (§12's PowerVR: GLES 3.1 without
  `GL_KHR_debug`) would never come to rest. A restart only happens where there
  is a backend left to come back on, which means only out of a Vulkan launch; a
  GL panic is reported and left down, as it was before any of this.

Both were cells of one table, so the table is where it now lives:
`monitor_gui::backend` holds the decision, out of `run_android`, and its tests
read every combination of the three markers on the host - including the
composition of the two tables, which asserts the invariant neither wrong shape
respected. The old code was Android-only, so the only way to check a cell was to
flash a device and try it.

**A panic after the renderer is up is not a launch that could not draw.** The
`catch_unwind` covers the whole life of the process, not just its start, which
gave the state machine a third way to be wrong: a panic on the main thread hours
into a run would write the attempt marker and pin a device that had been drawing
perfectly to GL for good. One `AtomicBool`, set at the top of the app-creator
closure - which eframe calls after `Painter::set_window` built the very pipeline
these drivers fail, and before the first frame (`native/wgpu_integration.rs`,
`set_window` then `app_creator`) - tells the two apart. Before it: the marker and
the GL fallback. After it: a restart, with the markers left alone, because the
backend has plainly worked. The restart is capped as well, three within ten
minutes, counted in shared preferences so the count outlives what it counts; past
that the process is left down rather than fed back into a deterministic panic.
The way out is `_exit` and not `std::process::exit`: the libc exit path runs
`atexit` handlers and flushes, through a driver that has already failed once.

**The attempt marker stopped being permanent.** It is a claim about the
environment that wrote it, so `MainActivity` now drops it when the app version or
`Build.FINGERPRINT` has changed - a fixed app or a fixed driver is exactly what
would make it a lie, and without this one bad launch kept a device off Vulkan for
ever. The gate was already self-clearing for the same reason; this is that idea
applied to the other half.

**What was measured, and one thing that was not.** On the device: the gate sends
the first launch to GL with no panic at all; `force-vulkan` produces the full
sequence - panic caught, payload logged, no `SIGABRT`
(`logcat -d | grep -E 'Fatal signal|Abort message'` is empty), and the wall back
on GL by itself. Not measured: the GL-panics-therefore-no-restart branch, which the
OPPO cannot exercise because its GL driver works and there is no switch to make
it fail. The marker written *before* Vulkan is tried is what covers even that
case - a panic that cannot be caught (one raised while already unwinding, say)
still leaves the mark, so the launch after it lands on GL rather than repeating.

## 20. go2rtc frames the video on another interleaved channel

*Two live channels, the second one a go2rtc re-publish of the first camera —
2026-10-09*

A wall with two live RTSP channels: the first is a camera pulled directly
(`rtsp://192.168.27.40:88/videoSub`), the second the same camera re-published by
go2rtc (`rtsp://192.168.17.88:8554/cam_front_door_2`). The first tile streams;
the second sits on `connecting` for good. The same URL plays in VLC and
`ffplay`, so the stream and the credentials are not in question.

**The client assumed channel 0 is the video.** `RtspClient` framed the video
track with `SETUP ... interleaved=0-1` and then treated *every* interleaved
channel except 0 as RTCP:

```rust
let (channel, payload) = self.read_interleaved().await?;
MediaPacket::Rtp  if channel == 0
MediaPacket::Rtcp otherwise
```

That holds for the camera - asked for `0-1`, it answers `0-1` and sends the
video there. It does not hold for go2rtc. RFC 2326 lets the server choose its
own channels, and go2rtc numbers them by **track index**: its SDP lists the
audio track first (`a=control:trackID=0`, PCMU) and the video second (`trackID=1`,
H264), so the video is framed on `2-3` whatever the client asked for. The client
read channel 2, called it RTCP, and threw the video away - no picture ever
reached the decoder, so the tile never left `connecting` (the stall detector
eventually recycled it, but nothing was ever going to decode).

**Measured with the handshake dumped by hand**, first against go2rtc:

```text
track[0] media=audio enc=PCMU  pt=96 control=trackID=0
track[1] media=video enc=H264  pt=97 control=trackID=1
SETUP video control="trackID=1" interleaved=0-1
12s window: rtp_packets=0 rtcp_packets=782   rtp_bytes=0
```

and, with the raw channels printed, the 782 "RTCP" packets were all on channel
**2**, payload type 97 - plainly the video, discarded. The camera, by contrast,
answered `0-1` and put its video (`pt=96`) on channel 0, which is why it had
always worked.

**The pair the server answers with is the client's.** `SETUP` now reads the
`interleaved=RTP-RTCP` parameter out of the answer's `Transport` header, keeps
it on the client as `(rtp, rtcp)`, and frames from there: `read_media` compares
against the negotiated RTP channel, and a receiver report goes out on the
negotiated RTCP channel instead of a hardcoded channel 1. When the answer names
no pair, the requested `0-1` stands, so nothing changes for a camera that
answers the way it was asked. After the change, the same go2rtc URL:

```text
SETUP video control="trackID=1" interleaved=0-1
SETUP -> channel 2
12s window: rtp_packets=766 rtcp_packets=0 rtp_bytes=964949 first_pt=Some(97)
```

The regression test (`rtsp::tests::reads_the_video_from_the_channel_the_server_confirmed`)
runs an SDP with audio ahead of video and a server that answers `interleaved=2-3`
whatever was requested, then asserts the video RTP framed on channel 2 is
delivered as `MediaPacket::Rtp`.

**What generalises.** A device list ordered audio-first is the tell: a server
that numbers channels by track order will hand the video a pair other than the
requested one exactly when a track precedes it. Honouring the answer rather than
the request is the fix; the request was only ever a suggestion.

## 21. A camera that names no sub stream still has one

*A 192.168.27.40-49 block of FOSCAMs and one Reolink — 2026-10-09*

Adding the block by scan, one camera came up with a real sub stream address
while the rest only had an inferred one. The guess was right - FOSCAM's
`/videoMain` -> `/videoSub` is in the inference table - but the question was why
one camera could be *discovered* and the others could not.

**Both kinds answer ONVIF; the difference is the profile names.** Probing the
block: `.40`-`.48` are FOSCAM (ONVIF on `:888`, HTTP Basic), `.49` a Reolink
(ONVIF on `:80`/`:8000`, HTTP Digest). All of them resolve a main stream fine.
They part company on `GetProfiles`:

```text
FOSCAM MCS2020 / C2   prof0 prof0_name 1920x1080  looks_like_sub=false
                      prof1 prof1_name  640x480  looks_like_sub=false
Reolink doorbell      000 Profile000_MainStream 2560x1920  looks_like_sub=false
                      001 Profile001_SubStream   640x480  looks_like_sub=true
```

`looks_like_sub` is a name match - `sub`, `second`, `low`, `small`, `stream2`,
`_2`. Reolink labels its second profile `…SubStream`, so it is recognised and
`GetStreamUri` returns the real `Preview_01_sub`. FOSCAM names its profiles
`prof0` / `prof1`, which carries no such hint, so no profile looked like a sub
stream, `sub_uri` stayed `None`, and `import_device` fell back to
`apply_sub_inference` - a URL guess that happens to be right here and would not
be for a camera whose scheme is not in the table.

**The fallback: the smaller profile is the sub.** `select_streams` now picks the
main as the largest profile and the sub as the smallest profile that names
itself like one; **when none does, the smallest genuinely smaller profile is
taken instead**, so the URI is fetched rather than guessed. It has to be
*smaller*, which keeps a device with several equally sized profiles from having
an arbitrary one labelled as its sub stream, and a single-profile device still
falls back to the URL inference as before. After the change the FOSCAMs answer
with a real sub stream:

```text
main_profile=Some("prof0") main_uri=Some("rtsp://192.168.27.40:88/videoMain")
sub_profile=Some("prof1")  sub_uri=Some("rtsp://192.168.27.40:88/videoSub")
```

The choice is a pure function of the profile list (`discovery::onvif::select_streams`),
so the cases are unit-tested without a device -
`falls_back_to_the_smaller_profile_when_no_name_says_sub` covers the FOSCAM
names, a named sub winning over the resolution fallback, one profile, and two
equally sized ones.



