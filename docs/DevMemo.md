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
something like this, then delete it. The traces in `pipeline/rtsp_session.rs`
that name the interleaved channel, the NAL type, the marker bit and the RTP
timestamp found this bug; they are noise once the session is understood.

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
| FOSCAM (北过道, `192.168.27.42`) | Answers the ONVIF Profile T probe with an `a=sendonly` `audioback` media (`PCMU/8000`) and accepts the talk-back `SETUP` and 150 written packets, but nothing comes out of its speaker — direct and through go2rtc alike. The media is advertised; the hardware does not play it. The vendor's own two-way audio is probably its HTTP path rather than this one, so a back channel that answers is not proof of a speaker that works. |

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
builds `avcodec-63` - the crate carries a branch for each). The trim costs one
thing, visible in the script: the x86 SIMD paths need `nasm` on the build
machine. Hardware decoding is compiled in - `--enable-vaapi --enable-ffnvcodec
--enable-nvdec`, the VAAPI and NVDEC hwaccels for H.264 - and it adds two build
inputs of its own. `nv-codec-headers` is headers only: FFmpeg reaches CUDA
through its dynlink shims, so libcuda and libnvcuvid are opened at run time and
nothing NVIDIA-specific travels in the package. `libva` is the other way round:
VAAPI is *linked*, so libva 2.22.0 is built from source and staged beside the
executable with `libva-drm`, or the program would not start on a machine that
never installed it. The bundled libva has the VA driver directories compiled in
(`XGVIEW_VA_DRIVER_DIRS`), because the driver is the machine's own and this
libva has to be told where that machine keeps it.

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

**Building on Android.** FFmpeg is not built there and the manifest says so: it
is declared for targets other than Android, so enabling the crate's default
features pulls none of it into the Android dependency graph. OpenH264 *is* built,
as the fallback for an H.264 stream `AMediaCodec` will not take: the platform
decoder is tried first, and a refusal at open time hands the access unit that was
refused to the software one (`monitor_codec::fallback`). It is C++, so `cc` links
a C++ runtime - Android's shared one, `libc++_shared`, which the build scripts
take from the NDK and put in the APK beside the library that needs it. The static
runtime is the tidier-looking alternative and a trap: Android keeps it in two
archives, and `CXXSTDLIB=c++_static` alone leaves the library loading with its
RTTI undefined ("cannot locate symbol `_ZTVN10__cxxabiv117__class_type_infoE`",
on the device, at launch).

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
| macOS | `videotoolbox` | One entry, and nothing to order: it is part of the system, so there is no driver to look for and no second path behind it. |

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
`strings libavutil.so | grep libcuda.so.1` finding the loader.

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

## 17. H.265: decoded on every platform

*Landed 2026-10-13, opening 1.4. The desktop and Android halves are both
implemented and measured — "What landed" first, then the parts only found on the
way there.*

**What landed.** An H.265 camera has now been in front of the viewer:
`rtsp://192.168.17.87:8554/webcam`, a gortsplib relay whose SDP names `H265` at
90000 and advertises its sets as `sprop-vps` / `sprop-sps` / `sprop-pps`, with
no `a=framesize`. It decodes at 30 fps with `errors=0` on every package - `HW` on
Direct3D 11, on CUDA in WSL, and through `AMediaCodec` on a SHIELD, `SW` on the
CPU when asked for. Four pieces did it:

- `monitor_core::hevc`, a sibling of `h264.rs` rather than a shared body: the
  two byte header (`(byte0 >> 1) & 0x3f`), VPS / SPS / PPS as types 32 / 33 /
  34, the aggregation packet 48 with its two byte length prefixes, the fragment
  unit 49 whose `FuType` rebuilds the header, and key frames found by IRAP type
  16-23. The parts of `h264.rs` that are not codec specific carry over
  unchanged: closing a picture on a timestamp change or the marker, caching the
  sets and replaying them VPS, SPS, PPS before a picture that brings none, and
  `invalidate_current` on a sequence gap.
- `rtsp::parse_parameter_sets` now also collects `sprop-vps` / `sprop-sps` /
  `sprop-pps`; the depacketizer keys them by the type inside each NAL, so the
  order the camera lists them in (`{pps, sps, vps}` here) is not relied on.
- `run_session` picks the depacketizer from the codec, and `video_decoder`
  releases H.265 — and its hardware preference — everywhere now. The H.264-only
  calls beside the decode loop are guarded by the codec.
- `monitor_codec::sps` gained `hevc_picture_size`, and `decode_parameter_sets`
  returns H.264's SPS and PPS or H.265's VPS, SPS and PPS in the order a decoder
  reads them. That was Android's half: `AMediaCodec` is opened with the sets in
  `csd-0`..`csd-2` and a size, and refuses every picture when the size is not
  the stream's (`-30003`, §9). Measured on a SHIELD: H.265 at 1920x1080, `HW`,
  30 fps, `errors=0`, `csd=[24, 62, 7]`.

**The decoder half was already in place**, which is not what was expected. `Codec`
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

**Where the GPU is, platform by platform.** That last sentence - libavcodec
picking the mode itself - holds only where the hwaccel was actually compiled in,
and the packages do not all get their decoder the same way:

* **Windows** links vcpkg's `ffmpeg:x64-windows`, a full build: the HEVC DXVA and
  CUDA modes are in it, and H.265 decodes on the GPU (measured: `HW`, D3D11).
* **Linux** builds its own trimmed FFmpeg (`scripts/build-linux.sh`,
  `--disable-everything`), and there `--enable-vaapi` / `--enable-nvdec` bring in
  only the framework - the per-codec module is what libavcodec opens, and just
  `h264_vaapi` and `h264_nvdec` were named. H.265 decoded on the CPU with a CUDA
  device open beside it. Fixed by naming `hevc_vaapi` and `hevc_nvdec` as well,
  and by folding the hwaccel set into the marker that keys the FFmpeg tree - which
  it was not, so a machine with the libraries already cached kept the H.264-only
  build and the new flags were never compiled. Measured after: 8-14 ms a picture
  through CUDA against about 34 on the CPU.
* **macOS** does not build FFmpeg at all (`scripts/build-macos.sh` links the
  `~/opt/ffmpeg` prefix through pkg-config), and that prefix is a near-default
  source build (`MACOS-SETUP.txt` 4B) or Homebrew's - neither disables anything,
  so `hevc_videotoolbox` is in it. It is also the one platform where the tile
  lied. A mid-2012 MacBook Pro (Intel HD Graphics 4000) showed `HW` on H.265,
  although that GPU has no HEVC decoder and the pictures were decoded on the CPU.
  Two things line up to cause it. FFmpeg asks VideoToolbox for HEVC with
  `EnableHardwareAcceleratedVideoDecoder` rather than `Require...` - software
  fallback allowed, where every other codec gets `Require`
  (`libavcodec/videotoolbox.c`) - so a session is created on a machine with no
  HEVC hardware; and VideoToolbox hands its pictures over in
  `AV_PIX_FMT_VIDEOTOOLBOX` whichever of its two decoders produced them, so the
  format the badge reads carries no answer. The check is put to the system
  instead: `VTIsHardwareDecodeSupported` (macOS 10.13+) is asked before a device
  is opened, and a codec this machine has no hardware decoder for is not offered
  to VideoToolbox at all - it decodes through FFmpeg's software HEVC and the tile
  reads `SW`. H.264 is unaffected (HD 4000 has that decoder), so `HW` still means
  `HW` where there is one. The fix is not yet run on that Mac; what it is expected
  to show is H.264 `HW`, H.265 `SW`.
* **Android** is not FFmpeg at all: it decodes through `AMediaCodec`, the
  platform's own decoder, so there is no hwaccel to compile in and no cache to
  key - and no software fallback for HEVC either.

The lesson for a trimmed build: each codec's hwaccel has to be named one by one,
and the cache that skips the build has to be keyed on that set. The Linux package
had neither, and decoded H.265 on the CPU because of it.

**Android, and how it differed.** The platform decoder is opened from the first
access unit and built to a size, so the two things FFmpeg had been reading out of
the bitstream for free had to be read here instead: a HEVC sequence parameter set
parser for the size, and the parameter sets as separate `csd-N` buffers - VPS,
SPS and PPS, where the H.264 path wrote SPS and PPS. Both are in
`monitor_codec::sps` now (`hevc_picture_size`, `decode_parameter_sets`), and
`amediacodec.rs` fills `csd-0`.. in the order they come back in.

There is no software H.265 on Android - OpenH264 is H.264 alone - so an HEVC
stream a device's own decoder will not take has nowhere to fall back to and
shows the placeholder, where an H.264 one still falls to OpenH264.

**Low latency is safe only on a stream that does not reorder.** The decoders were
opened in low latency mode on purpose - `low-latency` on Android,
`AV_CODEC_FLAG_LOW_DELAY` on the desktop - which was fine while every camera was
H.264 with no B-frames: there is nothing to put back in order, so nothing is
lost. This H.265 stream has B-frames, and a decoder told to be fast hands its
pictures over in the order it decoded them - `2, 7, 5, 4, 6, 11, 9, 8, 10` - so
the tile showed a picture and then an older one, over and over, which reads as a
regular shudder (the report was "it plays a bit, jumps back, plays, jumps
back"). Both flags are gone: the decoder reorders by the presentation times it
was given, and a stream that needs no reordering pays nothing for the freedom.
The order measured after the change is strictly one frame after the next. The
cost on a reordered stream is its reorder depth, a few frames - the price of
showing it in order at all.

**One way the picture would still be wrong once it decodes.**

- **Ten bits.** `Main 10` is a different problem: the whole path is NV12, eight
  bits per sample, one layout. A ten bit stream would fail at decode or at the
  copy back, not at the depacketizer. Still open — what landed is `Main` (eight
  bit).

The colour matrix, once named here as a second fault, is no longer one: the
fragment shader of §6 now writes the matrix and the range of every
`DecodedFrame` from `ColorSpace`, so an H.265 stream tagged BT.709 is read as
BT.709 with no change to the WGSL.

**What happens today if one is plugged in.** On the desktop it decodes and shows
its picture, hardware first and software if the GPU will not have it. On Android
nothing breaks but nothing shows: the channel connects, negotiates, keeps
reporting its statistics and shows a placeholder, with a single `no decoder for
this codec` warning to say why — deliberate, so an unsupported stream looks like
a waiting tile rather than a connection that never stops reconnecting.

**How the work was cut, and what it leaves.** The desktop step needed neither
the SPS parser nor Android: the depacketizer plus the guards, with FFmpeg left
to learn the size from the stream, put a picture on a desktop — this is what
landed. The second step is Android: the SPS reader and the CSD layout above.

**For whoever buys the cameras.** `Main` (eight bit) rather than `Main 10`, and
a model that still offers an H.264 profile on another stream — the H.264 sub
stream is what an Android viewer can show today, and what any machine can fall
back to.

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

---

## 22. A supervised viewer keeps the wall up

*Landed 2026-10-13, in 1.4.12.*

**What landed.** A supervisor now sets `XGVIEW_SUPERVISED=1` on the child it
starts, and the viewer reads it once at start-up. Under that mark, `Back` on
the wall no longer ends the program — the wall is meant to stay up, and the
only way out is the exit button in the settings.

**Why a variable and not `--supervise`.** `--supervise` is not handed down to
the child, or the child would be a supervisor of itself, one process deeper,
for ever. A variable cannot turn the child into a supervisor: only the switch
does that, and the switch is filtered out. The variable is inherited, but the
child is a viewer and never looks for the switch, so the inheritance is inert.

**Why not the parent process.** The child could read its parent's command line
and find `--supervise` there — `/proc/<ppid>/cmdline` on Linux,
`NtQueryInformationProcess` on Windows, `libproc` on macOS. But the parent may
already be gone: a supervisor that crashed leaves the child reparented to init,
and the very moment the mark is most needed is the moment it cannot be read.
The process tree is also reshaped by `xgview-cli`, systemd and the kiosk
wrapper, so the parent is not reliably the supervisor. A variable is set once,
at the only point the relationship is certain, and asks nothing of the tree
after that.

**The exit button.** The system settings tab ends with an *exit* button and a
*reboot* button. Exit leaves at once; reboot restarts the machine behind a
confirmation window, and only where the process has the privilege to do it
(`SeShutdownPrivilege` on Windows, the `admin`/`wheel` group on macOS, root or
`systemctl` on Linux; Android has no reboot permission). Under a supervisor
the exit button is the only way out of the program — `Back` is held — which is
the point of the mark.

---

## 23. MSE: the same stream over a WebSocket, and the reason a channel decodes on the CPU

*Landed 2026-10-13, in the 1.4.x line.*

**What it is.** go2rtc can serve every stream it holds twice: as RTSP (the
restream, `rtsp://host:8554/<stream>`) and as **MSE** — fragmented MP4 pushed
over a WebSocket. Frigate's UI uses the second, and so does the viewer now,
which matters off the LAN: the RTSP port is often closed outside, while the web
port the WebSocket rides is not. The two endpoints differ by server:

```text
Frigate      ws(s)://<host>:8971/live/mse/api/ws?src=<stream>   (its own port)
go2rtc       ws(s)://<host>:1984/api/ws?src=<stream>            (its API port)
```

**The protocol.** The client opens the WebSocket, sends a JSON handshake naming
the codecs it takes, and then reads binary frames:

```text
-> {"type":"mse","value":"video/mp4; codecs=\"avc1.640029,hvc1.1.6.L153.B0\""}
<- {"type":"mse","value":"video/mp4; codecs=\"…\""}   (JSON reply, skipped)
<- <binary> ftyp+moov   init: codec config (SPS/PPS in avcC/hvcC)
<- <binary> moof+mdat   media: access units, AVCC/HVCC length-prefixed
```

`crates/monitor_core/src/mse.rs` parses the boxes (`moov`→`avcC`/`hvcC` for the
parameter sets and the picture size, `moof`/`trun`/`tfhd`/`tfdt` for the sample
table) and converts each AVCC/HVCC sample to Annex-B, so the decoders take it
unchanged. The playback session is `run_ws_session` in
`crates/monitor_core/src/pipeline/ws_session.rs`.

**Two servers, two logins.** The endpoints authenticate differently, and the
path is what tells them apart (`is_frigate_mse`): Frigate fronts its MSE stream
behind the same JWT as its REST API — Basic is rejected with 401, so
`frigate_ws_login` POSTs `/api/login` and carries the `frigate_token` cookie as
a Bearer header — while a standalone go2rtc answers its own `/api/ws` behind the
HTTP Basic account its `api:` section names. The connection, the credential and
the self-signed-certificate TLS (accepted, but with the handshake signatures
still verified) live in one `open_mse`, shared by the playback session and the
discovery probe that measures a stream over MSE. Each camera keeps the two
logins apart — the restream account on the RTSP address, the WebSocket login on
the MSE one.

**The plain-Baseline fix applies here too — this is the part that bit.** §7
covers why a channel can decode on the CPU over RTSP: a stream declaring plain
Baseline (`profile_idc` 66, `constraint_set1_flag` clear) matches no entry in
the Direct3D 11 / DXVA profile table, so the hwaccel is refused and libavcodec
falls back to software. That fix — `sps::constrain_baseline`, setting the one
flag — lived only in the RTSP session, and the MSE session was written
separately without it, so a camera that decoded on the GPU over RTSP fell to the
CPU over MSE. It is now one `BaselineFix` both sessions hold: the RTSP session
observes every access unit as before, and the MSE session observes the init
segment's parameter sets — where its sequence parameter set arrives, before they
are handed to the decoder, which is where `ff_get_format` reads the profile —
and every media sample after. Decided once per session; the flag is H.264's
alone, and the healthy majority of streams is never touched. It is announced at
`info` (`declaring the stream's baseline profile as constrained…`) because
decoding from bytes the camera did not send is not something to do quietly.

**One consequence for the import.** A Frigate camera is imported with **both**
addresses, and its main/sub pairing is measured by resolution over both at once
(`probe_resolution` and `probe_resolution_mse` raced together) — so a closed
RTSP port no longer costs a camera its sub stream. The protocol a camera is
*played* with is a separate choice (`ConnectionMode`: `Auto` prefers RTSP and
falls back to MSE, `Rtsp`/`Mse` force one), per camera, switched from the
Cameras tab. An imported camera's addresses are read-only in the edit form
(`imported`) until it is detached, so one half of a managed pair cannot be
edited out of step with the other.

---

## 24. The focus ring is the remote's cursor, and a press does not move it

*Landed 2026-10-14, in the 1.4.x line.*

**What egui does, and therefore what it does not.** A widget is given the
keyboard focus by the arrow and `Tab` walk at the end of a frame, by `Escape`
clearing it, or by an explicit `request_focus` - and by nothing else. A pointer
press does the opposite: a press outside the focused widget surrenders its focus
(`context.rs`, `pointer_pressed_elsewhere`) and a press inside one grants
nothing. The only widget in the library that takes the focus when it is clicked
is `TextEdit`, which needs the caret. "Clicking a button does not focus it" is
egui's default, not a rule of ours.

**The rule here.** The ring is the remote's cursor: it is what the arrows walk
from, and `Owner` - whether the keys belong to the wall or to the controls - is
decided by whether anything holds it. A press is an action, not a placement, so
it neither takes the ring nor hands it back. Capture is opted in only where a
control's own model depends on it: the tab strips, the reorder arrows, and any
text field.

**Where a press does move it, and why.** A panel hands the ring to itself once,
as it appears, whichever input opened it (`Handoff`): a modal that appears
without the ring is unreachable to a remote. The wall is the other way round - a
press on a tile moves the *tile* ring and gives the widget focus back, because a
control that holds it keeps the bars up and the wall keys would not come back.

**Handing it back.** Where a window remembers the button that opened it, it
records it only for a press that came from the keyboard or a remote
(`!response.clicked_by(PointerButton::Primary)`). A window opened with the
pointer closes and the ring stays where it was; the frame's anchor puts it back
when the panel behind is still open. Several places were written the other way
round and had to be corrected, which is §14's lesson again: the `Id` is shared
between uses, and so is the ring.

**A ring that is not ours to place: the file dialog.** It is drawn by a crate
inside an egui window of its own, and nothing in it is ever registered under a
scope here. The frame's cleanup - the one that lets a focus with no widget
behind it go, so the accessibility tree does not panic on it - would therefore
let the file name field's go on the very frame the dialog took it. Nothing
focused is then exactly what the crate's type-to-search reads as "the viewer is
typing at the list": a keystroke meant for the name becomes a search. The
cleanup is held back from it while it is up, the way `panel_anchor` and the
hand-offs are. Inside it the ring is the crate's, kept and given up by its own
rules - an arrow key breaks out of the field and into the list, by design.

**The ring stands only while the wall is being operated.** *Landed 2026-10-18,
in the 1.4.x line.* It used to be drawn whenever the bars were on screen, and
for the bars' idle time after they went; that is a cursor standing on a wall
nobody is watching it on, and on a fresh start it is one standing there before
the viewer has touched anything. The reading is now the viewer's own input:
`ring_seen: Option<Instant>` is armed by `ring_keys` (and by a click on a tile)
and the ring is drawn within `RING_IDLE` = 8 s of it, so it comes back with the
first key that walks it or a click on a tile, goes eight seconds later, and starts
out `None`, which is what keeps a fresh run's wall free of a cursor.

**What arms it is what can place it.** `ring_keys` - not the bars' reading, and not
"any input": a cursor is placed by a key that walks it (the arrows, Back, the F1/F2
that carry it into the bars, and Enter or Space **while the wall owns the keys** -
on a control they activate the control, which is how a remote presses a page
button) and by a click that lands on a tile, and by nothing else. The pointer's
other actions place nothing: a press alone is the start of a page turn, a release
is the tail of one already answered, a movement crosses the wall, a wheel scrolls
a body. **A page turn is never one of
them, whichever way it is made** - the page keys, the toolbar's page arrows and a
swipe all move the picture rather than the ring, and that is what makes the three
of them behave alike. **Anything that moves the wall's cursor while the viewer is
not aiming takes down a ring that is showing** - `ring_follows_cursor`, which
compares the page *and* the focused channel: a page turned re-places the focus
into the new page, a layout that shrinks under the focus re-places it the same way
(a 4x4's ninth channel is the third one in a 2x2), and switching a camera off
clamps it. The one frame it leaves alone is the one that aimed, since an arrow
walking off the edge of the wall turns the page itself and the ring follows it in.
A view change does re-place the focus as a side effect
(`normalize` keeps it on the visible page), but that is the page moving under the
cursor, not the viewer aiming at a tile - and aiming is the whole of what the ring
is for.

**Its clock is its own, and the bars meet it at the two ends of their life.** The
ring does not follow `chrome_idle_secs`: that is the viewer's setting for how long
the bars stand in full screen, and a cursor that faded with them would leave the
wall unaimed halfway through a press. `ring_standing` therefore reads the ring's
own ten seconds, and `ring_follows_bars` is all the bars do to it - arm it as they
appear, take it with them as they go, and leave it alone in between. **The state
it compares against has to be read as the frame begins, not where the bars are
drawn**: entering full screen takes them away in `handle_keys`, before anything
is drawn, so a `was` read at the drawing already reports them gone and the
transition is missed - which is how the ring came to stand on a picture with no
bars for ten seconds after F11.

**Why not a gate on `chrome.visible`.** Written that way first, and it does not
survive the wall: the wall's own keys are deliberately not a request for the bars
(`viewer_active` exists to keep them away - the bars take their height from the
wall), so in full screen, with the bars away and the viewer driving the wall with
the arrows, nothing can bring them back. A ring drawn only while they were up was
therefore never drawn at all in the one case the ring exists for. The connection
has to be the two transitions, not the state.


## 25. Two activities, so the wall comes up already turned

*Landed 2026-10-16, in the 1.4.51 line.*

**The problem.** Android fixes an activity's window orientation from the
manifest's `android:screenOrientation` at creation time - before `onCreate`,
before any code runs. Whatever the manifest says is what the first frame is
drawn into. Calling `setRequestedOrientation` from `onCreate` changes it
afterwards, and the change is visible: the wall comes up one way and turns
to the other. A remembered portrait started landscape and rotated; a
remembered landscape, under `unspecified`, started portrait (the phone's
natural direction) and rotated. No single manifest value avoids both,
because the manifest is static and the preference is not.

**The fix.** A transparent `LauncherActivity` is the entry point. It reads
`xgvconfig.json` - the same file the native side will read a moment later -
and starts the matching fixed-orientation activity: `MainActivity`
(`userLandscape`) or `MainActivityPortrait` (`userPortrait`), then finishes.
The window the native thread draws into was created with the right
orientation from the manifest, so there is no rotation to see.

**Why two classes, not an alias.** `activity-alias` does not support
`android:screenOrientation` - only `<activity>` does. `MainActivityPortrait`
is an empty subclass of `MainActivity`; the `private static native` methods
are declared on `MainActivity` and resolved through that class by JNI, so
the subclass needs no declarations of its own. `instance` is
`static volatile MainActivity`, and a `MainActivityPortrait` is a
`MainActivity` by inheritance, so the static callbacks the native side
finds through `FindClass("com/xhbl/xgview/MainActivity")` keep working
unchanged.

**What the launcher does not do.** It does not load the native library or
start the native thread - that is `MainActivity.onCreate`'s job, via
`NativeActivity.super.onCreate()`. It does not decide the backend
(`applyVulkanGate` stays on `MainActivity`, before `super.onCreate` there).
It does not handle orientation changes at runtime - those still go through
`setRequestedOrientation` on the current activity, which works fine once
the wall is up. The launcher only runs on cold start; the boot receiver's
`getLaunchIntentForPackage` finds it through the `LAUNCHER` category it
carries in the manifest.

**A launcher that reads the wrong name.** It reads the remembered orientation
out of the same file the native side writes. When the configuration files were
renamed (§28), the name was changed in Rust and in the launcher's comment but
not in the launcher's own `File` - it still looked for `config.json`, found
nothing, and answered "landscape" every time. The two-activity scheme worked;
what it was reading simply was not there. A name that appears in Java and in
Rust is a name that has to be changed in both.


## 26. Upright is a second wall, not the first one turned

*Landed 2026-10-16, in the 1.4.x line.*

**Why a second layout set.** A 16:9 wall on a phone held upright is four thin
strips with the picture shrunk into the middle of each. Landscape grids divide
the width; a phone stood up wants rows. So the portrait layouts are their own
set - `1x1`, `1x2`, `1x3`, `1x4`, one column - and they are kept out of
`GridLayout::ALL` entirely (`GridLayout::PORTRAIT` is a list of its own).
Anything that walks `ALL` - the density cycle, the layout picker, the digit
keys for landscape - would otherwise step into a one-column grid it cannot
draw sensibly in a wide window, and the surest way to keep them apart is to
never put them in the same list. The wall switches sets when the orientation
changes; in portrait the digit keys `1`-`4` pick the stacked grids directly.

**The state, doubled.** Turning the device to look at the same cameras another
way must not throw away the way you had them the first time, so the two
orientations keep their arrangement separately: a group holds a `layout` and a
`portrait_layout`, a camera holds an `aspect` and a `portrait_aspect`, and the
orientation itself is `android_orientation`. `set_orientation` parks the grid
being left through `sync()` before loading the one being entered, so neither
overwrites the other. Page and focus stay shared and are clamped to whatever
grid is now in force - a page number means "the second screenful", and both
ways of stacking have a second screenful.

**Where it is offered.** Only where the device can plausibly stand upright:
a touchscreen, and not a television (`android::supports_portrait`). Without
that the orientation button is not drawn at all, and `F1` is likewise inert
while upright - a room without a touchscreen has no reason to be vertical, and
offering the toggle there would only be a way to get into a layout with no way
out. The setting panel and the layout picker are landscape constructs and are
closed on the way in rather than removed from portrait.

**What turns with it.** The OSD rotates a quarter turn with the picture, but
only where it would otherwise be unreadable: portrait *and* zoomed, which is
the single-camera full-screen view, where the video itself is turned to fill a
tall window. On a grid tile the text stays level and is simply re-laid for a
narrower box. The camera's `portrait_aspect` is what lets a 16:9 sensor be
letterboxed the other way round when the tile is tall, and it is chosen per
camera through a picker of its own - the same crossing of the wall's cells
against the file's cameras as everywhere else (§29).


## 27. A corner that holds a stack: the combined OSD slots

*Landed 2026-10-16, in the 1.4.x line.*

**The problem.** A tile's corner showed one thing. `Detail`, the item that
stacked several - codec, rate, size - had been added for exactly this, and it
was one fixed stack in one label. A second corner wanted the same idea with
different contents, and there was no second one to have.

**Four slots, one variant each.** `Detail` became `Combined1` and three
siblings join it: `Combined1`-`Combined4`, each standing for a list of atomic
items the viewer picked. The old name is kept as a `serde` alias on
`Combined1`, because the enum is written by variant name and a configuration
saved as `Detail` has to keep opening - an unknown variant would take the
whole document, cameras included, down with it (`from_json` refuses the lot).
`OsdItem` is now thirteen: `Off`, the seven single items, `DateTime`, and the
four slots. `DateTime` is new as well, a `yyyy-mm-dd hh:mm:ss` clock, and it is
atomic like the rest.

**What a slot may hold.** `OsdItem::ATOMIC` is the eight items that can sit
*inside* a slot - the combined variants are deliberately not in it, so a slot
cannot hold a slot. The lists live in `combined_info`, four `Vec<OsdItem>`,
slot 1 defaulting to `[Codec, Fps, Format]` - the old `Detail` stack, so an
upgraded configuration shows what it used to.

**Why the settings button skips empty ones.** A corner is walked through by a
single cycle button, and `cycle_options` builds its list from the slots that
are not empty. A slot the viewer has never filled is not offered, so the
button cannot come to rest on a corner that shows nothing - an item that
exists only to be blank is a step the viewer has to walk past every time.


## 28. Sealing a configuration: arithmetic, not memory

*Landed 2026-10-16, in the 1.4.x line.*

**Why the old scheme had to go.** The exported copy was an [age](https://age-encryption.org)
file behind a passphrase, and age's scrypt stretch is memory-hard by design:
every attempt asks for a large fixed slice of memory before it can even fail.
That is deliberate - it is what makes an offline guess expensive - but it is
memory the smallest machines simply do not have to spare, and a television box
asked for it does not run slow so much as grind: a swap storm, with the wall
stopped while it swaps. An export that hangs the viewer is not an export.

**What replaced it.** PBKDF2-HMAC-SHA256, two hundred thousand rounds, over
the passphrase, and the key it yields put over the document by
XChaCha20-Poly1305. PBKDF2 is arithmetic and a hash of state - it asks for no
memory to speak of, which is exactly what the smallest machine can afford; the
cost in guesses-per-second is what is given up, and the round count is written
into the document so raising it later leaves what is already out there
readable. The 24-byte nonce is what makes a fresh random one safe with no
counter behind it, and it is why `XChaCha20` rather than the 12-byte
`ChaCha20` of the same crate.

**The format.** `xgv-seal/v1:` then base64 of the frame - the round count as
four big-endian bytes, a fresh 16-byte salt, the 24-byte nonce, then the
ciphertext with its Poly1305 tag. Nothing in the frame is trusted before the
key exists: it is read before the passphrase is stretched, so every length it
names is checked, and a round count of zero or above `MAX_ROUNDS` is refused
there and then. Four million rounds is not a number this program writes; a
document asking for it is asking for an attempt that outlasts the viewer, and
reading it is not a way to find out whether the passphrase was right.

**On a thread of its own.** Two hundred thousand rounds is hundreds of
milliseconds, and the render loop has a wall to draw; `spawn_seal` and
`spawn_open` hand the document and the passphrase to a thread of their own and
`poll_sealing` takes what comes back on a later frame. A wrong passphrase and
a damaged file come back as the one answer they are - the authentication
refusing - because nothing distinguishes them that does not amount to
weakening it.

**The names.** The stored file is `xgvconfig.json` and a travelling copy is
`xgvconfig.seal`; both carry the application's name so a stray one is
identifiable as this program's. The rename is what found the launcher reading
the old name (§25). Only the clear JSON is read on import - a document sealed
by an earlier version is not recognised and not migrated, and the two forms
are not interchangeable.


## 29. A channel index is a place on the wall, not a place in the file

*Landed 2026-10-16, in the 1.4.x line.*

**The shape of the confusion.** `AppConfig::active_cameras()` returns the
enabled cameras of the active group, cloned out as a list. Everything above it
- the scheduler's channel indices, the cells a page hands out, the number a
dialog numbers its rows by - counts positions *in that list*. The file's
`cameras` array is indexed by *slot*: the position an entry occupies among all
cameras, disabled ones and other groups included. The two numbers agree only
while the wall is on the first group and nothing before the camera in question
is disabled. They are the same integer type, they are frequently both `0`, and
they are not the same thing.

**Where it showed.** The portrait aspect picker took `grid::page_cells()`'s
cell index, which is a wall position, and read `config.cameras[cell]` with it.
On the built-in group with nothing disabled that is the right entry, so the
bug hid: the dialog listed the built-in group's cameras and the group in
front of the viewer looked like the one being ignored. On a later group, or
behind a disabled camera, it edited the wrong camera's aspect.

**The crossing, named.** `AppConfig::active_slots()` returns where those
cameras sit in the file, in the same order, and it is the one place the two
numberings are converted. A dialog that turns a wall position back into an
entry goes through it - `cameras_list` had always done so, the picker had not.
The regression test pins the case that hid the bug: a disabled camera between
two enabled ones, on the first group, where the two numberings disagree most
quietly.


## 30. Pushing the magnified picture in: a window, not a second stream

*Landed 2026-10-18, in the 1.4.x line.*

**What it is.** The magnified viewport is one camera filling the screen, and
reading a detail off it means showing *less* of that picture over the same area.
So the tile shows a **window** onto the source, and shows that window whole:
`PictureZoom` is the window's scale and its centre in the picture's own
coordinates, `grid.rs` turns it into the source rectangle the tile samples, and
everything else - the decoding, the streams, the layout, the letterboxing that
placed the picture in the tile - carries on untouched. The scale is therefore a
measure of what is *not* being shown, and pushing in costs one texture lookup
rather than a reconnect. That is what makes this worth having at all: the
magnified view is already pulling the main stream, so the detail the viewer came
for is already in the texture.

**Two ways to draw a window.** The plain canvas hands the window to
`painter.image` as its uv rectangle. The turned canvas - which is what the upright
magnified view draws with - samples four corners through a mesh of its own, and
the window there has to be turned with the picture: what is further down the
picture lies further along the texture, and what is further left lies further
down it. The whole window is exactly the four literals that canvas was written
with, which is how a picture nobody has pushed in is drawn as it always was
(`a_turned_window_is_sampled_where_the_picture_puts_it`).

**What egui does with a second finger, and why the drag had to be taken away.**
`egui-winit` names the first touch the mouse pointer and pushes each finger as its
own `Event::Touch`, so a pinch arrives twice over: as the fingers, which is
`InputState::multi_touch()`, and as a pointer being dragged across the picture.
Both are wanted - the drag is how the picture is moved - but never both at once.
A drag with two fingers down would step the magnified viewport to the next channel
under them, and a drag *after* the picture has been pushed in is the picture
moving rather than the channel changing. `draw_grid` therefore drops what the tile
reports as a drag in both of those cases and reads the pointer's own movement
instead - `pointer.delta()`, and only from a press whose `press_origin` is on the
wall, so a drag that began on a bar stays the bar's. The channel after this one is
still reached from the toolbar's arrows or the page keys, which step a magnified
viewport already.

**Two things about the reading itself.** `MultiTouchInfo::zoom_delta` is a
per-frame ratio and not a total, so the frames are multiplied in; and the first
frame of a gesture measures nothing, because `TouchState` forgets the previous
values when the number of fingers changes and takes the current one for both - so
the ratio there is exactly 1 and the window stays where it is. The aim is
`center_pos`, the fingers' middle, which is what the picture is pushed in about:
the frame after a gesture in which the fingers merely moved leaves the window
exactly where it was. A trackpad has no fingers to count and sends zoom events
instead - a mouse's ctrl-wheel arrives as the same factor - aimed at the pointer.

**When the window is given up.** It belongs to the viewport rather than to the
channel, and a viewport is a new one every time: magnifying, stepping to the next
channel and coming back to the grid each set `PictureZoom::FULL`, because part of
one picture means nothing on the next. The ends are the picture's own - the scale
never goes below whole, never past `PICTURE_ZOOM_MAX` (eight, which is where a sub
stream stops being a picture), and the window never leaves the picture, because
there is nothing outside it to show. The ring is left out of all of it: a pan
moves neither the page nor the focus, so `ring_follows_cursor` has nothing to take
down, and neither a pinch nor a drag is an aim (§24).

**The mouse, and the wheel that is a scroll.** egui reads a wheel as a scroll
unless it is turned with `options.zoom_modifier` (`input_state`:
`is_zoom = modifiers.matches_any(options.zoom_modifier)`), which is right
everywhere except here: the magnified viewport is the one place in the interface
with nothing to scroll. So the plain wheel is read on its own, and the factor is
egui's own - its `scroll_zoom_speed` over the delta it has *already smoothed* -
which is what makes a plain wheel and a ctrl-wheel zoom by the same amount per
notch: the smoothing splits one notch over several frames, and the factors
multiply, so the pieces compose to exactly the total. It is read only while the
pointer is over the wall, so a wheel the settings body scrolls with stays the
body's, and one turned with `Modifiers::COMMAND` is left to `zoom_delta()`, which
`pinch_frame` already folds in. Reading it is passive: nothing is taken out of the
queue, and a `ScrollArea` that consumed the delta has already zeroed the field.

**A zoom is not a request for the bars.** `wall_event` answered everything it did
not name with "the viewer is here, and asks for the bars" - true of a wheel while
a wheel did nothing on the wall, and wrong the moment it pushed the picture in: in
full screen, zooming brought the bars back over the picture, which is the one
thing a viewer pushing in to read a number plate did not ask for. The wheel and a
trackpad's `Event::Zoom` are now answered the way the wall's own keys and a swipe
are - **not** a request for the bars - so the bars keep their own clock while the
picture is being read. With the focus off the wall the whole of `any_input`
answers instead, and there a wheel is scrolling something, so it still counts
(§24).

**Not to be confused with the pinch that is not here.** A pinch on the *grid* -
two fingers over a tile to magnify it - was written first and taken out again:
magnifying is what the double press is for, and the gesture the viewer wanted in
the magnified view was this one.


## 31. A turn handled in place: the surface Android 8.x leaves behind, and the notch under it

*OPPO PBAM00 (Snapdragon 450, Adreno 506), Android 8.1 / API 27 — 2026-10-19.*

Two faults on the same generation of Android, both about the window a
`NativeActivity` is given. One is a turn that leaves the renderer drawing into
the old surface; the other is a notch the window covers and the UI does not keep
out of. They were fixed together in 1.4.82 and 1.4.83.

**The turn is handled in place, and Android 8.x forgets to say so.** The manifest
lists `orientation` and `screenSize` among the activity's configuration changes,
because the native side owns a winit event loop and cannot have a second one;
`setRequestedOrientation` turns the existing activity instead of recreating it.
That part is deliberate (§25). What the older platform does *not* do is raise
`onNativeWindowResized`: logcat shows only `ContentRectChanged`, which winit
answers with `TODO: find a way to notify application of content rect change` and
nothing else. So no `WindowEvent::Resized` is sent, eframe never calls
`Painter::on_window_resized`, and the swapchain keeps its old extent.

**The geometry gives it away.** SurfaceFlinger had the layer at `size=(1520,720)`
- the window the turn produced - with the application's buffers still at
`activeBuffer=[720x1520]`. `wgpu` sets the buffer geometry once, when it creates
the surface (`ANativeWindow_setBuffersGeometry(window, 0, 0, format)`, meaning
"the window's size"), and winit reads that geometry back through
`ANativeWindow_getWidth()` as the window size. On this generation nothing moves
it again, so even the size winit would report is the old one. Pausing and
resuming the activity - which destroys and recreates the native window - made
the picture return at once, which is what separated a stale surface from a
broken renderer.

**The fix is to make the platform compare again.** From
`MainActivity.onConfigurationChanged`, a one-shot `OnGlobalLayoutListener` waits
for the first layout whose shape matches the new display (a quick second turn
replaces the pending listener). Then `monitor_gui::android::set_window_geometry`
moves the buffer geometry to the display size, and the activity calls its own
`surfaceChanged` with the same figures. The framework's
`onSurfaceChanged_native` compares the window's dimensions with the ones it
remembered and raises `onNativeWindowResized`; winit turns that into `Resized`;
eframe rebuilds the swapchain. The log line is:

```text
native surface resized to 1520x720
```

The whole path is gated to `SDK_INT < P`. Android 9 and newer send the resize
themselves, as the device tests below show; on those releases the activity
returns before registering the listener.

**The notch was a second, unrelated hole.** With the status bar hidden the
activity reported `top = 0` for every edge, because only the navigation bar had
ever been reserved for. But the notch is still part of the window on Android
8.x: OPPO publishes it as `ro.oppo.screen.heteromorphism=229,0:492,53`, and the
window's `stable` inset was `[0,53][0,96]` - 53px of notch and status bar above,
96px of navigation bar below. The portrait toolbar was drawn in those 53px.

The fix reads the vendor's cutout on releases without `DisplayCutout`, caches
the vendor test, and takes the thickness from the OPPO rectangle (its height;
the stable top inset is the fallback for Xiaomi's and Huawei's properties, which
carry no usable size). Upright it is a top inset. On its side the notch is on a
narrow edge, so the same thickness becomes a side inset: `Display.getRotation()`
is `ROTATION_90` when the device has been turned 90° counter-clockwise (the top
edge on the left, as that call's own documentation defines it), and
`ROTATION_270` when it has been turned the other way. The side is added even in
immersive mode - a control under the notch cannot be used - while the navigation
bar's own strip stays tied to the setting. A device with none of the properties
keeps a zero inset.

A 180-degree portrait turn changes no configuration at all - the orientation
stays portrait - so a `DisplayManager` listener re-measures the insets when the
display changes, and the same thickness moves to the bottom. That path is kept
even though this OPPO never takes it: its sensor policy stops at 0/90/270 (an
explicit `reversePortrait` renders correctly, but the app does not force one).
A device that does hand the wall a 180-degree turn gets a clear bottom edge
instead of a notch across the status bar.

**Later releases already do both.** Measured, not assumed:

| device | system | what it does |
| --- | --- | --- |
| OPPO PBAM00 | 8.1 / API 27, Adreno 506 | rotation black before 1.4.82; notch 53px in portrait, and on the left in the landscape that was tested, after 1.4.83 |
| vivo X21UD A | 9 / API 28, Adreno 512 | rotation fine with no workaround; the window already starts 84px down in portrait and 65px in from the left in landscape, and the cutout's insets relative to the window are `Rect(0, 0 - 0, 0)` - the system moved the window below the notch |
| HUAWEI VTR-AL00 (P10) | 9 / API 28, Mali-G71 | no notch; rotation fine with the workaround skipped, so the 8.x fault is the platform's, not GL's |
| Samsung SM-S711U1 | 16 / API 36, Adreno 730 | punch-hole; window kept clear of it by the system; unchanged by this work |
| Mi Pad 4 | 8.1 / API 27, Adreno 512 | no notch; the rotation workaround applies, the notch inset is zero. **Not yet re-tested on the device** |

That is why no `DisplayCutout` code was added for API 28 and newer: on the
vivo the window is already inset, and adding the cutout's safe inset on top
would leave a second, empty band.

**Left open.** The reverse landscape on the OPPO was not exercised remotely -
the shell there has no `WRITE_SETTINGS`, so `user_rotation` cannot be set; the
`ROTATION_270` side is derived from the platform's definition and needs one
manual turn. On Xiaomi and Huawei 8.x devices the notch thickness falls back to
the stable top inset, which some of them report as zero in landscape; a side
band there is unproven. And a later OEM that lets a fullscreen window cover its
cutout would need the official cutout inset after all, guarded against a window
the system has already moved.

*One crash seen once on the OPPO, while this was being tested: a `SIGSEGV` in
`libGLESv2_adreno.so`, `EsxContext::GlPushDebugGroup` calling `strlcpy`, during a
start with ten channels. It did not reproduce on relaunch, and nothing in the
inset or resize paths points at it.*

## 32. The magnified viewport's bar: a strip of the interface's own buttons

*All platforms — 2026-10-19, 1.4.85–1.4.87; egui's own buttons, 2026-10-25.*

**What it is.** A small strip of controls drawn inside the magnified viewport:
the picture fit, a sound switch, and (from 1.5) the microphone. It is not a
panel and not a window; it is part of the wall's own picture, for the few things
a viewer wants while reading a detail - the shape of the picture, whether it is
heard, and whether they are talking to it - without leaving the magnification or
waking the chrome.

**Where it lives.** In the *picture's* frame, mapped onto the screen the way the
picture is: a quarter turn clockwise for a portrait wall's landscape stream, so
the strip turns with the picture and its words come out upright to the viewer.
The strip keeps 24 points from the picture edge and is centred along the edge's
whole length; its thickness is 56 and its controls are 44 deep. It used to have
activation bands - the depth the wall's own bars occupy, so the press that
summoned them summoned it - and those are gone: it is put up by a tap, wherever
on the picture the tap lands.

**Who answers it.** Since 1.5 the strip is a row of egui's own buttons -
`icons::button_in`, a button placed where the picture puts it rather than one
egui lays out - registered with the navigation layer under a scope of its own
(`tile-bar-row`, or `tile-bar-column` once the picture is turned). Only the
glyphs are drawn by hand, the same bargain the toolbar's icon buttons make. That
is what makes the bar behave like the rest of the interface for free: a press
lights the control it went down on, from the press and not the release (egui's
`widgets.active`), the ring is the focus the arrows walk, and the confirm key is
egui's own click on whatever has that focus. Until then the strip was answered
from the wall's painter and the tile's `Response`, with `Bar::armed` tracking a
cursor by hand, and the viewer saw a strip that looked like the bars and
behaved unlike them: a press that lit nothing until the finger came up, a ring
that could not be walked, a focus that was not egui's at all. Using it is also
not a request for the wall's bars - `viewer_active` answers false while the remote
is in it, where the focus being on a control would otherwise hold the bars
woken - so a press on a control cannot slide the chrome back over the picture it
belongs to.

**The seat.** The bar opens on a *seat*: a focusable, invisible, non-interactive
widget laid at the strip's first point - the trick the portrait fit picker uses
for its hint. It draws nothing and answers nothing, and it is a focus target
rather than a stop of the walk: it is not registered with the navigation layer,
so no arrow can walk back onto it, and `handle_keys` maps the first arrow off it
onto the first control (the handset while a call is up). A confirm key on the
seat is therefore spent, which is the rule the ring follows everywhere - it is
the arrows' cursor, and a bar nobody has walked carries none.

**Opened at the bottom.** The confirm key, and a tap, bring the strip up along the
bottom edge: neither is pointing at an edge, and the status side is where a strip
of controls is expected. `Bar::dock` still names the side - the side a bar is
drawn on is a property of the picture's frame, and the turned picture puts it
down the screen's side.

**Where it stands by default.** Upright and not turned, the strip is up from the
moment the picture is magnified and stays: `tile_bar_pinned` answers the state and
`settle_tile_bar` is what a change of picture consults - a magnification, a
channel step, the quarter turn - so turning the picture takes the strip with it
and turning back brings it again. The clock is kept up by the pin rather than
stopped, which is what keeps a call's handset up for the same reason. Turned, or
in landscape, the picture is asked for the strip: it fills the screen and a strip
along its side would be in the way. A bar that is already up - the upright default
or one a tap put there - is entered with the confirm key, which puts the focus on
its seat so the ring stays off until an arrow walks it. Until it is entered the
keys are still the wall's: the arrows step the channel, and Up and Down walk off
to the wall's own bars - a strip that is merely up is not the remote's surface
yet, which is what `bar_owns_keys` asks in `handle_keys`.

**The clock and the way out.** Eight seconds after the last operation on it, or
as soon as the picture is stepped, panned, or the wall is given to something
else, the strip goes. Every key it answers counts as an operation, not only the
presses - the early `return` of the controls branch used to leave the bar's clock
counting from the opening, so a viewer walking it saw it vanish mid-walk. `Back`
takes it down and the magnification after it, which means `Back` is the bar's
before it is the panels': the controls branch hands the focus back to the wall
and hides the strip, and the next press is the wall's. A tap on the picture
closes it; a tap on the strip itself (the padding included) does not, so aiming
at a control cannot dismiss it by grazing the strip. The 300 ms guard after
opening keeps the second tap of the double press that opened it from pressing a
control or shutting it.

**The tap.** One rule for the pointer, both ways: the strip is put up by a tap
and put away by one. A tap is a press that came up quickly - 0.3 s, well inside
the 0.8 s egui gives a click - stayed where it went down (4 points, tighter than
egui's own 6), and was the only finger on the glass. Everything else the pointer
can do over the picture is the picture's: a press that lingered is a thumb left
on the glass and does nothing, a drag that does not turn the page pans or does
nothing and leaves the strip standing, a pinch or a wheel pushes the picture in
and leaves it standing too. Only two things take it away besides a tap: the
picture changing - a page or a channel step, the magnification ending - and the
wall being given to something else. The earliest version of this closed the strip
on any click egui reported, which a slow touch and a finger that slid by a few
points both produce; the bands it used to be opened from are what the tap
replaced.

**A double press is still the way back to the grid**, and it is what makes the
tap a question with two answers. The first press of a double press is a tap, so a
tap that opened the strip would put it on screen for a moment on the way to the
grid - and a strip that was already up would be put away and asked for again.
The ask is therefore held: a tap stores when the strip may go up - immediately
plus egui's own double-click delay - and the frame that reaches that time is the
one that puts it up, unless the second press arrived first and dropped it. The
close has no such ambiguity and stays immediate: while the strip is up, a tap
puts it away, and a double press puts it away and then goes back to the grid. The
cost is the delay on opening a strip by touch - 0.3 s, the same window the
double press itself is measured in, and the same one egui makes any other
double-clickable control wait.

**The ring and the press.** A press lights the control under the pointer for as
long as it is held, and nothing after it: that is egui's `widgets.active`, the
same look the buttons of the bars give a press. The ring is the focus, so only an
arrow puts it on a control and only an arrow moves it; a touch never does. A call
started by the remote keeps the ring: the microphone it was on has become the
handset, so the focus is moved into that control rather than being lost with the
old one - the press that started the call from the keyboard is the press that can
end it. A focus that no longer names a control of the bar goes back to the seat.

**Turning the glyphs.** The portraits' bar carries its icons turned with the
picture, and a turned rectangle has to be built from its two corners rather than
from a minimum and a maximum: a quarter turn swaps the direction of both axes, so
the corner that was the icon's top-left is the screen's top-right and
`Rect::from_min_max` produced an inverted, empty rectangle. That is how the
microphone lost its capsule (the head, the part that was a rectangle) and the
picture fit its outer frame, while the strokes that draw the yoke, the stand and
the corner ticks came out whole. `icons::unit_rect` builds them with
`Rect::from_two_pos` now, and a test holds the turned capsule to a positive size
inside its square.


**The strip.** Every control is a square the depth of the strip, its glyph 22
points, drawn from the icon set and turned with the picture: the frame with the
arrows is the picture fit, the speaker is sound, the microphone is talk-back, and
a call replaces all three with the red handset. There used to be a word for the
fit, measured in monospace 14 and bounded to 56..96 points; the icon took its
place, so the width a control asks for is no longer a question and
`Control::Text` went with it.

**Where the fit says what it became.** An icon cannot name the fit it moved to,
so the strip says it in a toast in the middle of the screen - `原始`, `拉伸`,
`16:9` and the rest, through the same language-pack keys - while the corner OSD
keeps naming it on the picture. It is the one toast placed at the centre rather
than above the status bar: the news belongs to the picture the viewer is looking
at, not to a channel. `Toast::center` carries that difference, and the label is
drawn with `TextWrapMode::Extend` - an `Area` starts with no width of its own and
a wrapped label stacked a two-character fit name one character per line.

**The fits.** The wall dresses a tile with `aspect` in landscape and
`portrait_aspect` upright - two choices, per camera, chosen by orientation. The
magnified viewport is neither: a camera carries `zoom_aspect`, absent until the
strip's picture-fit control writes it, and `Original` (the camera's own ratio)
until then. `tile_aspect` is the one place that chooses: magnified it reads
`zoom_aspect`, otherwise the orientation's wall fit. The settings panel and the
portrait aspect picker write the wall's pair; only the strip writes the
viewport's. A configuration written before the field existed reads it as absent,
so a camera's first magnification is its own ratio.

**The quarter turn.** Upright, the magnified picture used to be turned a quarter
turn to fill the screen - which is what a landscape stream needs on a phone held
the right way up, and what many viewers do not want, because a camera that sees
the world sideways then has to be read sideways too. It is drawn as the camera
sends it now, letterboxed, and the strip's first control turns it when the
viewer wants the screen filled: `zoom_rotate`, one answer for the whole
configuration - the viewer who wants an upright screen filled wants it filled for
whatever they step to next, and a letterbox is not a property of a camera. It is
absent - not turned - until the control is pressed, and the control is only there
upright, where there is a screen to fill; a landscape wall never turns a picture.
Everything the strip does follows the answer: the ring, the layout and the
activation bands are read in the picture's frame, so a turned picture carries a
strip down the screen's side and an upright one a strip across its bottom, and
the arrow that walks the strip is the one pointing along it. The glyph is
`Icon::Rotate`: an arc most of the way round with a head on its end, and the
picture's frame in the middle.

**Two faults found on the way.** The hidden bars leave two invisible `Area`s over
their bands (`xgview-toolbar-band` and `xgview-status-band`) whose job is to keep
the summoning press off the tile. Magnified, that is exactly the press the strip
needs, and in landscape the two coincide - which is why the strip opened in
portrait (where the band is at the side) and not in landscape. The overlays are
now skipped while the viewport is magnified. The other: the strip was laid out
before the press with the *old* dock, so the frame that opened it painted at the
old edge and the next frame jumped to the new one. It is laid out again after the
press, with the dock and the measured label it has now, so the opening frame is
already in place.

**Left open.** The clock is still a flat eight seconds for every control; a call
suspends it because the handset is the way out and must not leave. The centred
toast is also used only by the fit so far - the other news has stayed where it
was, above the status bar.

## 33. One damaged length, every picture after it: why a camera can be pulled over UDP

*Landed 2026-10-01, in the 1.2.x line.*

**The default, and what it buys.** RTSP is pulled over TCP with interleaved
framing unless that camera says otherwise. One connection then carries the
control exchange, the RTP and the RTCP: it crosses a firewall that would have to
be opened twice for UDP, and it loses nothing - TCP retransmits whatever the wire
drops and hands the bytes over in order. A gap in the RTP sequence numbers over
TCP therefore says the *sender* skipped those packets; it is never the network.
Our gap counter is computed on the UDP path alone, where reordering and
duplication have to be written off (§16). `SETUP` asks for `interleaved=0-1` and
takes whatever pair the answer names, because a server is allowed to frame the
video on channels of its own choosing (§20).

**What went wrong, and whose end it was.** A Synology Surveillance Station's
high-resolution restream, pulled over the interleaved transport, came up and
stayed up - the session was healthy and packets kept arriving - while the
pictures were wrong. The shape of that matters: **nothing was lost in transit.**
TCP retransmits and delivers in order, so whatever the wall did not get to draw
was either never written by the relay or written in a shape the client could not
use - a packet the relay's own queue let go, or an interleaved length that did
not match the payload behind it. Both ends are the source's, and from the wall
they look alike: pictures missing, while every counter that measures the
connection stays healthy. The recorded note in `RtspTransport` names the second;
either way, the differential is what places the fault - the same stream over UDP
was intact, and switching that one camera's transport, with nothing else
changed, cleared it, while the depacketizer, the session deadline and the retry
schedule are shared by both paths. So the only variable left is the source's own
framing and send queue - not our client, and not the LAN between them. The box
here is a **relay in front of the cameras rather than a camera**, which is where
the search has to go when it comes back: a Surveillance Station restream is
software re-serving streams it did not encode.

**Why a mangled interleaved packet is not one lost packet.** Interleaved framing
is four bytes of header - `$`, a channel, and a two-byte big-endian length -
followed by exactly that many bytes, and there is no resynchronisation inside
the stream. A length that is wrong by even one byte moves the start of the frame
after it, and from there every frame is read from the wrong offset:
`read_packet` either reads up to 64 KiB of payload (the two-byte length
caps it) or takes
payload for header until the next `$` happens to line up. So one damaged byte
costs every picture after it, not one. A datagram is different: it arrives whole
or not at all, so a packet the network loses costs that packet alone, its
sequence number announces the gap, and the next datagram is still read from
where it begins. That is the whole trade - order and a firewall-proof path,
against a framing that a middlebox can shift under every picture that follows.

**A sequence gap would be a different source-side fault.** If the RTP numbers
themselves skip, TCP delivers that gap faithfully - the sender dropped those
packets, its send queue having overflowed or its real-time policy choosing a
skipped frame over a late one - and that is still not transport loss. The fix is
not the same either: UDP removes the relay's framing work, not its dropping
policy, so a stream that gaps for this reason may gap either way. The two are
worth telling apart before reaching for the switch - a gap in the sequence
numbers belongs to the sender, a picture the client cannot parse belongs to the
framing - and the recorded case here is the second.

**Why this is a setting and not a fallback.** The fallback of §16 runs one way,
UDP to TCP, and only for a handshake that is refused or goes silent: there the
session never came up, which is something the client can see. Here it comes up
and runs, and the damage exists only in the picture - no counter we keep can
tell "the source shifts the framing" from "this camera produces torn pictures"
without somebody watching. So the transport is chosen per camera and stored in
the configuration beside its address (`RtspTransport`, `toggled()` from the row's
`TCP` / `UDP` button), and it is offered only where there is an RTSP address to
pull; an MSE camera has one transport and nothing to switch.

**What UDP costs, and where it lives.** `setup_udp` binds a consecutive pair of
local ports and names them in `client_port`, then sends to the `server_port` the
answer returns; the RTP socket asks for a 4 MB receive buffer, because a
sixteen-channel wall arrives in bursts, and the RTCP socket is drained so its
reports do not pile up in the kernel. Only RTP reaches the caller. A reordered
datagram is written off as lost rather than held for its neighbours (§16), and
RTCP's lost and jitter fields start carrying the real numbers, which is what a
camera that adapts to receiver reports needs to hear. The two paths share
everything else, so a camera that runs better one way can be moved without
anything else about it changing.

## 34. Talking back: the track is offered only when the session asks, and go2rtc asks its first request

*Landed 2026-10-24, in the 1.5 line.*

**The back channel is one more media in the same SDP.** A camera that accepts
ONVIF Profile T audio from the client lists an audio track with `a=sendonly` -
the client sends, the camera receives - and a control of its own: `audioback` on
a camera, `trackID=N` behind go2rtc, which publishes it as
`media=audio,sendonly,codec=PCMU/8000`. The session negotiates it the way it
negotiates the video: `SETUP` that control with an interleaved pair, take the
pair the answer names (§20), and write RTP to that channel. The video path is
untouched, and a session that gets no such track simply has no talk-back. Only
the magnified session asks for one, because a camera may allow a single
back-channel client and the grid's sessions must not take it.

**The `Require` header works on a camera and not on go2rtc.** A client indicates
the capability with `Require: www.onvif.org/ver20/backchannel`, which is what a
camera that implements Profile T answers with the track. Against the go2rtc
restream it changes nothing: the track appears only when the stream URL carries
`backchannel=1` - the same stream whose `producers[].medias` already read
`audio, sendonly` - so a probe that only sets the header concludes there is no
back channel where there is one.

**Where go2rtc reads that query is the trap.** `internal/rtsp/rtsp.go` keeps
`c.URL` from the **first request of the connection** - our `OPTIONS` - and reads
`query := conn.URL.Query()` from it, and `reqTrackID` consults the same query.
So `backchannel=1` cannot be added at `DESCRIBE`: it has to be on the URL the
session opens with, before `OPTIONS`. And because `reqTrackID` reads a query
when there is one, the `SETUP` control URI must carry none, or the track is
looked up under the wrong id and the answer names another track's channels.
`RtspClient` therefore puts the flag on the `OPTIONS` URL and only there, and
`resolve_control` strips the query from the base before joining a relative
control. That this still holds is not something RTSP promises, so it is checked
against a live restream rather than assumed: `the_go2rtc_restream_still_offers_the_back_channel`
is an `#[ignore]`d test that connects to a go2rtc host named by
`XGVIEW_TALK_TEST_URL` and asserts the answer carries a `sendonly` audio media.
It is the test to run after a go2rtc upgrade, and its failure says the probe
assumption broke, not that the picture did.

**The header is session state, not a one-off.** `Require` is sent on the
`DESCRIBE`; a camera that took it is sent it again on the talk-back `SETUP` and
on the `PLAY`, because Profile T asks for it on the presentation as well and a
device expecting it there would otherwise open no back channel. A camera that
refused it is asked once more plainly, and the session then carries no header
at all - repeating what was already rejected would take the `PLAY` down and the
picture with it. `RtspClient::backchannel_require` holds which of the two this
session is. The `OPTIONS` that carries the query is tolerated the same way: a
`404` there is retried on the clean URL, and only a network failure is left to
the reconnect, so no part of the probe can end a session.

**The query is not free on a camera.** The first version left it on every
request and a Foscam direct (`192.168.27.40:88/videoMain`) answered `DESCRIBE`
with `404 Stream Not Found` - the parameter made the path a stream it does not
serve - so magnifying that camera lost the picture entirely. The flag is now
taken off again before the `DESCRIBE`, and the back channel is asked for the way
a camera reads it, with the `Require` header.

**Who puts the flag on the URL is part of the bug.** It came back a second time,
in the magnified view only, and the cause was that the channel manager appended
`backchannel=1` to the endpoint's own URL before opening the session: the client
then took the parameter for the caller's and stripped nothing, so it rode on
`DESCRIBE`, `SETUP` and `PLAY` and every direct camera but the one that tolerates
it answered `404`. The lesson is where the knowledge belongs - **only the RTSP
client knows which request reads the query and that it must be gone afterwards**,
so it both adds and removes it, and a URL that arrives carrying it is cleaned the
same way (`a_url_that_already_carries_the_flag_is_cleaned_before_the_describe`).
`Endpoint::talk` now carries the intent alone. The probe cost an afternoon
because the stand-alone probe tool passed a clean URL and therefore never
reproduced it: a probe has to take the same shape of input the product does.

A camera that will not take the header either - it answers `551 Option not
supported`, or `400` / `404` - is asked once more plainly, without the header,
and the session goes on with no back channel: the probe is allowed to fail, the
picture is not. What the SDP says is the answer, whichever request produced it.

**Sending is a clock, not a burst.** A 20 ms frame at 8 kHz mono is 160 samples:
the worker takes one from the queue every 20 ms, encodes it as G.711 (PCMU or
PCMA, whichever the track's `rtpmap` names) and writes it on the negotiated
channel, advancing the sequence number and the timestamp by 160 a packet. A queue
more than four frames behind is emptied instead of sent as a burst - the camera's
speaker wants the last 300 ms, not a backlog. Interleaved only: the back channel
is set up on the `TCP` transport, where the video's connection already carries
control.

**The track is fed continuously, silence included.** One packet a second was tried
while nobody talks, to save the 70 kbit/s of silence (1.5.33), and it was reverted:
with the audio going quiet after a call, some cameras stopped sending video too and
the session died of `no media data` thirty seconds later. `talk_gap` is therefore
the 20 ms frame rate on both arms today, and `TalkStream::silence_after` keeps the
timestamp on the wall clock rather than on the packet count, so the silence is the
camera's own speech path running - not a stream limping at a fiftieth of real time.
The cost is the 70 kbit/s while a camera with a back channel is magnified; the
alternative cost a black screen.

**What reaches the queue.** `cpal`'s input side runs at whatever rate and channel
count the device prefers; its callback mixes to mono and box-averages each
125 µs window - six input samples at 48 kHz, which is the anti-alias filter as
well as the downsample - and hands 16 bit samples on. Nothing is captured while
the switch is off, but the device stays open, so the start-up delay is paid once
rather than on every press.

**Android: the permission comes before the stream.** `cpal` reaches the
microphone through Oboe there, and Oboe refuses to open an input stream without
`RECORD_AUDIO` - a refusal that looks exactly like a box with no microphone. So
the manifest declares the permission (and `android.hardware.microphone` as not
required, because a television or a Shield has none), and the first press of the
talk switch asks the activity for it: `MainActivity.requestRecordAudio` answers
granted, or puts the system dialog up and answers `false`. The viewer says
"allow microphone access, then press it again" and the next press opens the
capture - the same shape as the storage permission an export asks for on Android
9. After that the two failures are distinct again: no permission (the dialog
path) and no device (`no microphone to talk into`).

**A call is full duplex, and the Audio tab can make it half duplex.** On the OPPO
the phone's own speaker came back through the microphone at a peak of 3003 while
the room sat at 150-300: a speaker beside a live microphone is a loop, and it
howls at some gain. Android's platform canceller is what takes it out there - the
capture and the playback both on the communication path, below - and a desktop's
speaker and microphone are far enough apart that the loop is not audible at a
normal volume, which is what `AppConfig::talk_mute` being off by default says.
The switch is for the machine where the two are close: ticked,
`call_mutes_speaker` silences the sound while the microphone is open - half
duplex, walkie-talkie style - and the far end is heard between calls rather than
during one. `sink.set_active(sound && !call_mutes_speaker())` in `apply_audio` and
`set_talking` is the whole of it; the sound switch itself is left as the viewer
set it, and the tab's hint says what the choice costs.

**Android: the capture and the playback are both the activity's, both on the
communication path.** `cpal` is no help for either half: an
`AcousticEchoCanceler` attaches to an `AudioRecord` session, and a media stream's
sound is not part of the canceller's reference signal - it only cancels what the
device plays as a voice call. So `MainActivity` opens both.

`startTalkCapture` opens an `AudioRecord` on `VOICE_COMMUNICATION`, attaches
`AcousticEchoCanceler` when `isAvailable()` says it can, and reads 16 bit mono
blocks on a thread of its own. The blocks come back through `nativeTalkPcm` into
`monitor_gui::mic::android_push`, which downsamples them with the same
`Downsampler` the `cpal` capture uses, rebuilt when a block reports a different
rate - so a phone recording at 44.1, 48 or 16 kHz reaches the camera as 8 kHz
either way. `startTalkPlayback` opens an `AudioTrack` with
`USAGE_VOICE_COMMUNICATION` and `CONTENT_TYPE_SPEECH` at the source's own rate (a
camera's audio is 8 or 16 kHz mono, and `AudioTrack` takes either, so nothing is
resampled here either), written non-blocking from the channel worker so a full
track drops a block rather than stalling the worker, which is also pumping the
picture. `monitor_gui::audio::Sink` is that track on Android and `cpal`'s stream
everywhere else, and the app only knows the name.

**The mode, the speakerphone, and the permission that makes them stick.** The
canceller is engaged by `MODE_IN_COMMUNICATION`, and in that mode a
voice-communication track is routed to the *earpiece* unless the speakerphone is
forced on - which is where the sound went for two rounds of this work, and why it
was barely audible at full volume. Both calls need
`android.permission.MODIFY_AUDIO_SETTINGS`, which the manifest did not declare:
`setMode` was refused with a line in logcat and nothing else, the mode stayed
`NORMAL`, and the track played into the earpiece. With the permission declared,
`enterTalkAudio` sets the mode and forces the speakerphone on, `leaveTalkAudio`
puts both back with the call volume it found, and the pairing is re-asserted when
the playback is opened as well, because the mode can be dropped between the
capture and the track. `startTalkCapture` answers -1 (the permission dialog is
up), -2 (no device) or 0/1 (opened, without/with the canceller), and the bar's
microphone is drawn from that answer - no guessing, no optimistic icon.

**The voice path is a call's, so the volume is too.** A voice-communication track
follows `STREAM_VOICE_CALL`, not the media volume the viewer turns up, and
`enterTalkAudio` puts that stream at its maximum for the call and
`leaveTalkAudio` restores it - which is what makes "the volume is already at
maximum" true on the path that matters. No makeup gain: with the routing fixed the
speaker is loud enough at 1.0, and a gain that clips a loud camera buys nothing.

**The hub's target is what turns the microphone on, and it has to be set.** The
capture pushes into `TalkHub` unconditionally while the viewer is talking, but
the worker only sends those samples when `talk.talking_on(index)` answers - and
that answers `target == Some(index) && talking`. The target is set by the
`ChannelCommand::Talk` handler in the pipeline, which is also what reopens the
sessions either side of a move; a version of that handler that only moved the
session's `talk` flag left the hub's target at `None`, so every packet went out
as silence and the camera never heard the viewer - one-way talk, from the day
the feature landed until it was noticed. The two must move together:
`talk.set_target(target)` sits beside `talk_target = target`.

**The back channel is fed continuously, not kept warm.** An idle talk channel was
tried at one packet a second to save the 70 kbit/s of silence (1.5.33), and the
cameras did not take it: after a call ended, the audio stopping was enough for the
camera to stop sending video, and thirty seconds later the session was reconnecting
with `no media data`. The 20 ms cadence is therefore kept while the back channel
is open, silence included; only the queue and the loop's budget are the picture's
business. See [`crate::pipeline`]'s `talk_gap`.

**The desktop still has no canceller.** WebRTC's AEC3 with the playback as its
reference signal is the step there - a C++ dependency and an audio graph that
keeps the two in step - and until it lands the half-duplex rule above is what the
desktop does.

**A camera can stall the picture with the talk-back's own transport, so the
transport never waits.** The talk-back shares the video's TCP connection, and a
write that blocks there blocks the loop that reads the picture: the camera stops
being read, the picture stops arriving, and the session reconnects. `RtspClient`
therefore queues interleaved frames and writes what the socket will take
(`flush_outbox`, polled non-blocking), dropping the oldest of a hundred frames
when the camera will not drain them, and RTCP goes out the same way. Audio is
worth less than the picture, and the picture is what the queue protects.
**Measured on the OPPO PBAM00** (Android 8.1, API 27, 2026-10-24). The wall was
started, the doorbell magnified, and the bar's microphone tapped: the permission
dialog came up, the grant was accepted, and the next press logged
`microphone opened rate=44100 channels=2` - Oboe took the device at its own rate
and the box-averaging brought it to 8 kHz. `AudioFlinger` shows the app's own
`RECORD` track on an input thread while it runs, and a temporary probe of the
pushed samples read peaks of 150-300 in a quiet room (so real audio, not a dead
silent stream). The session around it logged `talk-back track set up channel=4
codec=PCMU`. The SHIELD, which has no microphone, says so instead.

**A call takes the bar over.** While the microphone is open the strip carries one
control - a red square with the handset on it, the shape a phone draws on the key
that ends a call (two arcs: the receiver's back and its underside, not one line,
which read as a bracket at the size of a button) - and nothing else can be
pressed until it is pressed. The bar's idle clock is suspended for as long as the
call is up, because the one control that ends it would otherwise leave with the
strip eight seconds in; hanging up puts the fit, the sound switch and the
microphone back and the clock resumes from that press. Enter on the remote is
that press, and the selection is put back to the first control when the call
starts, since the fit it stood on before has gone.

**Idle cost, and the way it would go to nothing.** The back channel is acquired
with the session, not with the press: magnifying a camera whose media carries an
`audioback` track renegotiates that channel's session and SETUPs the track at
once, and from then on the camera's speaker is held open and the idle keepalive
above flows, whether or not anyone presses the microphone. Pressing it only
decides what goes into the packets. That is deliberate for now: the microphone
control is drawn from the answer the session really gave - no guessing, no
optimistic icon - and the press costs no negotiation at all, which is what makes
a press-to-talk button feel like a button. The price is the held channel plus the
continuous 70 kbit/s of silence while the picture is magnified - the idle rate
above is the spoken one, because the cameras wanted it that way.

The step that would remove the price is to acquire and release the *track* inside
the live session instead of reopening the session around it. The client already
has the pieces - `describe_backchannel` (the DESCRIBE that carries the ONVIF
header) and `setup_talk_interleaved` - and a TEARDOWN of the track's URL alone
leaves the picture's media untouched, so the video never blinks. The shape would
be: probe with the header when the camera is magnified and keep the SDP without
SETUPping the talk track (that answer is what can draw the microphone honestly),
SETUP the track on the press - one round trip, tens of milliseconds, no
renegotiation of the picture - and TEARDOWN it on hang-up or when the bar goes.
Cameras that refuse a mid-session SETUP would fall back to the present behaviour,
acquiring with the session. What keeps this out of the build for now is that it
moves a command into the pump loop that owns the client, and that a mid-session
SETUP after PLAY has to be verified against real cameras - the ones that answer
`551 Option not supported` are already known to need a fallback, and those are
exactly the ones a second SETUP would meet first.

**A bar that has gone is a call that has ended.** The strip is the call's whole
surface, so it is the call's existence: a press on the picture, `Back`, a pinch
or a wheel over the magnified picture, or a step to another camera all take the
bar down, and taking it down ends the call. That is one rule at the end of the
frame - `talking && !tile_bar.is_open()` - rather than each of those paths
remembering to hang up, which is what the earlier shape asked of them, and it is
why the idle has to be suspended rather than left to fire.

**The bar follows the viewport, whichever way the viewport moved.** Leaving the
magnification tears down the back channel and magnifying again reopens it, so the
switch disappears and returns with the session - and for a moment after
re-magnifying it can be missing until the new session reports its stats. That is
the same "appears when the stream says it can" rule as the sound switch; it is
not a failure. The one path that did not do this was **stepping the magnified
view** to the next or previous channel with an arrow or a swipe: `step_zoom`
moved the viewport and nothing else, so the sound stayed on the camera that was
left and the arriving camera's session was never asked for a back channel - its
bar carried no microphone at all, while entering the same camera from the grid
did. The talk target and the sink index are now moved by `apply_audio` on that
step as well, and it is the one place a new camera can enter the viewport
without going through `zoom_in`.

---

## 35. A call is a session of its own: the picture carries the picture, and RTP is not a heartbeat

*Landed 2026-10-25, in 1.5.47. Supersedes the last two paragraphs of §34: the
back channel is no longer taken when the picture is magnified, and the rule that
ends a call is no longer "the bar has gone".*

**What was wrong with the old shape.** The magnified session asked for the back
channel, set the `sendonly` track up and wrote a silence packet every 20 ms for
as long as the tile stayed magnified. Two costs, and the second is the one that
mattered: 70 kbit/s of nothing on a wall that may be magnified for hours, and a
camera's speaker held by a viewer who is not talking - a resource a camera may
only have one of. It also made the picture and the call the same session, so
anything that broke the call was a picture problem and anything that reopened
the picture was a call problem: magnifying reopened the session to add the
track, and stepping to the next camera reopened two.

**Three things replaced it, and each is independently testable.** The picture's
session asks for nothing (§34's header rules now apply only to the fallback
path). Whether a camera can be talked to is a question asked on a short
connection of its own: `OPTIONS` with `?backchannel=1`, `DESCRIBE` with the
Profile T header, the SDP read, the connection closed. A call is a third
connection, opened when the microphone is pressed and closed when it is released.

**The probe is `OPTIONS` and `DESCRIBE` and nothing else.** No `SETUP` means no
session identifier, which means there is nothing on the server to leave behind
and no `TEARDOWN` to send: the connection closing is the whole of the cleanup.
That is what makes it safe to ask a camera that is only being magnified, before
anyone has pressed anything - and it is why the answer is cached rather than
asked again on every frame.

**The answer belongs to an address, not to a camera or a channel.** A camera's
main and sub streams are two streams, and a back channel may be offered on one
of them; behind go2rtc the address is a stream name. So the cache is keyed by
the URL a call would be placed to, the interface resolves the magnified camera's
main stream to look it up, and a camera sitting in two slots is asked once. A
"no" expires after ten minutes, because a firmware or an account can change it;
a "yes" is kept for the run, because it is the answer the microphone is drawn on.

**Asking is not free of order.** The probe is sent after the schedule, never
before: magnifying reopens that channel on the main stream, and both commands go
down the same channel in the order they are sent. Sent first - from the same
place the talk target is set - the probe asks about the *sub* stream, the cache
key does not match the one the interface looks up, and the microphone never
appears. It is one line of ordering and it cost an afternoon.

**A call keeps itself alive, and this was the surprise.** The worker sends one
G.711 packet every 20 ms, and the first version of the stand-alone tool sent
exactly that and nothing else - the call died at thirty-two seconds with the
camera closing the connection under it. RTP is not a heartbeat: a camera times a
session out on its own clock and expects a *request*. `RtspClient::keep_alive`
sends a fire-and-forget `OPTIONS` at half the timeout the camera announced -
fifteen seconds on the D340W, thirty behind go2rtc - and a call then lasts as
long as the viewer wants it. The same worker reads the socket, because the
answers to those `OPTIONS` would otherwise pile up in the receive buffer and a
camera that has closed the session is only visible as a read that fails.

**Hanging up does not wait for an answer.** `TEARDOWN` is written and the
connection dropped: go2rtc does not answer it, and `RtspClient::teardown` waits
for the reply up to the client's own timeout, so a hang-up button took ten
seconds to do nothing. A session belongs to its connection as much as to its
identifier, so dropping the socket ends it either way - 283 µs, measured.

**Two sessions that must not interfere should not share one.** With the call on
its own connection, the picture's session can be thrown away and opened again
while the call keeps running, and a camera that closes one leaves the other
alone. That is now a mode of the stand-alone tool
(`talk_only --restart-picture N`) rather than a UI gesture, because the gesture
that used to force a reopen - opening a panel - is one the interface has since
made harmless. Both paths were measured on the fleet: forty-five seconds, the
picture's session dropped and reopened at ten, the call sent 2250 frames without
a gap.

**A camera that will not take a call gets the old arrangement.** `SETUP` of the
`sendonly` track alone, or the `PLAY` that follows, is refused by some devices -
they want the video set up first. That is a refusal of *this shape* of session,
not of the camera, and the picture's own session can still carry the media: the
worker hands the decision back to the supervisor, which reopens that channel with
`Endpoint::talk` set, and the old path feeds the audio as it always did. A
transport failure is the other case and gets no fallback - reopening would only
repeat it. The refusal is remembered for the run, so the next press goes straight
to the fallback instead of making the viewer wait for it again.

**The bar is the call, and a call makes the bar modal.** §34's rule - a bar that
has gone has ended the call, stated once at the end of the frame - was right
about the invisible call and wrong about everything else: a press on the picture,
a key that opens a panel, or reaching for the settings gear all took the bar
down, and with it the call. It is now the other way round: while the microphone
is open the bar stays where it is, a press that would put it away leaves it, a
panel opening over the wall no longer takes it down (the panel takes the focus,
not the strip), and a call ends on the handset, on `Esc` / `BACK`, or by leaving
the camera - which `set_target` already reaches from every direction.

**What the next protocol will need.** The channel is a trait with three methods
- how often, how many samples, and send - so a camera reached some other way
(Foscam's own TCP protocol is the one waiting, `talk/foscam.rs`) implements those
and nothing above it changes. The lifecycle deliberately stayed out of the trait:
`keep_alive` and `close` are asynchronous, an `async fn` in a public trait is not
object safe, and nothing here needs a second implementation to share them yet.

---

## 36. The speaker is chosen, and a stream that never asks for data is not kept

*Landed 2026-10-25, in 1.5.47.*

**A machine with two outputs has no "the" output.** A Debian kiosk here has a USB
audio dongle on card 0 and a television on HDMI at card 1 device 3. ALSA's
`default` is card 0, so the viewer played into a dongle with nothing plugged into
it while the picture was on the television - and there was no way to say
otherwise, because the sink asked `default_output_device()` and nothing else.
The settings panel now lists the platform's own devices next to a "system
default", stores the chosen name in the configuration, and reopens the sink when
it changes. The microphone gets the same list, for the same reason. Android is
left out: its capture runs on the voice-communication path the platform's echo
canceller needs, and there is no device there to name.

**The names are the platform's, and they go stale.** `plughw:CARD=PCH,DEV=3` on
Linux, `Speakers (Realtek(R) Audio)` on Windows, a device's own name on macOS.
None of them is stable against a driver update, a language change or a device
unplugged between two runs, so a name that no longer opens falls back to the
default and says so in the log. Storing an index instead would be worse: indices
move when a device is added.

**The list cannot be read while it is in use.** The first version asked the
platform for its devices on every frame, and on the kiosk the panel came back as
"the USB card and four HDMI ports" on start and then as two entries that it
stepped between and nothing else. An open device is exclusive: once the sink
holds `plughw:CARD=PCH,DEV=3`, the platform's enumeration no longer lists it, so
every frame was a list of everything *except* the device in use. The choice was
appended at the end to stay reachable, which made the next step from it wrap to
"system default" and then to the first real entry - a cycle of two. The list is
now read once, at start, before the sink opens anything, and kept; a device
plugged in later appears at the next start, which is when the sound is reopened
anyway. The lesson is not about audio: **an enumeration taken from a system that
is currently using one of the things it enumerates is not the enumeration you
want**, and reading it repeatedly turns a stable list into a moving one.

**A stream can be playing and write nothing at all.** This one cost an afternoon
and is worth writing down. On this kiosk the sound was silent with no error
anywhere: the PCM reported `state: RUNNING`, and `avail` grew by exactly 3 s of
frames per 3 s of wall clock - the hardware consuming, the application writing
nothing. The cause is in how ALSA reports readiness for a `dmix` PCM: `poll`
returns `POLLIN`, where a device backed by `hw:` or `plughw:` returns `POLLOUT`,
and `cpal`'s worker matches `Flags::OUT` exactly - anything else falls through to
`PollDescriptorsFlow::Continue`, which polls again and writes nothing. `default`
on a machine without PulseAudio or PipeWire is `dmix`, and that is exactly what a
minimal kiosk is. The measurements, from a `ctypes` probe that opens each device
the way `cpal` does:

| device | poll events | revents |
| --- | --- | --- |
| `default` (dmix) | 41 | 1 = `POLLIN` |
| `sysdefault` | 41 | 1 = `POLLIN` |
| `plughw:CARD=Audio,DEV=0` | 44 | 4 = `POLLOUT` |
| `hw:CARD=PCH,DEV=3` | 44 | 4 = `POLLOUT` |

So the sink counts callbacks and expects one *after* the stream has settled: a
device that keeps asking for data - every period, twenty-odd milliseconds - is
kept, and one that has stopped asking is dropped with a warning. That is a guard,
not a fix, and it does not catch this case on its own: the dmix stream here does
call back (the guard passes) and still writes nothing, so the write path is where
that one is lost. **The real answer is to choose the device** - `hw:CARD=PCH,DEV=3`
measured `avail` going *down*, which is a stream being fed - which is why the two
changes landed together.

---

## 37. A frame that runs long says so, and says what was on it

*Landed 2026-10-25, in 1.5.47.*

**The question "why does it stutter" is answered by a number, not by a look at
the code.** Two rounds of this were spent guessing: the settings panel felt slow,
the guess was the device enumeration, the enumeration was fixed, it still felt
slow, and the second guess - the label read on every frame - was right. Both were
found by reading, not by measuring, and the second one only after the first had
been ruled out by hand.

So the frame is timed. `XGVIEW_FRAME_MS` names a threshold in milliseconds:

```sh
XGVIEW_FRAME_MS=16 RUST_LOG=xgview::frame=info xgview
```

```text
INFO xgview::frame: a frame ran long ms=8.286897999999999 swallowed=48
     scene="wall settings=Streams channels=10 textures=10 decoder=ffmpeg"
```

`swallowed` is the frames that ran long inside the same second and were not
written - one line a second, with the count carried into the next, so a wall that
stutters on every frame cannot drown the log it is meant to explain. `scene` says
what was being drawn: `wall` or `magnified`, then any of `settings=<tab>`,
`layouts`, `blank`, `call`, and the counts that matter - channels, pictures on
the GPU, which decoder.

**What it measures is the frame, not the wait for the display.** The two clocks
are read at the top and the bottom of `update`, so the number is the work the
viewer does between two repaints. That is the part the viewer can do something
about; a frame that is short here and still arrives late belongs to the
compositor or the GPU, and no amount of work in this codebase will move it.

**Off by default, and cheap when off.** No threshold means `None` in the app and
one `Option` check a frame - the clocks are not read at all. The description is
built only for a frame that is going to be written, so the usual frame allocates
nothing for this.

**What it said, on the kiosk, ten pictures on the wall, threshold at 1 ms:**

| what was drawn | frames sampled | mean | max | repaints/s |
| --- | --- | --- | --- | --- |
| the wall | 11 | 2.85 ms | 5.46 ms | 42 |
| wall + settings, Display | 2 | 3.44 ms | 4.60 ms | 48 |
| wall + settings, Cameras | 14 | 3.66 ms | 7.32 ms | 46 |
| wall + settings, Streams | 51 | 3.81 ms | 8.29 ms | 47 |

The page that had been reading two files under `/proc` per row per frame now
costs about a millisecond a frame over the bare wall, against a budget of 16.6.
At a 16 ms threshold nothing at all was reported, on the wall or on that page.

The samples are one a second by design, so the table is a sample of frames rather
than a distribution - enough to tell "under the budget" from "over it", not enough
to say what the ninety-ninth percentile is. A run that wants the distribution
sets the threshold to 1 ms, which on this hardware reports very nearly every
frame, and keeps the log.

---

## 38. A maker's own protocol, and the one number that was wrong in the notes

*Landed 2026-10-25, in 1.5.52.*

**Why a second way in at all.** Nine Foscam cameras here, and the standard back
channel serves none of them: eight answer a `DESCRIBE` with no `sendonly` media,
and the ninth - the MGS5030, `.42` - offers it, negotiates the track, and then
plays silence. A camera agreeing to a back channel is not a camera with a
speaker behind it, which is why the maker's own protocol is preferred wherever
one is known: it is the only route measured to be *heard*.

**Which way in is decided from what the camera calls itself**, not from what it
answers: an `OPTIONS` comes back with no `Server` header at all, but the `401`
that answers a `DESCRIBE` carries the maker in its realm - `Foscam IPCam Living
Video` on eight of them, `Foscam` on the one with other firmware - and that
arrives before any credentials are sent, so identifying costs nothing extra. The
choice is a small pure function (`talk::route_from`), which is what makes it
testable without a camera.

**The protocol is TCP on the camera's own port**, the same 88 its RTSP lives on:
a `SERVERPUSH` request, a login block with the credentials in clear, and then
976-byte frames of 8 kHz 16-bit PCM every 60 ms. Nothing is encoded and nothing
is resampled - the talk hub already produces exactly the payload - so the channel
is a packetiser and a socket.

**The one number that was wrong.** The private notes said the login block carried
thirty-two bytes per credential. It carries **sixty-four**, and with thirty-two
the password lands sixteen bytes early, the camera answers `01` where a good
login answers `00`, and then drops every frame in silence. Two rounds of "the
tone is not audible" went into that; the fix is four bytes of arithmetic in a
struct.

**What finally settled it was a capture of the camera's own client**, and how to
get one is worth writing down because the obvious routes all fail: PCAPdroid's
adb control intents are ignored on 2.0.2, `run-as` needs a debuggable app, and
`/sdcard/Android/data/<package>` is unreadable to the shell. What works is
switching its dump mode to **HTTP server** in the app, starting the capture by
hand, and `curl -L http://<phone>:8080/` from the machine - it answers `302` to
`/PCAPdroid_<date>.pcap` and streams. The capture is `LINKTYPE_RAW`, which is
thirty lines of Python to walk.

**And the protocol has exactly one piece of feedback**, which is worth knowing
before reaching for a speaker: the fourth word of the login reply. `00 00 00 00`
is accepted, `01 00 00 00` is refused. Everything after that is silent whatever
happens - a wrong password looks exactly like a right one, and a frame is never
acknowledged. Which is why the settings gain a test tone: it is the only way a
person can tell whether the far end of this is a speaker or an empty jack.

## 39. A number is not an event: the statistics leave the channel

*Landed 2026-10-28, in 1.5.67.*

**What was wrong.** `StreamEvent::Stats` travelled on the same unbounded
crossbeam channel as the state changes, and the interface drained it in
`poll_events`, which runs on a repaint. A repaint is not a certainty: on Android
the app in the background stops being drawn, and on the desktop a covered window
does too. The channel went on filling meanwhile - one sample per channel every
`STATS_INTERVAL`, so eight a second on a 4x4 wall, each carrying a `camera_id`
`String` - and nothing bounded it. An hour away is 28,800 samples; a night is
more; a process that is never closed never stops.

**Why a bounded channel was not the answer**, though it is the obvious one. The
same channel carries `State`, and a bounded channel cannot drop *only* the
statistics. `try_send` would drop a state change when it mattered most - a
channel that failed while the app was away would come back saying it was still
running until the next transition - and `send` would park a runtime thread on a
full queue, which with an interface that is not draining is not a pause but a
stopped session.

**The rule the two kinds of message divide on**: a change has to be seen, a
measurement does not. `State`, `Transition`, `Talk` and `CallFailed` mark
something that happened once; a sample of the statistics describes the stream as
it is at the instant it is taken, and the next one replaces it whole. An
intermediate sample that nobody read is worth nothing, and the newest is the
entire truth. So the numbers left the channel and became a store -
`StatsStore`, next to `FrameStore`, doing what `FrameStore` already did for
pictures and for the same reason. A publish replaces the previous sample, so an
interface that stays away costs each channel one sample however long it stays
away, and a repaint that comes back reads the current numbers rather than a
backlog of old ones.

**Three details that are the whole of the correctness.**

The sample count is **global to the store, not per channel**. The interface
applies a sample when the count has moved, and the count has to keep rising
across a `clear` - a per channel counter that restarted at one would make a new
session's first sample look like one already applied, and the channel would show
nothing at all.

**A sample is dropped whenever the session that took it stops existing**: at the
end of an attempt, when a channel gives up, when it is suspended, and when its
task is aborted to be replaced - a switch, the decoding preference, a call
falling back to the picture's session. What it measured belonged to that
session, and the next one has its own codec, resolution and rate. Two of those
are worth their own line.

The **switch** is the one that shows a picture. `switching_from` is what keeps
the last frame of the stream being left on screen until the new one delivers
(`30.`), and a sample of the old stream applied after the transition ends that:
the numbers say `total_frames > 0`, which is the signal that the new stream has
data, and the held frame is let go for a black tile. Since the store has no
ordering against the channel, a sample published in the half second before the
switch is read after it - a quarter of all switches, not a race.

The **give-up** case is its own line because nothing will publish over that
sample again: a channel that failed would otherwise go on showing the rate it
had, for as long as it stayed failed.

**The interface reads the store after draining the events**, not before. The two
have no ordering between them - a sample published before a `State` may be read
after it - so reading last is what keeps a sample from a session that has just
ended from being applied over the state its ending set. That is also why the
`State` handler zeroing `fps` on `Connecting` still means something.

## 40. A rename is atomic, not durable: what a power cut does to the configuration

*Landed 2026-10-28, in 1.5.68.*

**What was there.** `AppConfig::save` wrote the JSON to `config.json.tmp` and
renamed it over `config.json`. The rename makes the *change* all-or-nothing,
which is the part everyone remembers, and it says nothing about whether either
file is on the disk - and neither the write nor the rename was followed by a
sync. A wall is a box that gets switched off at the mains rather than shut down,
so that gap is the normal way this program ends, not an exotic one.

**What a cut in that gap actually produces depends on the filesystem**, which is
why this went unnoticed for so long. ext4 has a heuristic for exactly this
pattern - `auto_da_alloc`, since 2.6.30 - which notices a write to a new file
followed by a rename over an existing one and forces the data blocks out before
the rename commits. Editors and configuration savers do this everywhere, so the
heuristic exists, and on a stock ext4 the outcome is the old file or the new one.
XFS, btrfs and the f2fs that Android uses under `/data` have no such rule: the
rename is a metadata operation and is committed on its own schedule, so a cut
after the commit and before the data is written leaves the name in place with
nothing behind it.

**An empty file is the outcome worth being afraid of**, not a stale one. `load`
reads the text, `from_json` fails on nothing at all, `load_or_default` logs and
starts on the defaults - the wall comes up with no cameras on it. The file is
deliberately left alone at that point ("it is the user's"), but the first
settings change saves the defaults over it, and by then there is nothing left to
recover. A stale file would have been recoverable; an empty one is not.

**The fix is two syncs, and they cover different losses.** `sync_all` on the
temporary file before the rename puts the bytes on the disk first, so the name
can never arrive ahead of its contents. The fsync of the *directory* afterwards
is what makes the rename itself durable: the name lives in the directory's own
data, and a cut before that reaches the disk takes the whole save back to the
file it replaced - the previous configuration, which is the good failure.

**Two platform notes.** A filesystem with no directory to flush - a FAT stick,
which `XGVIEW_CONFIG_DIR` lets a portable deployment point at - answers `EINVAL`
to that fsync; the file itself is already synced, so the error is treated as "as
durable as this filesystem gets" rather than as a failed save. On Windows a
directory cannot be opened as a file at all, so the renamed file is flushed
instead: NTFS keeps a file's name in its own record, and flushing it is the
durability the platform offers. It has to be reopened for writing to do that -
`File::open` gives a read-only handle and `FlushFileBuffers` refuses it with
`os error 5` - which is what the first version of this did, and what a `cargo
test` on the host caught that a `cargo check` inside WSL could not: the unix
branch is the only one that compiles there. **Run the platform's tests, not just
its type check**, when a `cfg` branch is the thing being added.

The cost is one fsync per save, and saves are debounced - `SAVE_DEBOUNCE` after
the last change - so it is not on the path of anything a person waits for.

## 41. The save keeps the version it replaced, and the load prefers it to the defaults

*Landed 2026-10-28, in 1.5.68.*

**Durability is a promise the platform can break.** The syncs in §40 are what
makes a save survive a cut on a filesystem that honours them; a filesystem that
ignores `fsync` - some SD cards and USB sticks, an Android phone whose vendor
tuned the flush away, a network mount - answers the call and does nothing. A
wall is also moved, cloned from an image, and copied off a drive that was pulled
out mid-copy. All of those end with a configuration that cannot be read, and the
previous behaviour was to answer that with the defaults, which is the one answer
that loses the cameras.

**So `save` keeps what it is about to replace**, as `xgvconfig.json.prev`, and
`load_or_default` answers an unreadable file with that copy instead of with the
defaults. The wall then comes back one settings change out of date, which is a
wall.

**The copy is written before the replacement, not after it.** Read the old file,
write the copy to `config.json.prev.tmp`, sync it, rename it onto `.prev`, and
only then write the new configuration. Keeping it afterwards would be simpler and
wrong: a cut between the two would leave a new configuration beside a copy of the
version before *that* one, and the copy would be the newer file. The order is
what makes `.prev` mean "one save ago" rather than "some other save".

**Two copy names, and they are not interchangeable.** The interface already kept
the configuration an import replaced, as `.json.bak`, before adopting a sealed
document. That is a different thing from a save's copy - it is written once per
import, by a different code path - and the load tries `.prev` first because a
save's copy is the newer of the two. This surfaced as a failing test: the harness
called `save` on the import copy, so the two paths kept overwriting each other's
file and the distinction the test was written to check could not exist. The
import copy now goes through `save_copy`, which is `write` without the
`keep_previous` step, and a copy of a copy is no longer litter beside the
configuration.

**The unreadable file is still not overwritten.** Recovery reads `.prev` and
returns it; the broken file stays on the disk, because it may be the user's own
edit and it is the only thing left that says what they meant. What the
configuration *is* at that moment is the recovered copy, so the next save writes
that over the broken file - which is the moment the old bytes stop being
recoverable, and the right moment for it.

**A copy that cannot be read is not a reason to fail the save.** `keep_previous`
logs and lets the save continue: a wall that cannot keep history is still better
than a wall that cannot save. The same for the fresh install, where there is
nothing to keep - `NotFound` is the expected answer, not an error.

## 42. One line ending for the whole tree

*Landed 2026-10-28, in 1.5.69.*

**What was there.** No `.gitattributes`, and `core.autocrlf=true` - which turns
out not to be this repository's setting at all, but the bundled PortableGit's
system config on the machine the tree was first cloned on. The index held LF for
everything, because `autocrlf` normalises on the way in, but twelve files were
CRLF in the blob *and* in the working copy, and the other 118 were LF in both.
The twelve were the audio-related sources (`monitor_codec/src/audio.rs`,
`ffmpeg_audio.rs`, `g711.rs`, `monitor_core/src/audio.rs`, `mse.rs`,
`reboot.rs`, the two `audio_live`/`audio_probe` examples, `monitor_gui/src/
audio.rs`), two `monitor_android` Java files and `bug_report.yml`.

**Why it matters more than it looks.** A clone made with `autocrlf=false` - a
Linux box, a colleague's git, a CI image - checks those twelve out as CRLF and
the rest as LF, and the split moves around with whoever touched the file last.
Editing a CRLF file in a tree where git expects LF is a whole-file diff that
says nothing about the change; the same file then looks different on two
machines for no reason anyone can find later. A `.gitattributes` travels with
the clone and overrides `core.autocrlf`, which is the only place this can be
decided once instead of per machine.

**`rustfmt.toml` with `newline_style = "Unix"` is not the fix**, and it is worth
being clear about why: it only covers `.rs`, only applies when rustfmt writes the
file, and `cargo fmt` is not run here - `cargo fmt --check` reports whole-file
differences across the tree, because the sources were never formatted with it.
The default, `Auto`, preserves whatever endings a file already has, so the option
only settles the endings for the day someone runs the formatter anyway or an
editor does format-on-save. It is in the tree for that, with a comment saying so,
and it is not what makes the tree LF. The `.gitattributes` is.

**The renormalise is two steps, not one.** `git add --renormalize .` rewrites the
blobs in the index; it does not touch the working copy, because an attribute
governs how git converts content, not what is already on disk. `git ls-files
--eol` keeps reporting `w/crlf` for the twelve until each file is checked out
again - `rm` them and `git checkout --` them, which writes through the new
`eol=lf` and produces LF. Skipping that leaves a tree whose index says LF and
whose files are still CRLF: clean to `git status`, wrong to every tool that reads
the bytes.

**How to prove the commit is only endings**: `git diff --cached
--ignore-cr-at-eol --numstat` reports `0 0` for every converted file. If a line
count shows up there, something else changed in the same commit.

**Nothing in the tree needed CRLF.** The six `.ps1` scripts were already LF and
run, and `assets/icons/windows/xgview.rc` was already LF and `rc.exe` compiles
it, so there was no file to carve an exception out for. The binary list in
`.gitattributes` is spelled out rather than left to `text=auto`'s NUL-byte sniff:
a keystore or an icon through a line-ending conversion is not a failure worth
leaving to a heuristic.

## 43. Two TLS implementations, and a tokio nobody used

*Landed 2026-10-28, in 1.5.70.*

**What was there.** `tokio = { features = ["full"] }` in three manifests. The
root package - the two binaries - names no tokio API at all: `src/main.rs` and
`src/bin/xgview-cli.rs` are `std::thread`, `std::process` and `std::time`.
Cargo unifies features across the graph, so that one line turned on `process`,
`signal`, `fs`, `io-std` and `parking_lot` for every target anyway, none of
which anything in the workspace calls.

**And rustls had two providers compiled in.** `monitor_core` asked for
`rustls = "0.23"` with the default features, which are `aws_lc_rs`; reqwest's
`rustls-tls` feature enables `__rustls-ring`, which pulls `ring`. Feature
unification does not pick one - it compiles both, and `cargo tree -i ring` and
`cargo tree -i aws-lc-rs` printed the same tree under each. The explicit
`aws_lc_rs` in `pipeline/ws_session.rs` was naming a provider that only half the
build was using.

**This was not a bug, and it is worth being exact about why not.** reqwest
builds its config with `ClientConfig::builder_with_provider` and falls back to
ring itself (`reqwest/src/async_impl/client.rs:765`), so nothing ever asked
rustls to guess. Had it called `ClientConfig::builder()`, rustls'
`get_default_or_install_from_crate_features` returns `None` when both features
are on and panics with "make sure exactly one of the 'aws-lc-rs' and 'ring'
features is enabled" - so the latent trap was there, and the explicit provider
in `pipeline/ws_session.rs` was likely what kept it from being found. The cost
paid was size and build time, not behaviour.

**Measured**, same source, same profile (thin LTO, `codegen-units = 1`,
`strip`), only the dependency lines differing between the two builds:

| | before | after | |
|---|---|---|---|
| `xgview.exe`, release | 22,005,760 | 20,594,176 | −1.35 MB (−6.4 %) |
| `libmonitor_android.so`, arm64 | 17,150,224 | 15,101,640 | −1.95 MB (−11.9 %) |

The APK is deflated, so the packaged saving is smaller than the library's; the
library is the number that is comparable across builds.

**The provider kept is ring, not aws-lc-rs**, for three reasons: reqwest already
wanted it, so the change is four lines in `pipeline/ws_session.rs` rather than
a `-no-provider` feature plus a process-wide `install_default()`; it is the
smaller of the two; and `aws-lc-sys` is built with cmake, so dropping it takes
cmake out of the build entirely - `cmake` and `fs_extra` left `Cargo.lock`
alongside `aws-lc-rs` and `aws-lc-sys`. What is given up is TLS 1.3's
post-quantum key exchange (`prefer-post-quantum` arrives with rustls' default
features and needs aws-lc-rs), which for a camera stream on a LAN is nothing.

**What tokio is asked for now**, and why each: `rt` (`spawn`, `JoinSet`,
`Handle`), `net`, `io-util`, `time`, `sync`, `macros` in `monitor_core`;
`rt-multi-thread` plus `net` and `time` in `monitor_gui`, because the runtime it
builds with `enable_all()` only has io and time drivers if those features are in
the build. The examples are `#[tokio::main]` programs and the tests use
`#[tokio::test]`, so `rt-multi-thread` and `macros` also appear in
`monitor_core`'s **dev**-dependencies - dev-dependencies do not reach the shipped
binary, and that is the only place a multi-thread runtime is needed on that side.

**How to keep it from coming back.** `cargo tree -i aws-lc-rs` must fail;
`cargo tree -e features -i tokio` lists the features that are actually on;
`cargo tree -d` for duplicates. The `ring` in `pipeline/ws_session.rs` is what
makes the choice visible at the point of use - with `default` features it is
possible to re-add a provider without touching a line of code.

**Left alone, deliberately.** `cargo tree -d` still reports duplicates, and they
are not all ours to fix: `calloop` 0.13/0.14, `smithay-client-toolkit` 0.19/0.20,
`rustix`, `linux-raw-sys`, `hashbrown`, `self_cell`, `miniz_oxide` and
`rustc-hash` come from winit and eframe disagreeing with themselves. `thiserror`
1/2 and `syn` 2/3 are pulled by crates we only consume. The three we could act
on are `getrandom` (0.2 ours, 0.3 from `ahash`, 0.4 from `uuid`), `base64`
(0.22 ours, 0.23 from `hyper-util`) and `webpki-roots` (0.26 from
`tokio-tungstenite`, 1.0 from reqwest); each is a few kilobytes, and the
`getrandom` bump changes the call in the sealed-configuration key path, which is
not worth re-testing for that.

## 44. The boot receiver is exported, and that is written down

*Landed 2026-10-28, in 1.5.71.*

**The finding.** `BootReceiver` is declared `android:exported="true"` with an
intent filter for `BOOT_COMPLETED`, `LOCKED_BOOT_COMPLETED` and the two
`QUICKBOOT_POWERON` variants. `BOOT_COMPLETED` is a protected broadcast, so no
third-party app can *send* it - but an intent filter only governs implicit
resolution. An explicit intent names the component and is delivered regardless,
so any app on the box can name `.BootReceiver` with any action and make the
viewer come to the foreground. `onReceive` reads the action to log it and does
not check it.

**The action check is not the fix, and it is worth saying why**, because it is
the obvious first idea. The action of the intent is chosen by the sender: an
app that wants this to happen sends `android.intent.action.BOOT_COMPLETED` and
an action allow-list is satisfied. Checking it would only reject senders who got
the action wrong, and nobody deliberately does that. The other half of the idea -
allow-listing the *sender* - has no API behind it: a manifest receiver is not
told the uid of whoever sent the broadcast, unlike a bound service where
`Binder.getCallingUid()` answers. There is nothing to compare against.

**The only real switch is `exported="false"`, and the documentation is
explicit** that it does not cost the system broadcasts: "If 'false', the only
messages the broadcast receiver receives are those sent by the system,
components of the same application, or applications with the same user ID." The
standard boot actions are sent by the system server and keep arriving.

**What stops it being false here is `QUICKBOOT_POWERON`.** The vendor variants
are sent by the ROM's own apps, and a vendor app that is not platform-signed
runs under its own uid, which a non-exported receiver refuses. The failure that
buys is a wall that does not come back after a power cut - the single thing this
receiver exists for, on exactly the boxes that need it most. That is not a trade
to make on the strength of a documentation page.

**What bounds the risk instead.** `onReceive` starts the package's launcher
intent and does nothing else: no configuration is read, nothing is written, no
privileged API is reached. The start is still subject to the Android 10+
background-activity-start restrictions, so on a box where XGView is neither the
launcher nor a `SYSTEM_ALERT_WINDOW` holder the attempt fails by itself. The
worst case is the wall appearing in front of whatever was on the screen.

**Where this is recorded**: the comment above the `<receiver>` in
`AndroidManifest.xml`, next to the attribute it is about, and a pointer to it in
`BootReceiver`'s javadoc that also says to keep the receiver's work bounded to
starting the launcher intent - that bound is what makes the trade acceptable. If
a box is ever confirmed to start through `BOOT_COMPLETED` alone, flipping the
attribute is the improvement, and the comment says so.

## 45. The boot receiver does not run before the user unlocks

*Landed 2026-10-28, in 1.5.71.*

**What was there.** `BootReceiver` was declared `android:directBootAware="true"`
and listed `LOCKED_BOOT_COMPLETED` beside `BOOT_COMPLETED` - the pair that looks
like "start as early as the platform allows", which is the right instinct for a
wall that has to come back after a power cut.

**It could not do that, for three reasons that all point the same way.** The
configuration lives in credential-encrypted storage - the entry point hands
`getFilesDir()` to `set_config_home` - so before the user unlocks there is no
camera list to draw. None of the three activities is direct boot aware, so the
`startActivity` below is refused by the system outright; the `catch` that logs
it is what ran. And had the start somehow succeeded, the viewer would have come
up on the defaults with no cameras, and would not re-read the configuration when
the user unlocked, because the process was already running.

**What it cost while it was there.** On a box with a lock screen, a failed start
and a `could not start the activity` line at every boot. On a box without one -
which is the normal TV box - the device is unlocked from the moment it boots, so
`LOCKED_BOOT_COMPLETED` and `BOOT_COMPLETED` arrive one after the other and the
receiver launched twice; the second was a no-op bring-to-front only because the
activity is `singleTask`.

**So both declarations are gone**, and the manifest now says why in the comment
above the receiver. What is left is the earliest moment the wall can actually be
drawn: `BOOT_COMPLETED`, after unlock. On a lockless box that is immediately
after boot, which is the case the receiver exists for; on a locked box it is
after somebody unlocks, which is later than the old code tried and the first
moment it could have worked.

**The other direction was considered and is worse.** Making direct boot real
means moving the configuration to device-protected storage, which is encrypted
with a device key rather than the user's credential - so the camera passwords in
it become readable to anything running before unlock, and the viewer would then
have to draw over the lock screen, which on a box that has a lock is precisely
what the operator does not want. A wall is not an alarm clock. If a deployment
needs the wall up before unlock, the answer is to take the lock off the box or
make XGView its launcher - not to weaken the storage the credentials sit in.

## 46. The earliest panic had nobody to restart it

*Landed 2026-10-28, in 1.5.71.*

**What was there.** `MainActivity.restartSoon` - the one path that brings the
wall back after a renderer panic - began with `final MainActivity self =
instance` and gave up when that was null, logging `no activity to restart with;
leaving the viewer down`. `instance` is assigned *after* `super.onCreate`, and
the native thread starts *inside* `super.onCreate`. A renderer that panics on
its first frame therefore panics in the one window where there is no activity
to ask, and the process exits with nothing scheduled to bring it back.

**On a wall that is the worst case, not an edge one.** Nobody is standing in
front of the box; a black screen stays black until somebody notices. Every other
failure on this path has a fallback - Vulkan to GL, GL to a bounded restart -
and this one had none.

**The fix is a context that exists earlier.** `onCreate` now takes
`getApplicationContext()` before `super.onCreate` and keeps it in `appContext`;
`restartSoon` uses the activity when there is one and that context when there is
not, and `mayRestart` takes the context rather than reaching for the activity.
Everything a restart needs - package manager, alarm service, shared preferences
- is reachable from it, and the restart limit still applies, so a driver that
panics on every launch still stops after three tries instead of looping.

**What this does not fix, and how to tell them apart.** A start that the system
refuses (Android 10+ background activity start with no exemption) and a wall
that comes up behind the launcher look the same from the outside - a process
that is running and a screen that is not showing it. Neither is a renderer
failure, so neither goes through `restartSoon`. The logcat line to look for is
`Background activity start ... blocked` from `ActivityTaskManager` for the
first, and `dumpsys activity activities` naming the launcher as the resumed
activity for the second.

## 47. Start on boot was a single shot in a race it could lose

*Landed 2026-10-28, in 1.5.71; the retry, and the two failures the retry
exposed, followed the same day.*

**The report was "sometimes the picture does not come up".** The receiver fires,
the process starts, and the wall is not on screen - sometimes. Two boots of the
same box, read out of the `events` buffer (`wm_on_resume_called`), say why:

```
        boot A                     boot B
13:19:52.143  tvlauncher   13:28:21.129  tvlauncher
13:19:52.942  Projectivy   13:28:21.829  Projectivy
13:19:53.433  Projectivy
13:19:53.868  xgview       13:28:21.908  xgview
```

Both boots started from `FallbackHome` (boot A at `13:19:48.692`, boot B at
`13:28:17.061`) and both had `BootReceiver` start the wall at the same point in
the sequence. The margins were 435ms in boot A and 79ms in boot B.

`Projectivy` is a third-party launcher (`com.spocky.projengmenu`) that starts
itself at boot with `CATEGORY_LEANBACK_LAUNCHER` from its own uid - twice in
boot A - and its accessibility service takes the foreground back when it sees
a `HOME` press. It is not the default home; `com.google.android.tvlauncher` is.
So the wall and that launcher are two independent boot launches with no order
between them, and the winner is whoever resumes last. The margins measured were
435ms and 79ms. A boot where the disk is busier or the native thread is slower
puts the wall second, and nothing looked again: `BootReceiver` starts the
activity once and `MainActivity` did not even override `onPause`, so being
covered was invisible to it.

**The fix is a bounded re-assert, not a fight.** `MainActivity` now keeps a
grace window open for `BOOT_GRACE_MS` (60s) when the launch carries the
`FROM_AUTOSTART` extra that `BootReceiver` already set - a manual open gets no
window, because covering it would be wrong. Inside the window, `onPause`
schedules `takeForegroundBack` 1.5s later; that re-sends the *launcher* intent -
`LauncherActivity`, the entry the boot receiver uses - and schedules the next
attempt itself, stopping at `BOOT_REASSERT_LIMIT` (2) attempts. Past the window,
or past the limit, a `HOME` press is the viewer's and is obeyed. Those two - the
window and the count - are the only bound; an earlier version also had one on
time on screen, and the paragraph after next is about why it went.

**`onUserLeaveHint` cannot tell a cover from a `HOME` press on this box.** It is
the documented way to know that the viewer left rather than being covered, so it
was implemented first: set a flag there, and have `onPause` obey instead of
answering. The device disproved it. A boot with nobody touching the remote:

```
14:11:32.948  xgview MainActivity started
14:11:35.581  Projectivy starts itself again (uid 10108, no input)
14:11:35.602  viewer left the wall        <- onUserLeaveHint fired
14:11:35.978  paused; leftByUser=true     <- so the wall let it stay in front
```

No key was pressed and no `keyevent` appears anywhere in the capture; the
launcher simply started itself and the callback fired anyway. Gating on it
therefore silenced the one case the whole mechanism exists for. It was removed
rather than fixed: there is no reading of that callback on this ROM that keeps
the case it was meant for.

**Time on screen was the next attempt, and it was worse than nothing.** The
replacement was `SETTLE_MS` (6s): `lastResumedAt` stamped in `onResume`, and a
pause within six seconds of it answered while anything later was left alone. The
reasoning was that the cover this exists for arrives about three seconds after
the wall appears - measured once, at `14:18:52.282` (`shown=2671`, answered by
`covered during boot (1/2)` at `14:18:53.824`, wall back afterwards). Three boots
after it landed:

```
wall resumed    +513ms over Projectivy   -> Projectivy back, seconds later
wall resumed    +789ms over Projectivy   -> Projectivy back, seconds later
```

In both the wall had been on screen for longer than six seconds by the time it
was covered, so `shown >= SETTLE_MS` and it obeyed - the wall stayed behind with
no attempt made. The number that was supposed to protect a `HOME` press also
suppressed the cover it was measured against, because "on screen for a while" and
"the viewer has seen it" are not the same thing when a launcher starts itself
late. Removed; the window and the count are the bound.

**The 1.5s delay is not padding.** Measured on the same box: `HOME` at
`13:26:53.625`, tvlauncher resumes at `53.854`, and Projectivy takes the
foreground at `54.989` - 1.1s *after* the press. Re-asserting as soon as the
wall was paused would have been overwritten by that. Waiting put the re-assert
at `55.203`, and the wall was back at `55.727`.

**Verified on the device**, all with `adb`, at the first pass's 3 in 120s:
covered once → back on top, log `(1/3)`; covered again → `(2/3)`; third time →
`(3/3)`; fourth → no log and the launcher stays. After the window expired,
`HOME` → the launcher stays and no re-assert runs. That run was read at the time
as the re-assert working; it was not. The same code was later shown to be a
no-op (see below), so what put the wall back in those three cases was the system
returning to the previous activity once the launcher's own start failed to take,
and the log line only coincided with it. What that run did establish is the
limit and the window: the fourth cover ran no attempt and the expired window ran
none either.

**The limits were then tightened to 2 in 60s.** On that box the three attempts
were spent inside 33 seconds (`13:26:55.203`, `13:27:10.791`, `13:27:27.875`),
which left the remaining 87 seconds of the window unguarded - the width bought
nothing and only lengthened the argument with a launcher that wanted the screen.
A minute covers the boot burst, which is over within seconds of the launcher
settling, and two attempts cover a launcher that takes the screen back once more
after we have answered.

**Re-sending the activity's own intent does nothing, and that was the bug.**
The first version re-sent `getIntent()`, reasoning that the activity is
`singleTask` so it would land as `onNewIntent`. Measured: two attempts, both
logged, both `START u0 {flg=0x34000000 cmp=com.xhbl.xgview/.MainActivity}`, and
neither produced a `wm_on_resume_called` - the activity was still there
(`visible=true`, still paused) and the launcher stayed in front. The activity is
already the root of its task, so the very flags that bring a task forward make
`ActivityStarter` read the start as "already at the top, deliver a new intent".
Starting the exported `LauncherActivity` instead does move the task forward, and
it also picks the right one of the two orientations. Verified: `(1/2)` at
`13:59:59.771`, `LauncherActivity` started at `59.779`, `MainActivity` at
`14:00:00.376`, wall back on top.

**An attempt that loses used to be the last one.** The trigger was `onPause`, and
a cover is one pause: an attempt that lost left the wall paused, so no further
`onPause` ever came and the second attempt was never scheduled - the limit read
"2" but meant "1". Measured at 3 in 120s: `(1/2)` at `13:49:24.186`, the
launcher's own start at `24.946`, and no `(2/2)` for the rest of the window.
`takeForegroundBack` now schedules the next attempt itself, which `onResume`
drops. Verified on the build before the intent fix, where an attempt still lost:
`(1/2)` at `13:57:04.513` and `(2/2)` at `13:57:06.055`, 1.5s apart with no pause
in between.

**The boot start itself can be dropped, and the re-assert cannot help.** One
boot in five measured: `Start proc ... for broadcast BootReceiver`, then
`START ... LauncherActivity` and the `SYSTEM_ALERT_WINDOW` allowance, and then
nothing - no `wm_create_activity`, no log line from the process at all, not even
the native side's first. Seven seconds later
`Killing ... (adj 975): empty #17`, alongside eight other processes killed the
same way. The boot was under load and the system logged
`Activity top resumed state loss timeout` and `Activity pause timeout` for the
previous home; the launch was lost in that, and the process was then reclaimed
as one with no components. There is no activity to re-assert from, so the
re-assert has nothing to offer here; the answer had to be a second start.

**`BootReceiver` looks again eight seconds later.** It records the boot in
shared preferences (`boot_at`, and `boot_retries = 0`), starts the wall, and
sets an alarm for `RETRY_DELAY_MS` (8s) carrying a `RETRY_START` broadcast to
itself. That retry compares `boot_at` with `resumed_at`, which
`MainActivity.onResume` writes on every resume: if the wall has been in the
foreground since the boot there is nothing to do and it says so
(`the wall came up <n>ms after the boot`), and if not it starts the wall again,
up to `RETRY_LIMIT` (2), rescheduling after each. The alarm is held by
`AlarmManager` rather than by the process, so it survives the process being
reclaimed - which is the whole failure it exists for. Verified on two real lost
starts: `the wall never came up after the boot; starting it again (1/2)`, then
`the wall came up 15430ms after the boot`; and `15689ms` in a later boot. The
alarm is due at `+8s` and the wall reached the foreground at `+15.4s`, so the
alarm itself fired several seconds late - `set` is inexact, and a device that is
still starting is the worst case for it.

**The retry can land in a process that has spent its event loop, and that used
to be the end of the app.** winit builds one `EventLoop` per process and refuses
a second (`EventLoop can't be recreated`), and a relaunch is handed to whatever
process is still cached rather than to a new one. What that left behind was not
a failed start but a dead shell: the activity was created, the native side gave
up, and the window neither drew nor closed. Nothing recovered it - not
`am start` from the shell, which found the same process each time:

```
14:44:19.847 START u0 {cmp=com.xhbl.xgview/.MainActivity} from uid 10122
14:44:19.887  7883  ERROR xgview: the viewer stopped
              error=cannot start the viewer: winit EventLoopError: EventLoop can't be recreated
14:44:27.705 START u0 {cmp=com.xhbl.xgview/.LauncherActivity} from uid 2000
14:44:27.754  7883  ERROR xgview: the viewer stopped            (the same, from the shell)
```

`run_android` now keeps a process-wide flag: a second `android_main` is logged
and the launch is handed to a fresh process through the `restartSoon` alarm the
panic fallback already uses, and the process exits either way. The exit is the
load-bearing half - a process that has spent its event loop cannot serve a
viewer however often it is asked - and the alarm is what brings the wall back
when it is not the top activity, which it usually is not when a launcher is in
front. Verified: a covered wall, then `am start`:

```
14:53:02.078  8736  the viewer has already been started in this process; it cannot be started again
14:53:02.078  8736  restarted 3 times within 10 minutes; leaving the viewer down
14:53:02.078  8736  no restart was scheduled; the next launch gets a process of its own
14:53:02.229  8843  marked this launch as a Vulkan attempt; the mark is cleared once the renderer is up
14:53:02.676  8843  the renderer came up on Vulkan; its attempt marker is cleared
```

Note that the restart budget was already spent and the recovery still happened:
the system restarts the process of a top activity that dies. The alarm covers
the other case.

**A failed start used to pin the box to the GL backend, which cannot start
here.** The attempt marker is written before a Vulkan attempt and is what sends
the next launch to GL - correct for a failed Vulkan attempt, wrong for a failed
*GL* one, because the marker is what chose GL. §9 has the measurement that makes
this fatal on this device: pinning wgpu to GL fails to start at all
(`WGPU error: Parent device is lost`). So one lost start - and the retry above
produces exactly those - was enough to pin the box to a backend that never comes
up, and every later launch chose GL because the marker was there, failed, and
left the marker behind.

How long that lasts is worth being exact about, because the first reading of it
was wrong. The marker is not permanent by design: `applyVulkanGate` drops it when
`Build.FINGERPRINT` or the app version changes (§19, "The attempt marker stopped
being permanent"). What made it stick here was that every install during this
work was the same version on the same ROM, so the escape hatch never fired. On a
box in the field the wall would stay down until the next release - recoverable,
and still unacceptable on a wall that has to come up on boot.

A failed GL start now drops the marker, so the next launch is Vulkan again. The
rule is `backend::failed_start_clears_marker` with the table and the invariant
beside the other two (`a failed start never leaves the next launch on GL`). A
device that alternates between the two backends is recoverable; one pinned to a
backend that cannot start is not. Verified on the stuck box: the launch after
the install still went to GL and failed, then

```
14:52:17.061  WARN xgview: the GL fallback could not start either; the next launch tries Vulkan again
```

and the next launch came up on Vulkan and drew all eight streams.

**The devices the marker exists for do not reach this rule.** It is worth
walking, because "a change to the fallback" is exactly where a device nobody has
in hand gets broken. The marker is there for §19's Adreno 506 and 512 on
Android 8.1 - Vulkan 1.0, a driver that is handed a device and loses it while
the first pipeline is built, and a GL driver that works - and for drivers the
version gate cannot name. Those devices answer `hasSystemFeature(
FEATURE_VULKAN_HARDWARE_VERSION, 0x401000)` with `false`, so they are behind the
gate and `decide` gives them `Gl(GlReason::Gate)`, which this rule does not
touch; a stale attempt marker under the gate is already dropped by
`Decision::clear_attempt_marker` before the question is asked. And what goes
wrong on them is a *panic* inside `create_render_pipeline`, caught by
`catch_unwind` and answered by `panic_action` - the other arm of the `match` in
`run_android`, which still keeps the marker so the next launch lands on GL.

The one cell that changes is "GL was chosen because of the marker, and GL could
not start". On those devices GL is the backend that works, so that cell is not
the expected outcome there; where it does happen the choice is one more Vulkan
attempt (one caught panic and a restart, capped at three in ten minutes) against
staying down for good.

**Measured on that device, 1.5.75.** The OPPO of §19 was put in front of the
work rather than reasoned about, and every claim above came out as stated:

- The gate answers first, as designed:
  `no Vulkan 1.1 on this device; the GL backend is marked for this launch` and
  `this device has no Vulkan 1.1; using the GL backend` - `Gl(GlReason::Gate)`,
  which the new rule does not match. Six channels at 15-22 fps on GL afterwards.
- `force-vulkan` still produces §19's sequence, unchanged: the panic is caught
  (`panicked wgpu error: Validation Error ... Parent device is lost`), the launch
  is marked, `the renderer panicked before it came up; the next launch uses the
  GL backend`, the process restarts and the wall is back on GL 5.2s later - with
  **no** `Fatal signal`, `Abort message` or backtrace from `libmonitor_android.so`
  anywhere in the log.
- The marker that attempt left behind never reaches the new rule either:
  `the version gate supersedes the previous attempt; its marker is cleared` is
  `Decision::clear_attempt_marker` doing what §19 says it does.

**A `HOME` press cannot be answered on this ROM, and that is worth knowing
before reading a log that looks like it was.** With the window armed, `HOME` gave
two attempts, both dropped by the system:

```
15:31:26.558  paused; shown=7924 graceLeft=51684 attempts=0
15:31:28.061  covered during boot; taking the foreground back (1/2)
15:31:28.065  START u0 {cmp=com.xhbl.xgview/.LauncherActivity} from uid 10138
15:31:28.071  W ActivityManager: Activity start request from 10138 stopped
15:31:29.575  covered during boot; taking the foreground back (2/2)
15:31:29.578  W ActivityManager: Activity start request from 10138 stopped
15:31:32.129  App Resumed - is running
```

The wall came back 2.5s after the last attempt with nothing else from us, so this
is the system returning to the previous activity - the same misreading §47
already records once. A `HOME` press puts the task into the stopped state, and a
start request from a task that is stopping is dropped rather than queued.

Covered by another application, the same build does answer, and on the first
attempt:

```
15:33:08.230  paused; shown=7769 graceLeft=52187 attempts=0
15:33:09.732  covered during boot; taking the foreground back (1/2)
15:33:10.299  App Resumed - is running
```

567ms, one attempt, and the resumed activity was the wall's rather than Settings'.
That is the case the mechanism exists for - a launcher or a media app starting
itself at boot - so the limitation on `HOME` costs nothing that was wanted, and
it is a second reason not to try to tell the two apart: on this ROM they are
distinguishable, and the distinction is exactly the one that works.

**ColorOS does not deliver `BOOT_COMPLETED` to an app that is not in its
autostart list, and the stopped flag is the reason.** Two boot tests on the OPPO
produced no `XGView.BootReceiver` line at all, with the wall down and the OPPO
launcher in front. What looked like two separate findings - the missing broadcast,
and a package that reported `stopped=true` again after the boot although it had
been launched (`stopped=false`) before the reboot - are one fact: the ROM
force-stops the app during boot when it is not whitelisted, and a stopped package
is excluded from every broadcast, this one included. An earlier reading of this
same measurement called the stopped-package exclusion a separate, ruled-out
cause. It was the cause.

Added to 手机管家 → 自启动 and rebooted from a clean `stopped=false`, the same
build comes up on its own:

```
15:45:21.862  Start proc 5084:com.xhbl.xgview/u0a138 for broadcast com.xhbl.xgview/.BootReceiver
15:45:22.095  START u0 {act=android.intent.action.MAIN cat=[android.intent.category.LAUNCHER]
              flg=0x34000000 pkg=com.xhbl.xgview cmp=com.xhbl.xgview/.LauncherActivity} from uid 10138
15:45:22.697  am_on_resume_called: [0,com.xhbl.xgview.MainActivity,LAUNCH_ACTIVITY]
15:45:22.826  am_activity_launch_time: [0,209187647,com.xhbl.xgview/.MainActivity,493,671]
```

835ms from the broadcast to `onResume`, no start refused, `stopped=false`
afterwards, nine channels on the hardware decoder at 15-21 fps. The retry alarm
fired once, eight seconds after the boot, and started nothing a second time:

```
u0a138:com.xhbl.xgview +5ms running, 1 wakeups, 1 alarms: *walarm*:com.xhbl.xgview.RETRY_START
```

One wakeup and no second `am_proc_start` is `retry` finding `resumedAt >= bootAt`
and standing down - the half of the mechanism that must not interfere, doing
nothing.

A boot test therefore needs two things that are easy to get wrong: launch the app
once after any `am force-stop`, which marks the package stopped, and check that
`stopped=false` holds immediately before the reboot. The flag the ROM sets during
boot is the same one, and until the broadcast fails there is nothing on the
outside that distinguishes the two.

**What this does not fix.** If the wall is not running at all - the earlier
snapshot of the same box had an empty `pidof com.xhbl.xgview` while Projectivy
was in front - then nothing here applies: that is a start that never happened
or a process that was killed afterwards, not a wall behind a launcher. The
`SYSTEM_ALERT_WINDOW` grant is also load-bearing for both paths: the log line
that proves the boot start was allowed at all is
`Background activity start for com.xhbl.xgview allowed because SYSTEM_ALERT_WINDOW permission is granted`.
Revoke it and the failure changes shape, from "covered" to "never started".

**Replacing the APK re-delivers `BOOT_COMPLETED`,** which is worth knowing
before reading a boot log taken after an install: `adb install -r` produced
`14:52:16.593 I/XGView.BootReceiver: received android.intent.action.BOOT_COMPLETED`
four minutes after the actual boot, and the wall came up from it. It is the
system giving a newly installed package the boot broadcast it missed, it is not
a bug in the receiver, and it means an update puts the wall back on screen by
itself.

## 48. A refused password was one of four failures wearing the same name

*Landed 2026-10-28, in 1.5.77.*

**The report was "the camera keeps reconnecting and the picture never comes
back".** The password in the form was wrong. What the tile said was
`reconnecting`, forever, and the log said `session failed` every thirty seconds
with the same line under it.

`CoreError::rtsp(...)` was carrying four different faults:

| Where it was built | What it meant |
| --- | --- |
| `rtsp.rs:1237` | the camera rejected the credentials, `401` |
| `rtsp.rs:1717` | the handshake timed out |
| `pipeline.rs:2188` | the picture stalled and the grace ran out |
| `pipeline.rs:2270` | no media arrived at all |

The line numbers are from the single `pipeline.rs` this was found in. 1.5.78 split
that file into a directory of seven; both stalls are in `pipeline/rtsp_session.rs`
now, one in `check_stall` and one where the wait runs out.

Nothing downstream could tell them apart, so nothing downstream could act
differently. `ReconnectPolicy::max_attempts` defaults to `0`, which means "retry
forever", so a rejected password was retried at the thirty-second ceiling for as
long as the viewer watched - and every attempt was logged the same way, with the
`401` buried in the message.

**It cost more than a spinner.** `talk_worker` read a plain `Rtsp` failure as the
camera refusing the *shape* of a call session, and reopened the picture's session
to carry the microphone on it instead. A camera with a wrong password refuses
that session too, so pressing the microphone blanked a picture that had been
fine.

**Failures carry a kind beside their message now** - `Auth`, `Refused`,
`Timeout`, `Unreachable`, `Protocol`, `Local` - and the channel acts on the kind
rather than on the text. `Auth` and `Local` stop it: the answer cannot change
with time, so the countdown would only delay the one line that says what is
wrong. Everything else is retried exactly as before.

The `Auth` message is carried without the `rtsp error: ` prefix the other
variants wear, because it is what the viewer reads on the tile and the tile shows
forty-six characters - `401 Unauthorized` has to be among them.

**Two things on the way back were wrong as well**, and both are older than this
change: neither could be reached until a channel could give up.

*Correcting the password reached nothing.* The plan a scheduler compares carried
the URL, the transport and the connection mode, but not the credentials, so an
edit produced no change, no `Activate`, and the channel kept the password it was
started with. What worked was a page turned away and back, because `Suspend`
drops the runtime and `Activate` builds a new one. The plan carries a fingerprint
of both logins now - a fingerprint and not the strings, because a plan is `Debug`
and lands in logs, and nothing reads the value, only whether it moved.

*The band that holds the last frame let go only on a sample from a session that
had produced a picture*, which a refused one never does. It stayed over the
failure panel for as long as the failure lasted, the one thing on the tile still
claiming that something was on its way. A channel that has given up lets go of
the frame: holding it is a promise.

## 49. Splitting seven files, and the five ways a scripted move lies

*Landed 2026-10-28, in 1.5.78 to 1.5.105.*

The refactor itself is recorded in `priv/docs/REFACTOR-PLAN.md`, which is kept in
the private repository rather than this one. What belongs here is what it cost.

**A move is only a move if nothing but position changed**, and the checks that
look like they prove it do not all do.

*The line multiset proves content, not order.* Sorting a function's lines into a
counter catches a lost line and says nothing about a reordered one - and order is
the whole of what `update` was. What works is to concatenate the new methods in
the order the caller invokes them and diff that against the old body: 733 lines
against 733, no difference, for the frame split.

*And the multiset is blind to an equal substitution.* It balances in total, and
only in total: a line dropped from one method and an identical line added to
another cancel, and the count comes back even with the code broken. The
`handle_keys` split read as seven lines missing and forty-six added - `});`,
three `return;`, `if typing {`, `if !nav_owns {`, `ctx.input_mut(|input| {` -
and the counter could say that the totals had moved and nothing about where
those seven had gone. Each was traced by hand to the method that now holds it.
A removed line is accounted for when it can be named where it went; the counter
is where the search starts, not where it ends.

*Filtering comments out of that check hides the damage.* A move script that
replaces identifiers does not know a comment from code, so `channels` became
`self.channels` inside a sentence - "Only the self.channels waiting out a
backoff care" - and inside an assertion message. Fifteen lines. The check that
would have caught it is the one that keeps comments in.

*Braces do not tell you where a block ends.* Counting `{` and `}` across a block
of Rust is fooled by a brace in a comment or a string, and `let keys =
ctx.input_mut(|input| { ... })` is a hundred and ten lines inside one call. What
works in this tree is the indentation of the closing line: a block opened at
eight spaces closes at exactly eight spaces plus a brace, and everything inside
is deeper. Assert that the block's own braces balance before writing anything.

*A block's start is its doc comment, and its end is the next block's start.* Cut
from the `fn` line and the previous block keeps the next one's `///`, which then
lands in the wrong file as a dangling doc comment - six of them, on the first
attempt at the pipeline split.

Two more that cost a round each. Anchors repeat: `if nav_owns {` appears at eight
spaces and again at sixteen inside a nested block, so a script must match the
whole line including its indentation. And a moved method needs `pub(super)` -
which is not a widening: a private method was visible to its module and
everything below it, and `pub(super)` from a child module says exactly that.

**The GUI split was the one that went cleanly**, because it moved methods between
files without touching a body: 132 methods, nothing missing, comments included.

**A modal is a payload, and being open is not a field.** Seven windows each
carried four fields - open, handoff, anchor, from - told apart by a suffix and
written out seven times. They are seven `Modal<T>` now, and `is_open()` is
`payload.is_some()`, so a window cannot say it is up while carrying nothing. The
two ends of every window - take the payload, draw it in a modal of a fixed width,
then put it back or close it and give the focus back - are one function, and what
is left in each window is the part in the middle.

That last one has a rule worth writing down, because it was got wrong twice: a
window that may still be **put back** copies its `from`, since it has to hand it
on to the next frame; whoever **gives the focus back** takes it. Taking it at the
top of a window that then decides to stay loses the button for the next frame,
and the focus never returns - which compiles, and looks like the window closing
correctly.
