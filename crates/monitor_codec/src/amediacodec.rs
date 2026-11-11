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
//! - **The activity's surface**: when the session has a window to render into,
//!   the codec is configured with it and `releaseOutputBuffer` hands the picture
//!   to the compositor. Nothing comes back through this process, which is what
//!   makes a 1080p picture cost a millisecond a frame instead of fifty. See
//!   `set_surface_window` and `SurfaceActivity`.
//!
//! A third mode once existed - an `AImageReader`'s window, with the pictures
//! acquired as `AImage` objects - and it was removed: on a SHIELD its buffers
//! cannot be locked for CPU access at all, and reading a picture back costs the
//! same from a reader as from the codec's own buffer. The measurements that
//! argued for it are in `priv/docs/playback-decode-cost.md`.
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

// The picture surface's record is a state machine of its own, and one that
// does not need the NDK: it lives in `surface`, where the race between a late
// frame and a replaced window can be tested on the desktop. The names the wall
// has always reached through `amediacodec` are kept here.
pub use crate::surface::{
    begin_surface_picture, forget_surface_picture, set_surface_layer_wanted,
    set_surface_suspended, set_surface_window, surface_holds_picture, surface_presenting,
    video_layer_active,
};
use crate::surface::{
    millis_now, surface_layer_wanted, surface_rendered, surface_session_closed, surface_snapshot,
    surface_suspended,
};

/// How many pictures the decoder's pace is measured over.
///
/// One line per this many pictures, at debug: the numbers say whether a channel
/// is being held up by the codec's buffer hand-off or by the copy that reads the
/// picture back, and they are the same numbers on every device - which is what
/// makes two of them comparable. See `priv/docs/playback-decode-cost.md`.
const PACE_FRAMES: u32 = 60;

/// `AMediaCodec` decoder.
///
/// It reads pictures back through `AMediaCodec_getOutputBuffer`, unless the
/// session was given the activity's surface - then there is nothing to read and
/// nothing comes back.
#[derive(Debug, Default)]
pub struct AMediaCodecDecoder {
    /// The codec, alive from the first access unit to the end of the session.
    handle: Option<*mut AMediaCodec>,
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
    /// What the codec's own padding is stripped with, on the one path that has
    /// a buffer to strip: a picture handed over as bytes has the codec's padding
    /// around it, and a picture rendered into a surface never comes here.
    stride: u32,
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
    /// The value of [`SURFACE_SUSPENDED`] this session was opened under.
    ///
    /// A codec cannot change the kind of output it was configured with, so a
    /// suspension or a resumption has to close it and let the next access unit
    /// open it again on the other path. Comparing this with the flag is what
    /// says whether that has happened yet.
    suspended: bool,
    /// Whether this session's picture is wide enough to be offered the
    /// activity's surface at all.
    ///
    /// The grid's sub streams are not, and going into or out of a magnification
    /// must leave their decoders alone: reopening nine of them would stall the
    /// whole wall for a change that concerns one picture.
    surface_eligible: bool,
    /// Whether this session has already tried to move on to the surface during
    /// the current magnification.
    ///
    /// A tile pinned to the main stream is decoded into byte buffers while the
    /// wall is a grid. Magnifying it changes no plan, so nothing replaces its
    /// session - and on a box that cannot afford to read 1080p back, the
    /// viewer watches a stuttering picture until some unrelated retry happens
    /// to reopen the stream. `decode` reopens this one decoder on the surface
    /// when the magnification begins. What is remembered is the *attempt*, not
    /// the state the decoder was opened under: a device that refuses to point
    /// this codec at the window falls back to byte buffers, and must not be
    /// reopened on every following frame. Leaving the magnification clears it,
    /// so the next one gets a fresh attempt.
    surface_attempted: bool,
    /// The surface's `request` as it stood when this session was opened. A
    /// decoder opened for a picture that has since been asked of another camera
    /// must not put its late frames back into the surface's memory: the wall
    /// would keep away from an area showing the camera that was just left. See
    /// `drain`.
    request: u64,
    /// The surface's `generation` as it stood when this session was opened, and
    /// the answer to "is the window I was configured with still the one on
    /// screen?" A codec is pointed at a window when it is configured and cannot
    /// be pointed at another, so a surface that was destroyed and made again
    /// needs a new session: otherwise it renders into a window nobody owns,
    /// which is a black rectangle on the screen. See `decode`.
    ///
    /// A number rather than the `ANativeWindow*` it used to be: the activity can
    /// hand back a different wrapper for the same surface, and two windows that
    /// come one after another can be given the same address. Neither says
    /// anything about whether the session is still the current one.
    ///
    /// This generation belongs to the *picture's state*. The codec also needs
    /// to know when the view it was configured with changed - a step to the
    /// other camera's view - which is `opened_epoch` below. Those two are not
    /// the same event: a switch keeps the picture and the request and only
    /// repoints the codec, while a surface that was destroyed and made again
    /// forgets everything.
    generation: u64,
    /// The surface's `window_epoch` this session was configured with.
    opened_epoch: u64,
    /// Whether this decoder counted itself in the surface's `sessions`. `close`
    /// takes it out again, and the flag is what makes that happen exactly once.
    ///
    /// Counted when a picture is first rendered into the surface, not when the
    /// codec is opened with one: between the two the surface still holds what
    /// the last session left in it, and anything that shows the picture's view
    /// on the strength of a window existing shows that instead - which is a
    /// frame of the previous camera, on a channel step.
    presenting: bool,
    /// Set once an output format this cannot read as NV12 has been reported.
    ///
    /// Byte-buffer mode only: with a surface the codec reports
    /// `COLOR_FormatSurface`, which is not a picture layout and is not warned
    /// about.
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
    /// With no window the codec hands every decoded picture back as a byte
    /// buffer. With one - the activity's surface - it renders the picture into
    /// it and hands nothing back at all.
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
        self.suspended = surface_suspended();
        let config = self.config.clone();
        let mime = CString::new(config.codec.mime())
            .map_err(|err| CodecError::Configure(err.to_string()))?;
        // The sets to configure the decoder with, and the picture size the
        // sequence one describes - H.264's SPS and PPS, or H.265's VPS, SPS and
        // PPS. What the stream describes beats what the session announced, which
        // is only a hint for the buffers the decoder sizes up front.
        let (csd, size) = decode_parameter_sets(config.codec, access_unit);
        let size = size.or(Some((config.width, config.height)).filter(|(w, h)| *w > 0 && *h > 0));
        self.surface_eligible = size.is_some_and(|(width, _)| width >= 1280);

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
        // A codec given a surface is not asked: the format names the layout of
        // memory this side reads, and a picture that never comes back has none.
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
        //
        // `video_layer_active` asks for a magnified viewport, not merely a
        // window. The window exists on a grid too, while its view is off
        // screen: a tile pinned to the main stream there is just as wide, and
        // a session configured with the surface would send every frame into
        // that hidden view while the grid waits for a texture it never gets.
        // See `surface::set_surface_layer_wanted`.
        if video_layer_active() && self.surface_eligible {
            // The window and the generation are one answer. Reading the window
            // alone and asking later which generation it was would let a
            // session that outlives its surface render into the new one.
            let snapshot = surface_snapshot();
            let window = snapshot.window;
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
                match opened {
                    Ok(codec) => {
                        self.handle = Some(codec);
                        self.started = true;
                        // Rendering into the activity's surface: the picture
                        // leaves the codec as a rendered frame, never as bytes.
                        //
                        // Not counted among the surface's sessions yet. The
                        // window is the one the last session was rendering into,
                        // and it still holds that session's last picture; a
                        // surface keeps what was in it until something replaces
                        // it. Until this codec has put a picture there, the view
                        // showing it would be showing the camera before this
                        // one. The count is taken in `drain`, on the first
                        // picture actually rendered.
                        self.on_surface = true;
                        self.generation = snapshot.generation;
                        self.request = snapshot.request;
                        self.opened_epoch = snapshot.window_epoch;
                        if let (Some((width, height)), Some(info)) = (size, self.info.as_mut()) {
                            info.width = width;
                            info.height = height;
                        }
                        return Ok(());
                    }
                    // A codec that will not render into this window is not a
                    // stream that cannot be decoded: the byte buffers below are
                    // the path every other stream on this device already takes,
                    // and it is slower rather than broken. Falling through to it is
                    // the difference between a picture at three quarters of its
                    // rate and no picture at all - which is what a device whose
                    // decoder refuses the window would otherwise get, since the
                    // window is offered to every stream wide enough to be worth
                    // it.
                    Err(err) => tracing::warn!(
                        target: "xgview::codec",
                        %err,
                        "the codec would not render into the activity's surface; reading pictures back instead"
                    ),
                }
            }
        }

        // Byte buffers, which is what every stream that is not on the
        // activity's surface uses. An `AImageReader` was tried here - the codec
        // renders into the reader's window and the pictures come back as images
        // - and it was taken out again: on a SHIELD the buffers cannot be locked
        // for CPU access at all (`AImage_getPlaneData: failed to lock buffer for
        // CPU access`, -30003), so every picture was lost, and on another box the
        // lock aborts inside the platform's own gralloc mapper. Reading a picture
        // back costs the same either way; the reader only ever made it possible
        // to *not* read it back, which is what the activity's surface does.
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
        self.started = true;
        // The size the stream describes is the one to report, not the one the
        // session guessed at before any of it arrived.
        if let (Some((width, height)), Some(info)) = (size, self.info.as_mut()) {
            info.width = width;
            info.height = height;
        }
        tracing::debug!(
            target: "xgview::codec",
            codec = config.codec.as_str(),
            "AMediaCodec session open, decoding into byte buffers"
        );
        Ok(())
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
        // Only a byte-buffer session has a colour format this reads: a codec
        // given a surface reports `COLOR_FormatSurface` (0x7F000789), which is
        // not a picture layout at all - the images come out of the reader in
        // `YUV_420_888` instead. Asked at run time rather than at build time,
        // because both kinds of session are built by the same binary.
        if !self.on_surface {
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
    /// them. What the release does with the picture is the one thing that
    /// differs: a byte buffer is thrown away after the copy, and a picture on
    /// the activity's surface is `render = true` - the release *is* the hand-off.
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
                    } else {
                        if info.size > 0 {
                            let mut capacity = 0usize;
                            let fetching = Instant::now();
                            let buffer = unsafe {
                                AMediaCodec_getOutputBuffer(codec, index as usize, &mut capacity)
                            };
                            self.stat_get += fetching.elapsed();
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
                        // Every rendered picture, not just the first: a session
                        // can play for hours between two seeks, and the moment a
                        // handover is measured from is the last one.
                        //
                        // But a frame from a session whose request or whose
                        // window has since changed is a picture of a camera or
                        // a surface that is gone. `surface_rendered` asks and
                        // writes under one lock, so a window replaced in the
                        // middle of this render cannot be told it holds a
                        // picture it never got. It also answers whether this
                        // session has just been counted as presenting: the
                        // first picture is the moment the surface stops being
                        // the previous camera's.
                        if render
                            && surface_rendered(
                                self.generation,
                                self.request,
                                millis_now(),
                                self.presenting,
                            )
                        {
                            self.presenting = true;
                            tracing::info!(
                                target: "xgview::codec",
                                "the first picture is in the activity's surface"
                            );
                        }
                    }
                }
                _ => break,
            }
        }
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

        // When the codec's stride already equals the padded row the upload
        // wants - which some codecs hand back - the plane is one contiguous run
        // and it is taken in a single pass rather than `height` small ones. The
        // cost being spent here is the number of calls, not the bytes: see
        // `priv/docs/grid-cpu-cost.md`, item a.
        let y = if stride == padded {
            data.get(..padded * height)?.to_vec()
        } else {
            let mut y = vec![0u8; padded * height];
            for row in 0..height {
                let start = row * stride;
                let source = data.get(start..start + width)?;
                y.get_mut(row * padded..row * padded + width)?.copy_from_slice(source);
            }
            y
        };

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
        let uv = match layout {
            Layout::SemiPlanar => {
                // The interleaved plane follows the luma one, one chroma row per
                // two luma rows, with the codec's padding in between. At the
                // upload stride it is one run, like the luma plane.
                if stride == padded {
                    data.get(base..base + padded * chroma_height)?.to_vec()
                } else {
                    let mut uv = vec![0u8; padded * chroma_height];
                    for row in 0..chroma_height {
                        let start = base + row * stride;
                        let source = data.get(start..start + width)?;
                        uv.get_mut(row * padded..row * padded + width)?.copy_from_slice(source);
                    }
                    uv
                }
            }
            Layout::Planar => {
                // Two planes of their own, U then V, each half as wide as the
                // picture, interleaved into the one plane the renderer binds.
                let mut uv = vec![0u8; padded * chroma_height];
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
                uv
            }
            Layout::Unknown => {
                // Neither shape fits the buffer the codec handed over, which is
                // a layout this cannot read: the picture is given up rather than
                // drawn in colours that are not there.
                return None;
            }
        };

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
        if self.presenting {
            self.presenting = false;
            surface_session_closed(self.generation);
        }
        if let Some(codec) = self.handle.take() {
            if self.started {
                unsafe { AMediaCodec_stop(codec) };
                self.started = false;
            }
            unsafe { AMediaCodec_delete(codec) };
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
        Ok(())
    }

    fn decode(
        &mut self,
        access_unit: &[u8],
        pts_us: i64,
        keyframe: bool,
    ) -> Result<Vec<DecodedFrame>> {
        let whole = Instant::now();
        // A codec renders into the window it was opened on, and cannot be
        // pointed at another. The activity's picture surface is destroyed when
        // the window goes away - leaving the app, or anything else that stops
        // it - and a new one is made when it comes back, so the window this
        // codec holds is by then a surface nobody owns: the picture it renders
        // goes nowhere, the view shows nothing, and the wall leaves the area
        // unpainted because a session still counts itself as presenting. That
        // is a black rectangle that stays until the stream is reopened for some
        // other reason.
        //
        // So the decoder is dropped and the next access unit opens a new one on
        // the window that exists now. The session prepends the parameter sets it
        // cached, so this reopens on whatever arrives; the first units are
        // dropped as non-keyframes and the picture is back on the next one.
        //
        // Only from one window to another: a window that is *gone* is no window
        // to reopen on, and the app is usually not on screen then. That case
        // falls to the same picture staying black, invisibly, until it returns.
        //
        // A suspension is not a window going away. The turn that asked for it
        // needs this picture as bytes, and undoing the turn needs it back in
        // the surface; neither is a change a configured codec can make, so the
        // session is dropped and the next access unit opens it on the other
        // path. `surface_presenting` answers for the frames in between.
        let wanted_now = surface_layer_wanted();
        if !wanted_now {
            // Back on a grid: the next magnification gets one attempt of its
            // own, whether or not this one managed to reach the surface.
            self.surface_attempted = false;
        }
        if self.suspended != surface_suspended() {
            tracing::info!(
                target: "xgview::codec",
                suspended = surface_suspended(),
                "the surface path was suspended or resumed; reopening the decoder"
            );
            self.close();
            self.on_surface = false;
            self.generation = 0;
        }
        // The window outlives the view. Going back to the grid moves the
        // picture's view off screen but does not end a stream that was opened
        // for it: a session still rendering into that surface hands the grid
        // no frames at all, and a tile pinned to the main stream would wait
        // for its first picture for ever. Reopen it on byte buffers, the path
        // every grid picture needs.
        if self.on_surface && !wanted_now {
            tracing::info!(
                target: "xgview::codec",
                "the magnified view went away; reopening the decoder for the grid"
            );
            self.close();
            self.on_surface = false;
            self.generation = 0;
        }
        // The other direction, and why the attempt is remembered. A tile pinned
        // to the main stream is opened on byte buffers while the wall is a grid;
        // magnifying it changes no plan, so nothing replaces the session. On a
        // television box reading 1080p back costs hundreds of milliseconds a
        // frame - the viewer watches that stutter until an unrelated retry
        // reopens the stream and the new session happens to take the surface.
        // Magnify on purpose instead: reopen this one decoder on the surface.
        //
        // Only a wide session, only once per magnification, only while the
        // surface can really be had. The last condition is what lets a window
        // that arrives late, or a turn that is undone, still get the switch
        // rather than burning the attempt on a decoder that would only fall
        // back to byte buffers. The wall keeps drawing its held texture until
        // the new session puts its first picture in the surface, exactly as a
        // magnification that switches streams already does. The grid's sub
        // streams are not eligible, so they are never disturbed.
        if self.surface_eligible
            && self.handle.is_some()
            && !self.on_surface
            && wanted_now
            && !self.surface_attempted
            && video_layer_active()
        {
            tracing::info!(
                target: "xgview::codec",
                "the magnified view asked for the surface; reopening the decoder"
            );
            self.surface_attempted = true;
            self.close();
            self.on_surface = false;
            self.generation = 0;
        }
        if self.on_surface {
            let current = surface_snapshot();
            if !current.window.is_null() && self.opened_epoch != current.window_epoch {
                tracing::info!(
                    target: "xgview::codec",
                    "the view a picture is rendered into changed; reopening the decoder"
                );
                self.close();
                self.on_surface = false;
                self.generation = 0;
            }
        }
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
                pictures = self.pictures,
                total_ms = self.stat_total.as_millis() as u64,
                "the decoder's pace"
            );
            self.stat_dequeue = Duration::ZERO;
            self.stat_output = Duration::ZERO;
            self.stat_get = Duration::ZERO;
            self.stat_pack = Duration::ZERO;
            self.stat_queue = Duration::ZERO;
            self.stat_drain = Duration::ZERO;
            self.stat_release = Duration::ZERO;
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
