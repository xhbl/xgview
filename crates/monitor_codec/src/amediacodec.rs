//! Android `AMediaCodec` binding (NDK media).
//!
//! The RTP access units are queued into an `AMediaCodec` opened in decoder mode.
//! Two output paths exist:
//!
//! - **Byte-buffer mode** (default): the codec is configured with a null
//!   window, and every decoded picture is handed back as a byte buffer through
//!   `AMediaCodec_getOutputBuffer`. The buffer is walked into the padded NV12
//!   planes the renderer uploads. On the Amlogic S905X5M box this path costs
//!   14-20 ms per 1080p frame inside `releaseOutputBuffer` alone — see
//!   `priv/docs/playback-decode-cost.md`.
//!
//! - **Surface mode** (`surface` feature): the codec is configured with an
//!   `AImageReader`'s `ANativeWindow`, and decoded pictures are acquired as
//!   `AImage` objects through `AImageReader_acquireNextImage`. The image's Y/U/V
//!   planes are read with `AImage_getPlaneData` and interleaved into the same
//!   NV12 planes. This is the path the box's own player uses, and the
//!   experiment is to measure whether the 14-20 ms disappears when the codec
//!   does not have to make its output CPU-visible.
//!
//! The surface path was tried once before and abandoned — it aborted on a
//! SHIELD (NVIDIA) within seconds when the process also drove a wgpu surface,
//! inside NVIDIA's sync fence handling (`fdsan`, `libnvrm_sync.so`). The box
//! this targets is Amlogic, not NVIDIA, so the abort is not expected to recur.
//! `docs/DevMemo.md` §9 holds the evidence from that attempt.
//!
//! The session is opened once, from [`VideoDecoder::configure`], and lives as
//! long as the channel does. A codec rebuilt for every packet never reaches a
//! steady state, and `low-latency` means nothing to a decoder created one packet
//! ago.

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_int, c_void, CString};
use std::ptr;
use std::time::{Duration, Instant};

use crate::audio::{AudioCodec, AudioDecoder, AudioStreamInfo};
use crate::layout::{Layout, shape_of};
use crate::sps::decode_parameter_sets;
use crate::{
    plane_stride, CodecError, ColorMatrix, ColorRange, ColorSpace, DecodedFrame, DecoderConfig,
    PixelFormat, Result, VideoDecoder, VideoStreamInfo,
};

/// Opaque `AMediaCodec*`.
type AMediaCodec = c_void;
/// Opaque `AMediaFormat*`.
type AMediaFormat = c_void;

/// `AMEDIA_OK`
const AMEDIA_OK: c_int = 0;
/// `AMEDIACODEC_BUFFER_FLAG_KEY_FRAME`
const BUFFER_FLAG_KEY_FRAME: u32 = 1;
/// `AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM`
#[cfg_attr(not(feature = "surface"), allow(dead_code))]
const BUFFER_FLAG_END_OF_STREAM: u32 = 4;
/// `INFO_OUTPUT_FORMAT_CHANGED`
const INFO_OUTPUT_FORMAT_CHANGED: isize = -2;
/// `INFO_TRY_AGAIN_LATER`
const INFO_TRY_AGAIN_LATER: isize = -1;

/// `COLOR_FormatYUV420Flexible`: the format a decoder offers when it is given no
/// surface to render into. What it hands over is NV12 in practice, with the row
/// pitch and plane height it reports as `stride` and `slice-height`.
const COLOR_FORMAT_FLEXIBLE: i32 = 0x7F42_0888;
/// `COLOR_FormatYUV420SemiPlanar`: the same layout under its older name.
const COLOR_FORMAT_SEMI_PLANAR: i32 = 0x15;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct AMediaCodecBufferInfo {
    offset: i32,
    size: i32,
    presentation_time_us: i64,
    flags: u32,
}

#[link(name = "mediandk")]
extern "C" {
    fn AMediaCodec_createDecoderByType(mime_type: *const c_char) -> *mut AMediaCodec;
    fn AMediaCodec_configure(
        codec: *mut AMediaCodec,
        format: *const AMediaFormat,
        window: *mut c_void,
        crypto: *mut c_void,
        flags: u32,
    ) -> c_int;
    fn AMediaCodec_start(codec: *mut AMediaCodec) -> c_int;
    fn AMediaCodec_stop(codec: *mut AMediaCodec) -> c_int;
    fn AMediaCodec_delete(codec: *mut AMediaCodec);
    fn AMediaCodec_getInputBuffer(codec: *mut AMediaCodec, index: usize, out_size: *mut usize)
        -> *mut u8;
    fn AMediaCodec_getOutputBuffer(codec: *mut AMediaCodec, index: usize, out_size: *mut usize)
        -> *mut u8;
    fn AMediaCodec_dequeueInputBuffer(codec: *mut AMediaCodec, timeout_us: i64) -> isize;
    fn AMediaCodec_queueInputBuffer(
        codec: *mut AMediaCodec,
        index: usize,
        offset: usize,
        size: usize,
        time_us: u64,
        flags: u32,
    ) -> c_int;
    fn AMediaCodec_dequeueOutputBuffer(
        codec: *mut AMediaCodec,
        info: *mut AMediaCodecBufferInfo,
        timeout_us: i64,
    ) -> isize;
    fn AMediaCodec_releaseOutputBuffer(codec: *mut AMediaCodec, index: usize, render: bool) -> c_int;
    fn AMediaCodec_flush(codec: *mut AMediaCodec) -> c_int;
    fn AMediaCodec_getOutputFormat(codec: *mut AMediaCodec) -> *mut AMediaFormat;

    fn AMediaFormat_new() -> *mut AMediaFormat;
    fn AMediaFormat_delete(format: *mut AMediaFormat);
    fn AMediaFormat_setString(format: *mut AMediaFormat, key: *const c_char, value: *const c_char);
    fn AMediaFormat_setInt32(format: *mut AMediaFormat, key: *const c_char, value: i32);
    fn AMediaFormat_setBuffer(
        format: *mut AMediaFormat,
        key: *const c_char,
        data: *const c_void,
        size: usize,
    );
    fn AMediaFormat_getInt32(
        format: *const AMediaFormat,
        key: *const c_char,
        out: *mut i32,
    ) -> bool;
}

/// Opaque `ANativeWindow*`.
///
/// Named in both modes because [`Self::open_session`] takes one either way -
/// `None` is what asks for byte buffers.
type ANativeWindow = c_void;

/// Opaque `AImageReader*`.
#[cfg(feature = "surface")]
type AImageReader = c_void;
/// Opaque `AImage*`.
#[cfg(feature = "surface")]
type AImage = c_void;

/// `media_status_t` - `AMEDIA_OK` is 0, the rest are non-zero.
#[cfg(feature = "surface")]
type media_status_t = c_int;

/// `AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE` - `AMEDIA_IMGREADER_ERROR_BASE`
/// (`-30000`) minus one. The ordinary "nothing has been rendered yet", as
/// opposed to a reader that failed.
#[cfg(feature = "surface")]
const IMGREADER_NO_BUFFER_AVAILABLE: media_status_t = -30001;

/// `AIMAGE_FORMAT_YUV_420_888` - the multi-plane Android YUV format the
/// AImageReader hands over when a decoder renders into its Surface. Three
/// planes: Y, U, V, each with its own row stride and pixel stride.
#[cfg(feature = "surface")]
const AIMAGE_FORMAT_YUV_420_888: i32 = 0x23;

#[cfg(feature = "surface")]
#[link(name = "mediandk")]
extern "C" {
    fn AImageReader_new(
        width: i32,
        height: i32,
        format: i32,
        max_images: i32,
        reader: *mut *mut AImageReader,
    ) -> media_status_t;
    /// The same reader, with the usage the buffers are allocated for. `format`
    /// and `usage` together decide which heap the producer's buffers come from,
    /// which is the thing the read-back speed turns out to depend on.
    fn AImageReader_newWithUsage(
        width: i32,
        height: i32,
        format: i32,
        usage: u64,
        max_images: i32,
        reader: *mut *mut AImageReader,
    ) -> media_status_t;
    fn AImageReader_delete(reader: *mut AImageReader);
    fn AImageReader_getWindow(
        reader: *mut AImageReader,
        window: *mut *mut ANativeWindow,
    ) -> media_status_t;
    fn AImageReader_acquireNextImage(
        reader: *mut AImageReader,
        image: *mut *mut AImage,
    ) -> media_status_t;
    fn AImage_delete(image: *mut AImage);
    fn AImage_getWidth(image: *const AImage, width: *mut i32) -> media_status_t;
    fn AImage_getHeight(image: *const AImage, height: *mut i32) -> media_status_t;
    fn AImage_getNumberOfPlanes(image: *const AImage, num_planes: *mut i32) -> media_status_t;
    fn AImage_getPlaneRowStride(
        image: *const AImage,
        plane_idx: i32,
        row_stride: *mut i32,
    ) -> media_status_t;
    fn AImage_getPlanePixelStride(
        image: *const AImage,
        plane_idx: i32,
        pixel_stride: *mut i32,
    ) -> media_status_t;
    fn AImage_getPlaneData(
        image: *const AImage,
        plane_idx: i32,
        data: *mut *mut u8,
        data_length: *mut i32,
    ) -> media_status_t;
    fn AImage_getTimestamp(image: *const AImage, timestamp_ns: *mut i64) -> media_status_t;
}

/// Opaque `ASurfaceControl*` - one compositor layer.
#[cfg(all(feature = "surface", feature = "probe"))]
type ASurfaceControl = c_void;
/// Opaque `ASurfaceTransaction*`.
#[cfg(all(feature = "surface", feature = "probe"))]
type ASurfaceTransaction = c_void;
/// Opaque `AHardwareBuffer*`.
#[cfg(all(feature = "surface", feature = "probe"))]
type AHardwareBuffer = c_void;

// The compositor path, which is the one every Android video player uses and
// this backend never did: the decoded picture is handed to a layer of its own
// instead of being read back into the process.
//
// It is behind `probe` because it is a measurement, not a feature: what it
// answers is whether the box can show 1080p at its own rate when nothing asks
// for the pixels, and what a layer above the wall's own window costs.
#[cfg(all(feature = "surface", feature = "probe"))]
#[link(name = "android")]
extern "C" {
    fn ANativeWindow_getWidth(window: *mut ANativeWindow) -> i32;
    fn ANativeWindow_getHeight(window: *mut ANativeWindow) -> i32;
    fn AImage_getHardwareBuffer(
        image: *const AImage,
        buffer: *mut *mut AHardwareBuffer,
    ) -> media_status_t;
    fn ASurfaceControl_createFromWindow(
        parent: *mut ANativeWindow,
        debug_name: *const c_char,
    ) -> *mut ASurfaceControl;
    // The layer lives as long as the process does - every channel presents
    // into the same one - so nothing releases it and this declaration is only
    // here for the sake of a caller that ever wants to.
    #[allow(dead_code)]
    fn ASurfaceControl_release(control: *mut ASurfaceControl);
    fn ASurfaceTransaction_create() -> *mut ASurfaceTransaction;
    fn ASurfaceTransaction_delete(transaction: *mut ASurfaceTransaction);
    fn ASurfaceTransaction_apply(transaction: *mut ASurfaceTransaction);
    fn ASurfaceTransaction_setVisibility(
        transaction: *mut ASurfaceTransaction,
        control: *mut ASurfaceControl,
        visibility: i8,
    );
    fn ASurfaceTransaction_setZOrder(
        transaction: *mut ASurfaceTransaction,
        control: *mut ASurfaceControl,
        z_order: i32,
    );
    fn ASurfaceTransaction_setPosition(
        transaction: *mut ASurfaceTransaction,
        control: *mut ASurfaceControl,
        x: i32,
        y: i32,
    );
    fn ASurfaceTransaction_setBufferTransparency(
        transaction: *mut ASurfaceTransaction,
        control: *mut ASurfaceControl,
        transparency: i8,
    );
    fn ASurfaceTransaction_setBuffer(
        transaction: *mut ASurfaceTransaction,
        control: *mut ASurfaceControl,
        buffer: *mut AHardwareBuffer,
        acquire_fence_fd: i32,
    );
}

/// The compositor layer, once it has been made.
#[cfg(all(feature = "surface", feature = "probe"))]
static OVERLAY_CONTROL: std::sync::atomic::AtomicPtr<c_void> =
    std::sync::atomic::AtomicPtr::new(ptr::null_mut());

/// The `ANativeWindow` the activity gave us for a decoded picture, once it
/// exists.
///
/// A codec configured with this renders into it and never hands a picture back:
/// the compositor scans the buffer out, and the pixels never enter this process.
/// That is the path every Android video player takes, and the reason none of
/// them care which box they run on. The wall keeps its own surface for the
/// interface, above this one, and leaves the picture's area unpainted.
static SURFACE_WINDOW: std::sync::atomic::AtomicPtr<c_void> =
    std::sync::atomic::AtomicPtr::new(ptr::null_mut());

/// Takes the surface a decoded picture is rendered into, or null when it went
/// away. See `SurfaceActivity` and `monitor_gui::android_surface`.
pub fn set_surface_window(window: *mut c_void) {
    SURFACE_WINDOW.store(window, std::sync::atomic::Ordering::Release);
    tracing::info!(
        target: "xgview::codec",
        present = !window.is_null(),
        "the surface a picture is rendered into came or went"
    );
}

/// Makes a compositor layer under the given window, and says whether it could.
///
/// This is the one arrangement that would let a decoded picture sit *under* the
/// wall without either surface having to be see-through: the layer is the
/// compositor's, not the view hierarchy's, so nothing has to blend. It answered
/// null for a `NativeActivity`'s window, which is what stopped it being used;
/// whether an ordinary activity's window differs is the question this asks.
#[cfg(all(feature = "surface", feature = "probe"))]
pub fn create_overlay(window: *mut c_void) -> bool {
    if window.is_null() {
        return false;
    }
    let control = unsafe {
        ASurfaceControl_createFromWindow(window as *mut ANativeWindow, c"xgview".as_ptr())
    };
    if control.is_null() {
        tracing::warn!(
            target: "xgview::codec",
            "this window offered no compositor layer"
        );
        return false;
    }
    let transaction = unsafe { ASurfaceTransaction_create() };
    unsafe {
        // SHOW, opaque, above the wall's own layer.
        ASurfaceTransaction_setVisibility(transaction, control, 1);
        ASurfaceTransaction_setBufferTransparency(transaction, control, 2);
        ASurfaceTransaction_setZOrder(transaction, control, 1_000_000);
        ASurfaceTransaction_setPosition(transaction, control, 0, 0);
        ASurfaceTransaction_apply(transaction);
        ASurfaceTransaction_delete(transaction);
    }
    OVERLAY_CONTROL.store(control, std::sync::atomic::Ordering::Release);
    tracing::info!(
        target: "xgview::codec",
        "this window offered a compositor layer"
    );
    true
}

/// Whether there is a surface a picture could be rendered into right now.
pub fn video_layer_active() -> bool {
    !SURFACE_WINDOW.load(std::sync::atomic::Ordering::Acquire).is_null()
}

/// Whether the probe was asked for its own compositor layer
/// (`debug.xgview.surface.overlay = 3`).
///
/// Superseded: the wall now has a surface of its own to hand a decoder, and this
/// is what the older probe used before that existed.
#[cfg(all(feature = "surface", feature = "probe"))]
pub fn overlay_wants_surface() -> bool {
    probe::number("debug.xgview.surface.overlay", 0) == 3
}

/// Whether the probe's window has to be see-through
/// (`debug.xgview.surface.hole = 1`).
#[cfg(all(feature = "surface", feature = "probe"))]
pub fn probe_translucent() -> bool {
    probe::number("debug.xgview.surface.hole", 0) == 1
}

/// False where there is no probe.
#[cfg(not(all(feature = "surface", feature = "probe")))]
pub fn probe_translucent() -> bool {
    false
}

/// How many pictures the decoder's pace is measured over.
///
/// One line per this many pictures, at debug: the numbers say whether a channel
/// is being held up by the codec's buffer hand-off or by the copy that reads the
/// picture back, and they are the same numbers on every device - which is what
/// makes two of them comparable. See `priv/docs/playback-decode-cost.md`.
const PACE_FRAMES: u32 = 60;

/// Measurement knobs, read from `debug.xgview.*` system properties.
///
/// Every one of them is a question the experiment has to ask, and asking it
/// through a property rather than a build is what makes the answer affordable:
/// one APK, `adb shell setprop`, restart, read the log. Nothing here is read in
/// a build without the `probe` feature, and nothing it does changes a shipped
/// decoder.
#[cfg(feature = "probe")]
mod probe {
    use std::ffi::{c_char, c_int, c_void, CStr, CString};
    use std::ptr;
    use std::sync::Once;
    use std::time::Instant;

    extern "C" {
        fn __system_property_get(name: *const c_char, value: *mut c_char) -> c_int;
    }

    /// Reads one `debug.xgview.*` property as a number, `fallback` when unset.
    pub fn number(name: &str, fallback: i32) -> i32 {
        let Ok(key) = CString::new(name) else {
            return fallback;
        };
        // `PROP_VALUE_MAX` is 92, and the call never writes more than that.
        let mut buffer = [0 as c_char; 92];
        let written = unsafe { __system_property_get(key.as_ptr(), buffer.as_mut_ptr()) };
        if written <= 0 {
            return fallback;
        }
        let text = unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_string_lossy();
        text.trim().parse().unwrap_or(fallback)
    }

    /// `AHardwareBuffer_Desc`.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct BufferDesc {
        width: u32,
        height: u32,
        layers: u32,
        format: u32,
        usage: u64,
        stride: u32,
        rfu0: u32,
        rfu1: u64,
    }

    /// Opaque `AHardwareBuffer*`.
    type Buffer = c_void;

    #[link(name = "android")]
    extern "C" {
        fn AHardwareBuffer_allocate(desc: *const BufferDesc, out: *mut *mut Buffer) -> c_int;
        fn AHardwareBuffer_describe(buffer: *const Buffer, out: *mut BufferDesc);
        fn AHardwareBuffer_lock(
            buffer: *mut Buffer,
            usage: u64,
            fence: c_int,
            rect: *const c_void,
            out: *mut *mut c_void,
        ) -> c_int;
        fn AHardwareBuffer_unlock(buffer: *mut Buffer, fence: *mut c_int) -> c_int;
        fn AHardwareBuffer_release(buffer: *mut Buffer);
    }

    /// `AHARDWAREBUFFER_FORMAT_Y8Cb8Cr8_420`
    const FORMAT_YUV420: u32 = 0x23;
    /// `AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN`
    const CPU_READ_OFTEN: u64 = 3;

    /// Times a copy out of a gralloc buffer this process allocated itself, and
    /// the same copy between two ordinary heap vectors.
    ///
    /// The pair is the whole point. The decoder's own buffer is one particular
    /// gralloc allocation on one particular device, and a number from it says
    /// nothing about which of the two - gralloc, or this device's reader - is
    /// slow. A buffer allocated here is the plainest gralloc there is; the heap
    /// copy is what the CPU can do when nothing is in the way. Between them
    /// they say whether 33 MB/s is the floor or an accident.
    pub fn read_bandwidth() {
        static ONCE: Once = Once::new();
        ONCE.call_once(run);
    }

    fn run() {
        const WIDTH: usize = 1920;
        const HEIGHT: usize = 1080;

        // The control: an ordinary heap copy of the same number of bytes.
        let bytes = WIDTH * HEIGHT * 3 / 2;
        let source = vec![7u8; bytes];
        let mut target = vec![0u8; bytes];
        let started = Instant::now();
        target.copy_from_slice(&source);
        let heap_ms = started.elapsed().as_secs_f64() * 1000.0;
        let heap_sink = std::hint::black_box(target[bytes / 2]);

        // And the same copy out of a gralloc buffer of our own.
        let desc = BufferDesc {
            width: WIDTH as u32,
            height: HEIGHT as u32,
            layers: 1,
            format: FORMAT_YUV420,
            usage: CPU_READ_OFTEN,
            stride: 0,
            rfu0: 0,
            rfu1: 0,
        };
        let mut buffer: *mut Buffer = ptr::null_mut();
        let allocated = unsafe { AHardwareBuffer_allocate(&desc, &mut buffer) };
        if allocated != 0 || buffer.is_null() {
            tracing::debug!(
                target: "xgview::codec",
                status = allocated,
                "the probe could not allocate a buffer"
            );
            return;
        }
        let mut actual = BufferDesc::default();
        unsafe { AHardwareBuffer_describe(buffer, &mut actual) };

        let mut address: *mut c_void = ptr::null_mut();
        let locked =
            unsafe { AHardwareBuffer_lock(buffer, CPU_READ_OFTEN, -1, ptr::null(), &mut address) };
        let (buffer_ms, buffer_bytes) = if locked == 0 && !address.is_null() {
            // A YUV 4:2:0 buffer is the luma plane plus half of it again, and
            // `stride` is in pixels.
            let length = actual.stride as usize * actual.height as usize * 3 / 2;
            let source = unsafe { std::slice::from_raw_parts(address as *const u8, length) };
            let mut target = vec![0u8; length];
            let started = Instant::now();
            target.copy_from_slice(source);
            let took = started.elapsed().as_secs_f64() * 1000.0;
            std::hint::black_box(target[length / 2]);
            unsafe { AHardwareBuffer_unlock(buffer, ptr::null_mut()) };
            (Some(took), length)
        } else {
            tracing::debug!(
                target: "xgview::codec",
                status = locked,
                "the probe could not lock its own buffer"
            );
            (None, 0)
        };
        unsafe { AHardwareBuffer_release(buffer) };

        let rate = |ms: f64, bytes: usize| -> u64 {
            if ms <= 0.0 {
                return 0;
            }
            (bytes as f64 / 1_000_000.0 / (ms / 1000.0)) as u64
        };
        tracing::debug!(
            target: "xgview::codec",
            heap_bytes = bytes,
            heap_ms = heap_ms,
            heap_mb_s = rate(heap_ms, bytes),
            buffer_bytes = buffer_bytes,
            buffer_ms = buffer_ms.unwrap_or(-1.0),
            buffer_mb_s = buffer_ms.map(|ms| rate(ms, buffer_bytes)).unwrap_or(0),
            stride = actual.stride,
            sink = heap_sink,
            "the probe: how fast this box reads memory"
        );
    }
}

/// `AMediaCodec` decoder.
///
/// In byte-buffer mode (the default) it reads pictures back through
/// `AMediaCodec_getOutputBuffer`. In surface mode (`surface` feature) it reads
/// them through `AImageReader_acquireNextImage` + `AImage_getPlaneData`.
#[derive(Debug, Default)]
pub struct AMediaCodecDecoder {
    /// The codec, alive from the first access unit to the end of the session.
    handle: Option<*mut AMediaCodec>,
    /// The image reader the codec renders into, when the `surface` feature is
    /// enabled. `None` in byte-buffer mode and before the session is opened.
    #[cfg(feature = "surface")]
    reader: Option<*mut AImageReader>,
    /// Pictures the codec has rendered into the surface but that have not been
    /// acquired yet, as `(pts_us, keyframe)` in the order they were rendered.
    ///
    /// In surface mode the picture arrives through the reader while the flag
    /// and the presentation time it was queued with arrive through
    /// `dequeueOutputBuffer`, and the two are separate calls: this is what
    /// pairs them back up. The queue is FIFO because the reader hands images
    /// over in the order the codec rendered them.
    #[cfg(feature = "surface")]
    pending: std::collections::VecDeque<(i64, bool)>,
    /// The compositor layer the probe presents into, once it has one.
    /// `debug.xgview.surface.overlay`: 0 read back as before, 1 hand the
    /// picture to the compositor, 2 take it and drop it - the last one is what
    /// the pixels cost when nothing at all asks for them.
    #[cfg(all(feature = "surface", feature = "probe"))]
    overlay_mode: i32,
    /// How many pictures the compositor was given. Reported beside `packed`
    /// so a session that is presenting rather than reading back still shows a
    /// rate; it stays zero on every path that does not present.
    stat_presented: u32,
    /// Pictures the codec has produced, counted whether or not they came back.
    ///
    /// A codec given a surface hands its pictures to the compositor and returns
    /// nothing, so the session that feeds it cannot tell a working stream from a
    /// stalled one by looking at the frames - this is what it counts instead.
    /// See `VideoDecoder::pictures`.
    pictures: u64,
    /// Configuration remembered from [`VideoDecoder::configure`], which is the
    /// only thing known before the stream is: the parameter sets the codec has
    /// to be opened with come with the first access unit.
    config: DecoderConfig,
    info: Option<VideoStreamInfo>,
    /// Colour description of the stream, read from the output format and used
    /// for every picture; the renderer's default until the codec announces its
    /// own.
    colorspace: ColorSpace,
    /// Picture size the codec announced for its output.
    width: u32,
    height: u32,
    /// Row pitch of a plane, and the number of rows the luma plane occupies.
    /// Both are the codec's to choose and are padding, not geometry.
    ///
    /// Byte-buffer mode only: a picture handed over as a buffer has the codec's
    /// padding around it, and these are what it is stripped with. An `AImage`
    /// carries its own row stride per plane, so surface mode never reads them.
    #[cfg_attr(feature = "surface", allow(dead_code))]
    stride: u32,
    #[cfg_attr(feature = "surface", allow(dead_code))]
    slice_height: u32,
    started: bool,
    /// Whether this session was configured with a surface to render into - a
    /// reader's or the activity's own.
    ///
    /// It decides how a decoded picture leaves the codec: rendered, and never
    /// read back, or copied into the planes the renderer uploads. It is a
    /// property of the session rather than of the build, because the wall runs
    /// both kinds side by side - nine sub streams on byte buffers and the one
    /// magnified picture on a surface of its own.
    on_surface: bool,
    /// Set once an output format this cannot read as NV12 has been reported.
    ///
    /// Byte-buffer mode only: with a surface the codec reports
    /// `COLOR_FormatSurface`, which is not a picture layout and is not warned
    /// about.
    #[cfg_attr(feature = "surface", allow(dead_code))]
    format_reported: bool,
    /// How long to wait for an input buffer before giving the access unit up.
    ///
    /// The codec holds its input buffers until it has worked through what it
    /// was given, so the wait is the codec's own pace - and it is a wait, not a
    /// delay: a buffer that is there is taken on the first, non-blocking try,
    /// and a machine that keeps up never waits at all. Giving the unit up
    /// instead is expensive: H.264 cannot be resumed past a picture it never
    /// saw, so the channel stays blank until the next key frame, seconds later,
    /// and a run of them is what the session's stall detector reads as a frozen
    /// stream. See [`crate::wait`], which is where the growth is decided.
    wait: Duration,
    /// The `color-format` the codec announced, `None` until it did.
    ///
    /// Byte-buffer mode only - see [`Self::format_reported`].
    #[cfg_attr(feature = "surface", allow(dead_code))]
    colour_format: Option<i32>,
    /// How this session's colour is laid out, decided on its first picture.
    ///
    /// The codec does not change the shape of its output mid-session, and the
    /// buffers it hands over are not all the same size, so the shape is read
    /// once from the first one and then trusted: deciding per buffer flips
    /// between two interpretations as the sizes vary, which is a picture whose
    /// colour changes from frame to frame.
    ///
    /// Byte-buffer mode only - see [`Self::stride`].
    #[cfg_attr(feature = "surface", allow(dead_code))]
    layout: Option<(Layout, usize)>,
    /// Whether the geometry of this session's pictures has been logged.
    ///
    /// The stride and slice height the codec chose are what the chroma plane's
    /// offset is computed from, and a picture that is right on one session and
    /// tinted on the next is a difference in these numbers.
    geometry_logged: bool,
    /// Where the time went, for the last [`PACE_FRAMES`] pictures: the wait for
    /// an input buffer and the drain that turns output buffers into planes.
    ///
    /// A picture that arrives late is either a codec that has not finished with
    /// the one before it or a copy that costs too much, and the two are fixed
    /// in different places. Reported rather than guessed at.
    /// Cached output buffer pointers, by buffer index: `(address, capacity)`.
    ///
    /// `AMediaCodec_getOutputBuffer` costs as much per picture as the release
    /// does on the Amlogic box, and the address it answers with for a given
    /// index is the codec's own buffer - the same memory every time that index
    /// comes round. Asking once per index rather than once per picture is worth
    /// 16 ms a frame there. The pool is thrown away whenever the codec says its
    /// output format changed, which is the one thing that reallocates it.
    #[cfg(feature = "probe")]
    output_buffers: std::collections::HashMap<usize, (usize, usize)>,
    /// Whether to keep those pointers - `debug.xgview.codec.cacheout`.
    #[cfg(feature = "probe")]
    cache_output: bool,
    stat_dequeue: Duration,
    stat_output: Duration,
    /// Time spent in `AMediaCodec_getOutputBuffer` alone.
    ///
    /// It is the one call in the byte-buffer drain that is neither a wait nor a
    /// copy, and it is deprecated in the NDK for being able to do real work: on
    /// the Amlogic box the drain's parts did not add up to the drain, and this
    /// was where the rest went.
    stat_get: Duration,
    stat_pack: Duration,
    stat_queue: Duration,
    stat_drain: Duration,
    stat_release: Duration,
    /// Time spent in `AImageReader_acquireNextImage` (surface mode only).
    /// Reported beside `release_ms` so the two paths can be compared.
    stat_acquire: Duration,
    /// Time spent asking an `AImage` for its planes - the three
    /// `AImage_getPlaneData` calls and the strides that come with them.
    ///
    /// Separate from the copy below because the two say different things: this
    /// one is the framework's price for making the buffer readable, and the
    /// copy is the price of the memory itself.
    stat_plane: Duration,
    /// Time spent in `AImage_getTimestamp` alone.
    ///
    /// It is not a field read: the framework may wait here for the picture to
    /// be finished with, and on the box this was measured on it was where a
    /// 1080p picture's time went.
    stat_stamp: Duration,
    /// Time spent copying the planes out of the image (surface mode only).
    stat_copy: Duration,
    /// Time spent reading planes out of an `AImage` (surface mode only).
    /// Reported beside `pack_ms` for the same reason, and is the sum of the
    /// two above.
    stat_image: Duration,
    stat_total: Duration,
    stat_frames: u32,
    stat_packed: u32,
}

// Safety: the codec is owned outright by the channel worker that opened it and
// is never touched from two threads at once; the renderer only ever sees the
// finished planes. The raw pointer is the only reason the type is not `Send` on
// its own.
unsafe impl Send for AMediaCodecDecoder {}

impl AMediaCodecDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens the codec session.
    ///
    /// In byte-buffer mode the codec is given no surface, so it hands every
    /// decoded picture back as a byte buffer. In surface mode (`surface`
    /// feature) an `AImageReader` is created and its `ANativeWindow` is passed
    /// to the codec, so decoded pictures are rendered into the reader's buffers
    /// and acquired as `AImage` objects.
    ///
    /// `access_unit` is the first one to reach the decoder, and it is what
    /// carries the parameter sets.
    ///
    /// It is opened from there rather than from `configure` because a session
    /// whose `SDP` announced no `sprop-parameter-sets` - which is what the two
    /// streams that failed on a Qualcomm decoder had in common - leaves neither
    /// a size nor the `csd-0`/`csd-1` that decoder insists on: it refuses the
    /// configuration with `-10000` rather than reading either from the bitstream.
    fn open(&mut self, access_unit: &[u8]) -> Result<()> {
        if self.handle.is_some() {
            return Ok(());
        }
        // Once per process, and before the first session: what this box's CPU
        // can read, and what it can read out of a gralloc buffer it allocated
        // itself. See [`probe`].
        #[cfg(feature = "probe")]
        probe::read_bandwidth();
        #[cfg(feature = "probe")]
        {
            self.cache_output = probe::number("debug.xgview.codec.cacheout", 0) != 0;
        }
        #[cfg(all(feature = "surface", feature = "probe"))]
        {
            self.overlay_mode = probe::number("debug.xgview.surface.overlay", 0);
        }
        let config = self.config.clone();
        let mime = CString::new(config.codec.mime())
            .map_err(|err| CodecError::Configure(err.to_string()))?;
        // The sets to configure the decoder with, and the picture size the
        // sequence one describes - H.264's SPS and PPS, or H.265's VPS, SPS and
        // PPS. What the stream describes beats what the session announced, which
        // is only a hint for the buffers the decoder sizes up front.
        let (csd, size) = decode_parameter_sets(config.codec, access_unit);
        let size = size.or(Some((config.width, config.height)).filter(|(w, h)| *w > 0 && *h > 0));

        // The layout is asked for rather than guessed: `YUV420Flexible` lets the
        // codec pick, and one here picks differently from one session to the
        // next. A codec that will not take the name is given the flexible one
        // and the shape is worked out from the buffer it hands over instead.
        //
        // Named first, and it costs nothing: asking for `0x15` and asking for
        // `0x7F420888` measure the same on the box that made this a question -
        // 44-57 ms a 1080p picture either way - so the one that decides the
        // colour is the one to ask for. See `priv/docs/playback-decode-cost.md`.
        //
        // In surface mode the colour-format is not asked for: the codec renders
        // into the AImageReader's Surface and the format is whatever the reader
        // was created with (`YUV_420_888`), not what `color-format` names.
        // The activity's own surface, when it has one. A codec given it renders
        // the picture into a buffer the compositor scans out, and nothing about
        // those pixels ever reaches this process - which on a box whose decoder
        // makes its output CPU-visible only at a price is the difference
        // between a picture at its own rate and one at three quarters of it.
        //
        // Only one session may have it: it is one surface, and nine decoders
        // rendering into it would be one picture drawn nine times over. The
        // grid's sub streams stay on their own path; the picture wide enough to
        // be worth the trouble is the magnified one, and a replay.
        if video_layer_active() && size.is_some_and(|(width, _)| width >= 1280) {
            let window = SURFACE_WINDOW.load(std::sync::atomic::Ordering::Acquire)
                as *mut ANativeWindow;
            if !window.is_null() {
                tracing::info!(
                    target: "xgview::codec",
                    "the codec is rendering into the activity's surface"
                );
                let opened = match Self::open_session(
                    &mime,
                    &csd,
                    size,
                    COLOR_FORMAT_SEMI_PLANAR,
                    Some(window),
                ) {
                    Ok(codec) => Ok(codec),
                    Err(err) => {
                        tracing::debug!(
                            target: "xgview::codec",
                            %err,
                            "the codec would not take a named layout, asking for a flexible one"
                        );
                        Self::open_session(&mime, &csd, size, COLOR_FORMAT_FLEXIBLE, Some(window))
                    }
                };
                return match opened {
                    Ok(codec) => {
                        self.handle = Some(codec);
                        self.started = true;
                        // Rendering into the activity's surface: the picture
                        // leaves the codec as a rendered frame, never as bytes.
                        self.on_surface = true;
                        if let (Some((width, height)), Some(info)) = (size, self.info.as_mut()) {
                            info.width = width;
                            info.height = height;
                        }
                        Ok(())
                    }
                    Err(err) => Err(err),
                };
            }
        }

        #[cfg(feature = "surface")]
        let (codec, reader) = {
            let (width, height) = size.unwrap_or((0, 0));
            let reader = Self::create_reader(width, height)?;
            let mut window: *mut ANativeWindow = ptr::null_mut();
            let status = unsafe { AImageReader_getWindow(reader, &mut window) };
            if status != AMEDIA_OK || window.is_null() {
                unsafe { AImageReader_delete(reader) };
                return Err(CodecError::Configure(format!(
                    "AImageReader_getWindow failed: status {status}"
                )));
            }
            let opened =
                match Self::open_session(&mime, &csd, size, COLOR_FORMAT_SEMI_PLANAR, Some(window)) {
                    Ok(codec) => Ok(codec),
                    Err(err) => {
                        tracing::debug!(
                            target: "xgview::codec",
                            %err,
                            "the codec would not take a named layout, asking for a flexible one"
                        );
                        Self::open_session(&mime, &csd, size, COLOR_FORMAT_FLEXIBLE, Some(window))
                    }
                };
            match opened {
                Ok(codec) => (codec, Some(reader)),
                Err(err) => {
                    // The reader owns the window the codec was offered, and a
                    // codec that never took it leaves nothing else holding it.
                    unsafe { AImageReader_delete(reader) };
                    return Err(err);
                }
            }
        };
        #[cfg(not(feature = "surface"))]
        let codec = match Self::open_session(&mime, &csd, size, COLOR_FORMAT_SEMI_PLANAR, None) {
            Ok(codec) => codec,
            Err(err) => {
                tracing::debug!(
                    target: "xgview::codec",
                    %err,
                    "the codec would not take a named layout, asking for a flexible one"
                );
                Self::open_session(&mime, &csd, size, COLOR_FORMAT_FLEXIBLE, None)?
            }
        };

        self.handle = Some(codec);
        #[cfg(feature = "surface")]
        {
            // A reader is a surface too: the codec renders into it, and the
            // pictures come back as images rather than as byte buffers.
            self.on_surface = reader.is_some();
            self.reader = reader;
        }
        self.started = true;
        // The size the stream describes is the one to report, not the one the
        // session guessed at before any of it arrived.
        if let (Some((width, height)), Some(info)) = (size, self.info.as_mut()) {
            info.width = width;
            info.height = height;
        }
        #[cfg(feature = "surface")]
        tracing::debug!(
            target: "xgview::codec",
            codec = config.codec.as_str(),
            "AMediaCodec session open, decoding into AImageReader Surface"
        );
        #[cfg(not(feature = "surface"))]
        tracing::debug!(
            target: "xgview::codec",
            codec = config.codec.as_str(),
            "AMediaCodec session open, decoding into byte buffers"
        );
        Ok(())
    }

    /// Creates an `AImageReader` for `YUV_420_888` pictures of the given size.
    ///
    /// `max_images` is kept small: the codec only needs one buffer in flight at
    /// a time for this pipeline's decode-then-read cadence, and a larger pool
    /// just holds memory. Five is the same count the framework's own
    /// `ImageReader` defaults to for a video-decode consumer.
    #[cfg(feature = "surface")]
    fn create_reader(width: u32, height: u32) -> Result<*mut AImageReader> {
        let mut reader: *mut AImageReader = ptr::null_mut();
        // The size the stream describes may not be known yet (width/height can
        // be 0 when the SDP carried nothing and the first AU has not been
        // parsed). Fall back to a generic size; the codec overrides the
        // reader's default once it announces its output format.
        let w = if width > 0 { width as i32 } else { 1920 };
        let h = if height > 0 { height as i32 } else { 1080 };
        // Which heap the reader's buffers come from is chosen by these usage
        // flags, and the copy out of them measured 33 MB/s against the codec's
        // own 850 MB/s - so the flags are worth asking about. Variant 0 is the
        // shipped behaviour (`AImageReader_new`, which means `CPU_READ_OFTEN`);
        // the rest are reachable only through the `probe` feature and a
        // `debug.xgview.surface.usage` property.
        #[cfg(feature = "probe")]
        let variant = probe::number("debug.xgview.surface.usage", 0);
        #[cfg(not(feature = "probe"))]
        let variant = 0;
        const CPU_READ_RARELY: u64 = 2;
        const CPU_READ_OFTEN: u64 = 3;
        const GPU_SAMPLED: u64 = 1 << 8;
        const COMPOSER_OVERLAY: u64 = 1 << 11;
        let usage = match variant {
            1 => Some(CPU_READ_RARELY),
            2 => Some(CPU_READ_OFTEN | GPU_SAMPLED),
            3 => Some(CPU_READ_RARELY | GPU_SAMPLED),
            4 => Some(CPU_READ_OFTEN | GPU_SAMPLED | (1 << 10)),
            // The compositor path needs the buffer to be usable as a layer:
            // `COMPOSER_OVERLAY` for the hardware planes and `GPU_SAMPLED_IMAGE`
            // for the ones the GPU has to composite itself.
            5 => Some(CPU_READ_OFTEN | GPU_SAMPLED | COMPOSER_OVERLAY),
            _ => None,
        };
        let status = match usage {
            Some(usage) => unsafe {
                AImageReader_newWithUsage(
                    w,
                    h,
                    AIMAGE_FORMAT_YUV_420_888,
                    usage,
                    5,
                    &mut reader,
                )
            },
            None => unsafe {
                AImageReader_new(w, h, AIMAGE_FORMAT_YUV_420_888, 5, &mut reader)
            },
        };
        if status != AMEDIA_OK || reader.is_null() {
            return Err(CodecError::Configure(format!(
                "AImageReader_new failed: status {status}"
            )));
        }
        tracing::debug!(
            target: "xgview::codec",
            width = w,
            height = h,
            usage = ?usage,
            "AImageReader created for YUV_420_888"
        );
        Ok(reader)
    }

    /// Creates, configures and starts one codec session, handing back the codec.
    ///
    /// `csd` is the parameter sets in the order the codec wants them, one per
    /// `csd-N` slot: H.264's SPS then PPS, or H.265's VPS, SPS and PPS.
    ///
    /// `window` is the `ANativeWindow` to render into, or `None` for byte-buffer
    /// mode. A codec that failed to configure is released before the error comes
    /// back, so a caller that wants to try again with a different format has
    /// nothing to clean up.
    fn open_session(
        mime: &CString,
        csd: &[Vec<u8>],
        size: Option<(u32, u32)>,
        colour_format: i32,
        window: Option<*mut ANativeWindow>,
    ) -> Result<*mut AMediaCodec> {
        let codec = unsafe { AMediaCodec_createDecoderByType(mime.as_ptr()) };
        if codec.is_null() {
            return Err(CodecError::Unsupported(format!(
                "AMediaCodec cannot create a decoder for {}",
                mime.to_string_lossy()
            )));
        }

        let format = unsafe { AMediaFormat_new() };
        unsafe {
            AMediaFormat_setString(format, c"mime".as_ptr(), mime.as_ptr());
            if let Some((width, height)) = size {
                AMediaFormat_setInt32(format, c"width".as_ptr(), width as i32);
                AMediaFormat_setInt32(format, c"height".as_ptr(), height as i32);
            }
            // The parameter sets themselves, which is what a decoder that will
            // not take a bare format is waiting for.
            for (slot, set) in csd.iter().enumerate() {
                let key = CString::new(format!("csd-{slot}")).expect("csd-N has no nul byte");
                AMediaFormat_setBuffer(format, key.as_ptr(), set.as_ptr().cast(), set.len());
            }
            // Deliberately no `low-latency`. A decoder told to run at low
            // latency hands its pictures over in the order it decoded them,
            // which for a stream with B-frames is not the order they are shown
            // in - the picture then jumps backwards, regularly, on every GOP.
            // Left alone, the codec reorders by the presentation times it was
            // given and hands them over to be shown, and a stream with no
            // B-frames still comes out one after another as it always did.
            //
            // `color-format` is a byte-buffer question only: it names the
            // layout of the memory the codec writes into. A codec given a
            // surface writes through gralloc and the reader decides the layout
            // (`YUV_420_888`), so naming one here would be a request about
            // memory that never exists.
            if window.is_none() {
                AMediaFormat_setInt32(format, c"color-format".as_ptr(), colour_format);
            }
            // Asked for only when a probe asks: a decoder that will not take it
            // fails the whole configuration rather than ignoring it, which is
            // why it is never on in a shipped build. On this stream - no
            // B-frames - the reordering it gives up cannot bite.
            #[cfg(feature = "probe")]
            if probe::number("debug.xgview.codec.lowlatency", 0) != 0 {
                AMediaFormat_setInt32(format, c"low-latency".as_ptr(), 1);
            }
        }

        tracing::debug!(
            target: "xgview::codec",
            mime = mime.to_string_lossy().as_ref(),
            width = size.map(|(width, _)| width).unwrap_or(0),
            height = size.map(|(_, height)| height).unwrap_or(0),
            csd = ?csd.iter().map(Vec::len).collect::<Vec<_>>(),
            "configuring the decoder"
        );
        // A null window is what asks for byte buffers; a non-null one renders
        // into the Surface the window wraps. `AMEDIACODEC_CONFIGURE_FLAG_ENCODE`
        // is 0 for the decoder direction.
        let window_ptr = window.unwrap_or(ptr::null_mut());
        let configured = unsafe { AMediaCodec_configure(codec, format, window_ptr, ptr::null_mut(), 0) };
        unsafe { AMediaFormat_delete(format) };
        if configured != AMEDIA_OK {
            unsafe { AMediaCodec_delete(codec) };
            return Err(CodecError::Configure(format!(
                "AMediaCodec_configure failed with status {configured}"
            )));
        }
        if unsafe { AMediaCodec_start(codec) } != AMEDIA_OK {
            unsafe { AMediaCodec_delete(codec) };
            return Err(CodecError::Configure("AMediaCodec_start failed".to_string()));
        }
        Ok(codec)
    }

    /// Reads the picture geometry and colour description the codec announced.
    ///
    /// Both sizes matter. The picture size is what the planes are cut to, and
    /// `stride` and `slice-height` are the padding the codec put around them:
    /// they are the codec's to choose and say nothing about the picture.
    fn read_output_format(&mut self, codec: *mut AMediaCodec) {
        let format = unsafe { AMediaCodec_getOutputFormat(codec) };
        if format.is_null() {
            return;
        }
        let mut width: i32 = 0;
        let mut height: i32 = 0;
        let mut stride: i32 = 0;
        let mut slice_height: i32 = 0;
        let mut standard: i32 = 0;
        let mut range: i32 = 0;
        let mut colour_format: i32 = 0;
        // Safety: each call only writes its out parameter, and `format` is live
        // until it is deleted below.
        let (has_width, has_height, has_stride, has_slice, has_standard, has_range, has_colour) =
            unsafe {
                (
                    AMediaFormat_getInt32(format, c"width".as_ptr(), &mut width),
                    AMediaFormat_getInt32(format, c"height".as_ptr(), &mut height),
                    AMediaFormat_getInt32(format, c"stride".as_ptr(), &mut stride),
                    AMediaFormat_getInt32(format, c"slice-height".as_ptr(), &mut slice_height),
                    AMediaFormat_getInt32(format, c"color-standard".as_ptr(), &mut standard),
                    AMediaFormat_getInt32(format, c"color-range".as_ptr(), &mut range),
                    AMediaFormat_getInt32(format, c"color-format".as_ptr(), &mut colour_format),
                )
            };
        unsafe { AMediaFormat_delete(format) };

        // A new output format is a new buffer pool, so every address kept for
        // the old one is stale.
        #[cfg(feature = "probe")]
        self.output_buffers.clear();

        if has_width && width > 0 {
            self.width = width as u32;
        }
        if has_height && height > 0 {
            self.height = height as u32;
        }
        if has_stride && stride > 0 {
            self.stride = stride as u32;
        }
        if has_slice && slice_height > 0 {
            self.slice_height = slice_height as u32;
        }
        if has_standard {
            self.colorspace.matrix = match standard {
                2 | 4 => ColorMatrix::Bt601,
                6 => ColorMatrix::Bt2020,
                _ => ColorMatrix::Bt709,
            };
        }
        if has_range {
            self.colorspace.range =
                if range == 1 { ColorRange::Full } else { ColorRange::Limited };
        }
        self.colour_format = has_colour.then_some(colour_format);
        // Only byte-buffer mode has a colour format this reads: a codec given a
        // surface reports `COLOR_FormatSurface`, which is not a picture layout
        // at all - the images come out of the reader in `YUV_420_888` instead.
        #[cfg(not(feature = "surface"))]
        {
            let nv12 = !has_colour
                || colour_format == COLOR_FORMAT_FLEXIBLE
                || colour_format == COLOR_FORMAT_SEMI_PLANAR;
            if !nv12 && !self.format_reported {
                self.format_reported = true;
                tracing::warn!(
                    target: "xgview::codec",
                    colour_format,
                    "the codec's output is not a format this reads as NV12"
                );
            }
        }
    }

    /// Releases every output buffer the codec has ready, packing the pictures it
    /// hands over.
    ///
    /// Every buffer has to be taken and given back: a buffer the codec hands out
    /// is not returned until it is released, and there are only a handful of
    /// them. That is true in both modes - what differs is what the release does
    /// with the picture. In byte-buffer mode it is thrown away after the copy;
    /// in surface mode `render = true` is what pushes it into the image reader.
    fn drain(&mut self, frames: &mut Vec<DecodedFrame>) {
        let Some(codec) = self.handle else {
            return;
        };
        let whole = Instant::now();
        self.drain_inner(codec, frames);
        self.stat_drain += whole.elapsed();
    }

    fn drain_inner(&mut self, codec: *mut AMediaCodec, frames: &mut Vec<DecodedFrame>) {
        let Some(_) = self.handle else {
            return;
        };
        loop {
            let mut info = AMediaCodecBufferInfo::default();
            let waiting = Instant::now();
            let index = unsafe { AMediaCodec_dequeueOutputBuffer(codec, &mut info, 0) };
            self.stat_output += waiting.elapsed();
            match index {
                INFO_TRY_AGAIN_LATER => break,
                INFO_OUTPUT_FORMAT_CHANGED => {
                    self.read_output_format(codec);
                    continue;
                }
                index if index >= 0 => {
                    if self.on_surface {
                        // A codec rendering into a surface hands its picture
                        // over by rendering it - there is nothing to read back
                        // here. A surface buffer's `size` is not the picture
                        // either: some devices report zero for every one of
                        // them. What is not a picture is the end-of-stream
                        // marker, and that is skipped.
                        //
                        // The entry remembered here pairs the flag and the
                        // moment with the image that will arrive through the
                        // reader. A codec rendering into the activity's own
                        // surface hands over no image at all, so nothing is
                        // remembered for it - a queue that grew for pictures
                        // nobody will take is a leak.
                        #[cfg(feature = "surface")]
                        if self.reader.is_some() && info.flags & BUFFER_FLAG_END_OF_STREAM == 0 {
                            self.pending.push_back((
                                info.presentation_time_us,
                                info.flags & BUFFER_FLAG_KEY_FRAME != 0,
                            ));
                        }
                    } else {
                        if info.size > 0 {
                            #[cfg(feature = "probe")]
                            let known = if self.cache_output {
                                self.output_buffers.get(&(index as usize)).copied()
                            } else {
                                None
                            };
                            #[cfg(not(feature = "probe"))]
                            let known: Option<(usize, usize)> = None;
                            let (buffer, capacity) = match known {
                                Some((address, capacity)) => {
                                    // Not timed: there is no call to time, and
                                    // that is the whole point of the cache.
                                    (address as *mut u8, capacity)
                                }
                                None => {
                                    let mut capacity = 0usize;
                                    let fetching = Instant::now();
                                    let buffer = unsafe {
                                        AMediaCodec_getOutputBuffer(
                                            codec,
                                            index as usize,
                                            &mut capacity,
                                        )
                                    };
                                    self.stat_get += fetching.elapsed();
                                    #[cfg(feature = "probe")]
                                    if self.cache_output && !buffer.is_null() {
                                        self.output_buffers
                                            .insert(index as usize, (buffer as usize, capacity));
                                    }
                                    (buffer, capacity)
                                }
                            };
                            if !buffer.is_null() {
                                let packing = Instant::now();
                                let frame = self.pack(
                                    buffer,
                                    capacity,
                                    info.presentation_time_us,
                                    info.flags & BUFFER_FLAG_KEY_FRAME != 0,
                                );
                                self.stat_pack += packing.elapsed();
                                self.stat_packed += 1;
                                if let Some(frame) = frame {
                                    frames.push(frame);
                                }
                            }
                        }
                    }
                    // Nothing is rendered in byte-buffer mode, so there is
                    // nothing to render now. A codec given a surface is the
                    // other way round: the render *is* the hand-off, and this
                    // is the call the box spends 14-20 ms in when there is no
                    // surface to render into.
                    let render = self.on_surface && info.flags & BUFFER_FLAG_END_OF_STREAM == 0;
                    let releasing = Instant::now();
                    unsafe { AMediaCodec_releaseOutputBuffer(codec, index as usize, render) };
                    self.stat_release += releasing.elapsed();
                    if info.flags & BUFFER_FLAG_END_OF_STREAM == 0 {
                        // A picture came out, whether it was read back or handed
                        // to the compositor. The session that feeds this decoder
                        // counts these; see `VideoDecoder::pictures`.
                        self.pictures = self.pictures.wrapping_add(1);
                    }
                }
                _ => break,
            }
        }
        // The release above is what puts the picture into the reader, so the
        // images can only be taken once every buffer has been handed back.
        #[cfg(feature = "surface")]
        self.acquire_images(frames);
    }

    /// Takes every image the codec has rendered into the reader.
    ///
    /// Called after the output buffers have all been released, because a buffer
    /// that has not been released with `render = true` has not reached the
    /// surface yet. An image that cannot be read as NV12 is deleted and dropped,
    /// which returns its buffer to the reader's pool - leaving one acquired
    /// would starve the reader after `max_images` frames.
    #[cfg(feature = "surface")]
    fn acquire_images(&mut self, frames: &mut Vec<DecodedFrame>) {
        let Some(reader) = self.reader else {
            return;
        };
        loop {
            let mut image: *mut AImage = ptr::null_mut();
            let acquiring = Instant::now();
            let status = unsafe { AImageReader_acquireNextImage(reader, &mut image) };
            self.stat_acquire += acquiring.elapsed();
            if status != AMEDIA_OK || image.is_null() {
                // `AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE` is the ordinary "not
                // yet" and every other status is a reader this cannot use;
                // either way there is nothing more to take this round.
                if status != IMGREADER_NO_BUFFER_AVAILABLE && status != AMEDIA_OK {
                    tracing::debug!(
                        target: "xgview::codec",
                        status,
                        "the image reader handed back no image"
                    );
                }
                break;
            }
            let reading = Instant::now();
            // The probe's two no-readback modes. Mode 1 hands the picture to a
            // compositor layer, mode 2 drops it on the floor; the difference
            // between them and the normal path is the whole cost of asking the
            // CPU for pixels.
            //
            // Mode 3 is not one of them: it is the *session* that renders into
            // the activity's surface, and such a session has no reader at all -
            // it never reaches this code. A session that does have one still
            // reads its pictures back, which is how the wall's tiles keep their
            // pictures while the magnified viewport is on the video layer.
            #[cfg(all(feature = "surface", feature = "probe"))]
            if self.overlay_mode == 1 || self.overlay_mode == 2 {
                if self.overlay_mode == 1 {
                    self.present_overlay(image);
                }
                self.pending.pop_front();
                self.stat_packed += 1;
                self.stat_image += reading.elapsed();
                unsafe { AImage_delete(image) };
                continue;
            }
            let frame = self.pack_image(image);
            self.stat_image += reading.elapsed();
            self.stat_packed += 1;
            unsafe { AImage_delete(image) };
            if let Some(frame) = frame {
                frames.push(frame);
            }
        }
    }

    /// Hands one decoded picture to a compositor layer of its own.
    ///
    /// The layer is a child of the activity's window and is created the first
    /// time a picture arrives. Nothing is copied: the `AHardwareBuffer` the
    /// codec wrote into is the buffer the compositor scans out, which is what
    /// every Android video player does and what this backend never did.
    ///
    /// The image is kept alive until the transaction has been applied - the
    /// buffer handle belongs to it - so the caller deletes it afterwards.
    #[cfg(all(feature = "surface", feature = "probe"))]
    fn present_overlay(&mut self, image: *mut AImage) {
        let control = OVERLAY_CONTROL.load(std::sync::atomic::Ordering::Acquire);
        if control.is_null() {
            return;
        }
        let control = control as *mut ASurfaceControl;

        let mut buffer: *mut AHardwareBuffer = ptr::null_mut();
        let status = unsafe { AImage_getHardwareBuffer(image, &mut buffer) };
        if status != AMEDIA_OK || buffer.is_null() {
            return;
        }
        let transaction = unsafe { ASurfaceTransaction_create() };
        unsafe {
            ASurfaceTransaction_setBuffer(transaction, control, buffer, -1);
            ASurfaceTransaction_apply(transaction);
            ASurfaceTransaction_delete(transaction);
        }
        self.stat_presented += 1;
    }

    /// Turns one `AImage` into the padded NV12 planes the renderer takes.
    ///
    /// `YUV_420_888` is three planes - Y, U, V - each with a row stride of the
    /// device's choosing and a pixel stride that says whether the colour is
    /// interleaved. The renderer wants NV12: the luma as it comes, and one plane
    /// of U,V pairs. So the luma is copied row by row past its padding, and the
    /// two colour planes are walked together into one.
    ///
    /// The row and pixel strides are asked for rather than assumed: a device
    /// that hands over planar colour has a pixel stride of 1 on each, and one
    /// that hands over semi-planar colour has 2 with the two planes
    /// overlapping. Reading either as the other is a picture with its colour
    /// swapped or striped.
    #[cfg(feature = "surface")]
    fn pack_image(&mut self, image: *mut AImage) -> Option<DecodedFrame> {
        // Taken first, so a picture this cannot read still consumes the entry
        // that belongs to it and the queue stays lined up with the images.
        //
        // The moment comes from the image itself when the device reports one -
        // that is the presentation time the codec was given, carried on the
        // buffer - because it cannot drift: a picture the codec dropped leaves
        // no image behind, and a moment read from a queue that expected one
        // would be off for every picture after it. The queue is the fallback
        // for a device that reports no timestamp, and it is what carries the
        // key-frame flag either way - that only arrives on the buffer.
        let mut timestamp_ns: i64 = 0;
        let stamping = Instant::now();
        unsafe { AImage_getTimestamp(image, &mut timestamp_ns) };
        self.stat_stamp += stamping.elapsed();
        let remembered = self.pending.pop_front();
        let keyframe = remembered.map(|(_, key)| key).unwrap_or(false);
        let pts_us = if timestamp_ns != 0 {
            timestamp_ns / 1000
        } else {
            remembered.map(|(pts, _)| pts).unwrap_or(0)
        };

        let planes_started = Instant::now();
        let mut width: i32 = 0;
        let mut height: i32 = 0;
        let mut planes: i32 = 0;
        // Safety: `image` is live for the whole call - the caller deletes it
        // after this returns - and each call only writes its out parameter.
        unsafe {
            AImage_getWidth(image, &mut width);
            AImage_getHeight(image, &mut height);
            AImage_getNumberOfPlanes(image, &mut planes);
        }
        if width <= 0 || height <= 0 || planes < 3 {
            self.stat_plane += planes_started.elapsed();
            return None;
        }
        let (width, height) = (width as usize, height as usize);

        let mut data = [ptr::null_mut::<u8>(); 3];
        let mut lengths = [0i32; 3];
        let mut row_strides = [0i32; 3];
        let mut pixel_strides = [0i32; 3];
        for plane in 0..3 {
            unsafe {
                AImage_getPlaneData(image, plane, &mut data[plane as usize], &mut lengths[plane as usize]);
                AImage_getPlaneRowStride(image, plane, &mut row_strides[plane as usize]);
                AImage_getPlanePixelStride(image, plane, &mut pixel_strides[plane as usize]);
            }
        }
        self.stat_plane += planes_started.elapsed();
        let copying = Instant::now();
        // Safety: each plane's pointer and length come from the same image and
        // are valid until it is deleted; the slices live only for this call.
        let slice_of = |plane: usize| -> Option<&[u8]> {
            let pointer = data[plane];
            let length = lengths[plane];
            if pointer.is_null() || length <= 0 {
                return None;
            }
            Some(unsafe { std::slice::from_raw_parts(pointer, length as usize) })
        };
        let (luma, u_plane, v_plane) = (slice_of(0)?, slice_of(1)?, slice_of(2)?);

        if !self.geometry_logged {
            self.geometry_logged = true;
            // The numbers that decide whether this is interleaved or planar
            // colour, and whether a row has padding - the two things a picture
            // that is striped or tinted is a difference in.
            tracing::debug!(
                target: "xgview::codec",
                width,
                height,
                planes,
                y_row_stride = row_strides[0],
                y_pixel_stride = pixel_strides[0],
                u_row_stride = row_strides[1],
                u_pixel_stride = pixel_strides[1],
                v_row_stride = row_strides[2],
                v_pixel_stride = pixel_strides[2],
                y_len = lengths[0],
                u_len = lengths[1],
                v_len = lengths[2],
                "the geometry of an AImage picture"
            );
        }

        let padded = plane_stride(width);
        let luma_row = if row_strides[0] > 0 { row_strides[0] as usize } else { width };
        let luma_pixel = if pixel_strides[0] > 0 { pixel_strides[0] as usize } else { 1 };

        let mut y = vec![0u8; padded * height];
        for row in 0..height {
            let start = row * luma_row;
            let out = y.get_mut(row * padded..row * padded + width)?;
            if luma_pixel == 1 {
                let source = luma.get(start..start + width)?;
                out.copy_from_slice(source);
            } else {
                // A luma plane with a pixel stride above one is not a layout
                // this reads; it would need a walk per sample.
                let source = luma.get(start..start + width * luma_pixel)?;
                for (column, byte) in out.iter_mut().enumerate() {
                    *byte = source[column * luma_pixel];
                }
            }
        }

        let chroma_width = width.div_ceil(2);
        let chroma_height = height.div_ceil(2);
        let u_row = if row_strides[1] > 0 { row_strides[1] as usize } else { chroma_width };
        let v_row = if row_strides[2] > 0 { row_strides[2] as usize } else { chroma_width };
        let u_pixel = if pixel_strides[1] > 0 { pixel_strides[1] as usize } else { 1 };
        let v_pixel = if pixel_strides[2] > 0 { pixel_strides[2] as usize } else { 1 };

        // The shape Android almost always hands over: U and V in one buffer, V
        // one byte after U, two bytes per sample. That is *already* an NV12
        // row, so a row is one contiguous copy - and it matters more than it
        // looks. The interleaved walk below reads one byte per bus transaction,
        // and a decoder's buffers are not cheap to touch: on the Amlogic box it
        // measured 184 ms for one 1080p picture, against 3.6 ms for the codec's
        // own byte buffer. A contiguous copy of the same bytes is a memcpy.
        let semi_planar = u_pixel == 2
            && v_pixel == 2
            && data[2] as usize == data[1] as usize + 1;

        let mut uv = vec![0u8; padded * chroma_height];
        for row in 0..chroma_height {
            let out = uv.get_mut(row * padded..row * padded + chroma_width * 2)?;
            let u_start = row * u_row;
            if semi_planar {
                // The plane's reported length is one byte short of a whole
                // number of rows: the last U sample ends where the last V
                // sample begins, so the extent the framework reports for the U
                // plane stops one byte before the row does. Asking for the full
                // row there fails, and a `?` on it would drop every picture of
                // the session - which is exactly what it did before this was
                // clamped.
                let available = u_plane.len().saturating_sub(u_start).min(chroma_width * 2);
                let source = u_plane.get(u_start..u_start + available)?;
                out.get_mut(..available)?.copy_from_slice(source);
                continue;
            }
            let v_start = row * v_row;
            // The last chroma row of some devices is shorter than the rows
            // above it, so the walk stops at what the plane actually holds
            // rather than reading past it.
            let u_end = (u_start + chroma_width * u_pixel).min(u_plane.len());
            let v_end = (v_start + chroma_width * v_pixel).min(v_plane.len());
            let u_slice = u_plane.get(u_start..u_end)?;
            let v_slice = v_plane.get(v_start..v_end)?;
            for column in 0..chroma_width {
                let u_at = column * u_pixel;
                let v_at = column * v_pixel;
                let (Some(u), Some(v)) = (u_slice.get(u_at), v_slice.get(v_at)) else {
                    break;
                };
                out[column * 2] = *u;
                out[column * 2 + 1] = *v;
            }
        }
        self.stat_copy += copying.elapsed();

        Some(DecodedFrame {
            width: width as u32,
            height: height as u32,
            format: PixelFormat::Nv12,
            colorspace: self.colorspace,
            pts_us,
            keyframe,
            buffer: None,
            planes: vec![y, uv],
        })
    }

    /// Turns one output buffer into the padded NV12 planes the renderer takes.
    ///
    /// `None` when the buffer is shorter than the layout its own format
    /// describes, which is a picture this cannot cut correctly and is better
    /// dropped than cut in the wrong place.
    ///
    /// Byte-buffer mode only: with a surface the picture never reaches a buffer
    /// this could read, and [`Self::pack_image`] does the same job from an
    /// `AImage`.
    #[cfg_attr(feature = "surface", allow(dead_code))]
    fn pack(
        &mut self,
        buffer: *const u8,
        capacity: usize,
        pts_us: i64,
        keyframe: bool,
    ) -> Option<DecodedFrame> {
        let (width, height) = (self.width as usize, self.height as usize);
        if width == 0 || height == 0 || capacity == 0 {
            return None;
        }
        // Safety: the codec handed over `capacity` readable bytes at `buffer`,
        // and the slice lives only for this call - the caller releases the
        // buffer the moment this returns.
        let data = unsafe { std::slice::from_raw_parts(buffer, capacity) };
        let stride = if self.stride == 0 { width } else { self.stride as usize };
        let slice_height = if self.slice_height == 0 { height } else { self.slice_height as usize };
        let chroma_height = height.div_ceil(2);
        let padded = plane_stride(width);

        let mut y = vec![0u8; padded * height];
        for row in 0..height {
            let start = row * stride;
            let source = data.get(start..start + width)?;
            y.get_mut(row * padded..row * padded + width)?.copy_from_slice(source);
        }

        // Where the colour is, and in which of the two shapes the codec chose.
        //
        // `color-format` says `YUV420Flexible` for both of them, so it cannot be
        // asked: what distinguishes them is the size of the buffer the codec
        // handed over. Interleaved (NV12) needs the luma plane, its padding and
        // one chroma row per two luma rows; planar (I420) needs the luma plane
        // and two chroma planes of half the width each. One here does both, on
        // the same 640x480 stream, from one session to the next: 491520 bytes
        // (768 rows, interleaved, chroma at row 512) on one, and 460800 -
        // exactly 640x480 + 320x240 + 320x240 - on the next. Read as the wrong
        // one, the picture is sharp and green-magenta striped: the luma where it
        // should be, and the colour of a picture that is not there.
        //
        // The shape is asked for at configure time - `COLOR_FormatYUV420SemiPlanar`
        // - and a codec that takes the name has no say in it. This is for the ones
        // that do not: it reads the shape out of the size of the buffer, and when
        // the two shapes are the same size - which they are when the luma is not
        // padded - it takes the one that was asked for.
        let base = stride * slice_height;
        let (layout, chroma_stride) = *self
            .layout
            .get_or_insert_with(|| shape_of(width, height, stride, slice_height, capacity));
        if !self.geometry_logged {
            self.geometry_logged = true;
            // Chroma is meant to sit near 128 for a low-saturation picture, so
            // the first bytes at each candidate offset say which of them holds
            // it: two numbers per offset, both around 128, and it is chroma.
            let at = |offset: usize| -> [u8; 4] {
                let mut out = [0u8; 4];
                if let Some(slice) = data.get(offset..offset + 4) {
                    out.copy_from_slice(slice);
                }
                out
            };
            tracing::debug!(
                target: "xgview::codec",
                width,
                height,
                stride,
                slice_height,
                capacity,
                chroma_base = base,
                colour_format = ?self.colour_format,
                at_height = ?at(stride * height),
                at_slice = ?at(stride * slice_height),
                "the geometry of a session's pictures"
            );
            // One picture out per geometry, for looking at by hand. The grid's
            // streams only: a 1080p buffer is three megabytes and the fault is
            // in the small ones.
            if capacity <= 1_500_000 {
                crate::dump::dump(width as u32, height as u32, stride as u32, slice_height as u32, data);
            }
        }
        let mut uv = vec![0u8; padded * chroma_height];
        match layout {
            Layout::SemiPlanar => {
            // The interleaved plane follows the luma one, one chroma row per two
            // luma rows, with the codec's padding in between.
            for row in 0..chroma_height {
                let start = base + row * stride;
                let source = data.get(start..start + width)?;
                uv.get_mut(row * padded..row * padded + width)?.copy_from_slice(source);
            }
            }
            Layout::Planar => {
            // Two planes of their own, U then V, each half as wide as the
            // picture, interleaved into the one plane the renderer binds.
            let u_base = stride * height;
            let v_base = u_base + chroma_stride * chroma_height;
            for row in 0..chroma_height {
                let u_row = data.get(u_base + row * chroma_stride..u_base + (row + 1) * chroma_stride)?;
                let v_row = data.get(v_base + row * chroma_stride..v_base + (row + 1) * chroma_stride)?;
                let out = uv.get_mut(row * padded..row * padded + width)?;
                for column in 0..chroma_stride {
                    out[column * 2] = u_row[column];
                    out[column * 2 + 1] = v_row[column];
                }
            }
            }
            Layout::Unknown => {
                // Neither shape fits the buffer the codec handed over, which is
                // a layout this cannot read: the picture is given up rather than
                // drawn in colours that are not there.
                return None;
            }
        }

        Some(DecodedFrame {
            width: width as u32,
            height: height as u32,
            format: PixelFormat::Nv12,
            colorspace: self.colorspace,
            pts_us,
            keyframe,
            buffer: None,
            planes: vec![y, uv],
        })
    }

    /// Stops and releases the codec. Safe to call twice, and called by [`Drop`]
    /// when the channel ends.
    fn close(&mut self) {
        if let Some(codec) = self.handle.take() {
            if self.started {
                unsafe { AMediaCodec_stop(codec) };
                self.started = false;
            }
            unsafe { AMediaCodec_delete(codec) };
        }
        // The reader owns the `ANativeWindow` the codec was configured with, so
        // it outlives the codec and is deleted after it.
        #[cfg(feature = "surface")]
        if let Some(reader) = self.reader.take() {
            unsafe { AImageReader_delete(reader) };
        }
    }
}

impl VideoDecoder for AMediaCodecDecoder {
    fn name(&self) -> &'static str {
        "AMediaCodec"
    }

    fn configure(&mut self, config: &DecoderConfig) -> Result<()> {
        self.info = Some(VideoStreamInfo {
            codec: config.codec,
            width: config.width,
            height: config.height,
            fps: None,
            hardware: true,
        });
        // Remembered, not acted on: the codec session is opened on the first
        // access unit, which is the first thing that can describe the stream.
        self.config = config.clone();
        // A new session knows nothing about the pace of this codec yet, so it
        // starts at the floor and finds out. Waiting longer than the codec needs
        // is not free: the wait blocks the thread the session runs on, and a
        // session that blocks is a session that is not reading its socket - see
        // `worker_threads`. See [`Self::wait`].
        self.wait = crate::wait::WAIT_MIN;
        self.geometry_logged = false;
        self.layout = None;
        #[cfg(feature = "surface")]
        self.pending.clear();
        // A new session is a new codec, and its buffers are not the last one's.
        #[cfg(feature = "probe")]
        self.output_buffers.clear();
        Ok(())
    }

    fn decode(
        &mut self,
        access_unit: &[u8],
        pts_us: i64,
        keyframe: bool,
    ) -> Result<Vec<DecodedFrame>> {
        let whole = Instant::now();
        let opening = Instant::now();
        if self.handle.is_none() {
            self.open(access_unit)?;
            tracing::debug!(
                target: "xgview::codec",
                took_ms = opening.elapsed().as_millis() as u64,
                "the decoder was opened"
            );
        }
        let Some(codec) = self.handle else {
            return Err(CodecError::Configure("decoder was never configured".to_string()));
        };

        // The wait is retried once the ceiling has grown: a codec that was a
        // hundred milliseconds short of a buffer is the case this exists for,
        // and giving the picture up there is what costs the channel its next
        // key frame. A codec that does not answer at all still ends the wait at
        // `WAIT_MAX` and the unit is given up as before.
        let dequeuing = Instant::now();
        let Some(index) = self.dequeue_input(codec) else {
            return Err(CodecError::NoBuffer);
        };
        self.stat_dequeue += dequeuing.elapsed();

        let mut capacity = 0usize;
        let queueing = Instant::now();
        let buffer = unsafe { AMediaCodec_getInputBuffer(codec, index, &mut capacity) };
        if buffer.is_null() || capacity < access_unit.len() {
            // The input buffer has to be handed back or the codec runs out of
            // them; queueing it empty is how a dequeued input buffer is returned
            // unused. The picture it would have carried is lost either way.
            unsafe {
                AMediaCodec_queueInputBuffer(codec, index, 0, 0, pts_us.max(0) as u64, 0);
            }
            return Err(CodecError::Decode("input buffer too small".to_string()));
        }
        unsafe {
            ptr::copy_nonoverlapping(access_unit.as_ptr(), buffer, access_unit.len());
        }

        let mut flags = 0u32;
        if keyframe {
            flags |= BUFFER_FLAG_KEY_FRAME;
        }
        unsafe {
            AMediaCodec_queueInputBuffer(
                codec,
                index,
                0,
                access_unit.len(),
                pts_us.max(0) as u64,
                flags,
            );
        }
        self.stat_queue += queueing.elapsed();

        let mut frames = Vec::new();
        self.drain(&mut frames);
        self.stat_total += whole.elapsed();
        self.stat_frames += 1;
        if self.stat_frames >= PACE_FRAMES {
            tracing::debug!(
                target: "xgview::codec",
                size = ?(self.width, self.height),
                calls = self.stat_frames,
                packed = self.stat_packed,
                dequeue_ms = self.stat_dequeue.as_millis() as u64,
                output_ms = self.stat_output.as_millis() as u64,
                get_ms = self.stat_get.as_millis() as u64,
                pack_ms = self.stat_pack.as_millis() as u64,
                queue_ms = self.stat_queue.as_millis() as u64,
                drain_ms = self.stat_drain.as_millis() as u64,
                release_ms = self.stat_release.as_millis() as u64,
                acquire_ms = self.stat_acquire.as_millis() as u64,
                image_ms = self.stat_image.as_millis() as u64,
                stamp_ms = self.stat_stamp.as_millis() as u64,
                plane_ms = self.stat_plane.as_millis() as u64,
                copy_ms = self.stat_copy.as_millis() as u64,
                presented = self.stat_presented,
                pictures = self.pictures,
                total_ms = self.stat_total.as_millis() as u64,
                surface = cfg!(feature = "surface"),
                "the decoder's pace"
            );
            self.stat_dequeue = Duration::ZERO;
            self.stat_output = Duration::ZERO;
            self.stat_get = Duration::ZERO;
            self.stat_pack = Duration::ZERO;
            self.stat_queue = Duration::ZERO;
            self.stat_drain = Duration::ZERO;
            self.stat_release = Duration::ZERO;
            self.stat_acquire = Duration::ZERO;
            self.stat_plane = Duration::ZERO;
            self.stat_stamp = Duration::ZERO;
            self.stat_copy = Duration::ZERO;
            self.stat_image = Duration::ZERO;
            self.stat_total = Duration::ZERO;
            self.stat_frames = 0;
            self.stat_packed = 0;
        }
        Ok(frames)
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        // The end of a session: hand over whatever the codec is still holding.
        let mut frames = Vec::new();
        self.drain(&mut frames);
        Ok(frames)
    }

    fn pictures(&self) -> u64 {
        self.pictures
    }

    fn info(&self) -> Option<&VideoStreamInfo> {
        self.info.as_ref()
    }
}

impl Drop for AMediaCodecDecoder {
    fn drop(&mut self) {
        self.close();
    }
}

impl AMediaCodecDecoder {
    /// Waits for an input buffer, growing the ceiling and trying again while
    /// the codec is only a little behind.
    ///
    /// Three rounds at most: the ceiling doubles, so the last one is four times
    /// the first, and a codec that has not answered by then has stopped rather
    /// than fallen behind. `None` is that case, and the picture is given up as
    /// it always was. See [`Self::wait`].
    fn dequeue_input(&mut self, codec: *mut AMediaCodec) -> Option<usize> {
        for _ in 0..3 {
            let budget = self.wait.max(crate::wait::WAIT_MIN);
            let started = Instant::now();
            let deadline = started + budget;
            let mut index = unsafe { AMediaCodec_dequeueInputBuffer(codec, 0) };
            while index < 0 && Instant::now() < deadline {
                index = unsafe { AMediaCodec_dequeueInputBuffer(codec, 10_000) };
            }
            let next = crate::wait::next_wait(budget, started.elapsed(), index >= 0);
            let grew = next > budget;
            self.wait = next;
            if index >= 0 {
                return Some(index as usize);
            }
            if !grew {
                return None;
            }
        }
        None
    }

    /// Drops the buffered pictures and restarts the codec, for a stream switch
    /// (main <-> sub) that keeps the same session.
    pub fn reset(&mut self) {
        if let Some(codec) = self.handle {
            unsafe { AMediaCodec_flush(codec) };
        }
        // A flush discards whatever the codec had not rendered, so the entries
        // remembered for those pictures no longer have one coming.
        #[cfg(feature = "surface")]
        self.pending.clear();
        // And a flushed codec may hand back different buffers.
        #[cfg(feature = "probe")]
        self.output_buffers.clear();
    }
}
/// Android `AMediaCodec` decoder for the compressed audio codecs - AAC above
/// all. It reads raw AAC access units and hands over interleaved s16 PCM, the
/// same contract as the desktop's FFmpeg backend. The session is opened once
/// with the stream's `AudioSpecificConfig`, which is the SDP's `config=`.
#[derive(Debug, Default)]
pub struct AmcAudioDecoder {
    handle: Option<*mut AMediaCodec>,
    info: Option<AudioStreamInfo>,
    /// Running timestamp handed to the input queue so the codec orders its
    /// output. One AAC frame is 1024 samples.
    next_pts_us: u64,
}

// Safety: same as the video codec - the channel worker owns it outright and is
// the only thread that ever touches it.
unsafe impl Send for AmcAudioDecoder {}

impl AmcAudioDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    fn close(&mut self) {
        if let Some(codec) = self.handle.take() {
            unsafe {
                AMediaCodec_stop(codec);
                AMediaCodec_delete(codec);
            }
        }
    }

    /// Reads whatever the codec has ready into `out` as interleaved s16.
    fn drain(&mut self, out: &mut Vec<i16>) {
        let Some(codec) = self.handle else { return };
        loop {
            let mut info = AMediaCodecBufferInfo::default();
            let index = unsafe { AMediaCodec_dequeueOutputBuffer(codec, &mut info, 0) };
            if index < 0 {
                break;
            }
            if info.size > 0 {
                let mut capacity = 0usize;
                let buffer = unsafe { AMediaCodec_getOutputBuffer(codec, index as usize, &mut capacity) };
                let start = info.offset as usize;
                let len = info.size as usize;
                if !buffer.is_null() && start + len <= capacity {
                    // The audio output format is interleaved 16 bit PCM.
                    let ptr = unsafe { buffer.add(start) } as *const i16;
                    let slice = unsafe { std::slice::from_raw_parts(ptr, len / 2) };
                    out.extend_from_slice(slice);
                }
            }
            unsafe { AMediaCodec_releaseOutputBuffer(codec, index as usize, false) };
        }
    }
}

impl AudioDecoder for AmcAudioDecoder {
    fn name(&self) -> &'static str {
        "amediacodec"
    }

    fn configure(&mut self, info: &AudioStreamInfo) -> Result<()> {
        self.close();
        let mime_name = match info.codec {
            AudioCodec::Aac => "audio/mp4a-latm",
            other => {
                return Err(CodecError::Unsupported(format!("no AMediaCodec audio for {other}")))
            }
        };
        let mime = CString::new(mime_name).map_err(|err| CodecError::Configure(err.to_string()))?;
        let codec = unsafe { AMediaCodec_createDecoderByType(mime.as_ptr()) };
        if codec.is_null() {
            return Err(CodecError::Unsupported(format!("no AMediaCodec decoder for {mime_name}")));
        }
        let format = unsafe { AMediaFormat_new() };
        unsafe {
            AMediaFormat_setString(format, c"mime".as_ptr(), mime.as_ptr());
            AMediaFormat_setInt32(format, c"sample-rate".as_ptr(), info.sample_rate as i32);
            AMediaFormat_setInt32(format, c"channel-count".as_ptr(), info.channels as i32);
            // AAC Low Complexity, the profile the cameras send.
            AMediaFormat_setInt32(format, c"aac-profile".as_ptr(), 2);
            // The SDP's `config=` is the AudioSpecificConfig the decoder opens with.
            if !info.extradata.is_empty() {
                AMediaFormat_setBuffer(
                    format,
                    c"csd-0".as_ptr(),
                    info.extradata.as_ptr().cast(),
                    info.extradata.len(),
                );
            }
            AMediaFormat_setInt32(format, c"max-input-size".as_ptr(), 16 * 1024);
        }
        let configured = unsafe { AMediaCodec_configure(codec, format, ptr::null_mut(), ptr::null_mut(), 0) };
        unsafe { AMediaFormat_delete(format) };
        if configured != AMEDIA_OK {
            unsafe { AMediaCodec_delete(codec) };
            return Err(CodecError::Configure(format!("AMediaCodec audio configure failed: {configured}")));
        }
        if unsafe { AMediaCodec_start(codec) } != AMEDIA_OK {
            unsafe { AMediaCodec_delete(codec) };
            return Err(CodecError::Configure("AMediaCodec audio start failed".to_string()));
        }
        self.handle = Some(codec);
        self.next_pts_us = 0;
        self.info = Some(info.clone());
        Ok(())
    }

    fn decode(&mut self, access_unit: &[u8], out: &mut Vec<i16>) -> Result<()> {
        let codec = self
            .handle
            .ok_or_else(|| CodecError::Configure("audio decoder not configured".into()))?;
        let index = unsafe { AMediaCodec_dequeueInputBuffer(codec, 0) };
        if index >= 0 {
            let mut capacity = 0usize;
            let buffer = unsafe { AMediaCodec_getInputBuffer(codec, index as usize, &mut capacity) };
            if !buffer.is_null() && access_unit.len() <= capacity {
                unsafe { ptr::copy_nonoverlapping(access_unit.as_ptr(), buffer, access_unit.len()) };
                let rate = self.info.as_ref().map(|i| i.sample_rate).unwrap_or(8000).max(1) as u64;
                let pts = self.next_pts_us;
                self.next_pts_us += 1024 * 1_000_000 / rate;
                unsafe { AMediaCodec_queueInputBuffer(codec, index as usize, 0, access_unit.len(), pts, 0) };
            }
        }
        self.drain(out);
        Ok(())
    }

    fn flush(&mut self, out: &mut Vec<i16>) -> Result<()> {
        self.drain(out);
        Ok(())
    }

    fn info(&self) -> Option<&AudioStreamInfo> {
        self.info.as_ref()
    }
}

impl Drop for AmcAudioDecoder {
    fn drop(&mut self) {
        self.close();
    }
}
