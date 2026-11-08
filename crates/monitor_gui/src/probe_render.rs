//! A minimal egui on a `SurfaceView` the app owns.
//!
//! The wall runs in a `NativeActivity`, which has turned out to be the reason
//! the decoder cannot be given a surface of its own: a window handed to native
//! drawing cannot also host a view hierarchy - the framework's drawing wipes
//! the native one - and a separate window takes the activity's input with it.
//! Both were measured; see `priv/docs/playback-decode-cost.md` §13.
//!
//! What is left is an ordinary activity hosting two surfaces: one for this
//! renderer, one for the video. §14 proved wgpu can present to a
//! `SurfaceView`'s window; this module answers the last open question - whether
//! **egui** can be drawn there, fonts and tessellation and all, rather than a
//! bare clear colour.
//!
//! Nothing in the app calls it. `ProbeActivity` does, and only by hand.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

/// The size the surface was last told it has.
static WIDTH: AtomicU32 = AtomicU32::new(1920);
static HEIGHT: AtomicU32 = AtomicU32::new(1080);

/// Whether the drawing thread should keep going.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// A window pointer that may cross to the drawing thread.
///
/// The pointer is the `SurfaceView`'s, and the activity keeps the view alive
/// for as long as this thread runs - it calls [`stop`] before letting go. The
/// wrapper is what says so to the compiler; there is nothing else it does.
struct Window(*mut std::ffi::c_void);

impl Window {
    /// The pointer itself. A method, so a closure that calls it captures the
    /// wrapper - which is `Send` - rather than the raw field, which is not.
    fn pointer(&self) -> *mut std::ffi::c_void {
        self.0
    }
}

// Safety: see the type's own note. The thread only hands it to wgpu.
unsafe impl Send for Window {}

/// Starts drawing into `window`, on a thread of its own.
pub fn start(window: *mut std::ffi::c_void) {
    if window.is_null() {
        return;
    }
    RUNNING.store(true, Ordering::Release);
    let window = Window(window);
    std::thread::spawn(move || run(window.pointer()));
}

/// Tells the drawing thread the surface's new size.
pub fn resize(width: i32, height: i32) {
    if width > 0 {
        WIDTH.store(width as u32, Ordering::Release);
    }
    if height > 0 {
        HEIGHT.store(height as u32, Ordering::Release);
    }
}

/// Asks the drawing thread to stop, which it does at the next frame.
pub fn stop() {
    RUNNING.store(false, Ordering::Release);
}

/// Writes one line to logcat.
///
/// This activity is started on its own, so the process has no `tracing`
/// subscriber of its own - the wall installs one when it starts, and nothing
/// here does. A probe that cannot say what it found is worth nothing, so the
/// few lines that matter go straight to the log.
fn note(message: &str) {
    #[link(name = "log")]
    extern "C" {
        fn __android_log_print(
            priority: i32,
            tag: *const std::ffi::c_char,
            format: *const std::ffi::c_char,
            ...
        ) -> i32;
    }
    let Ok(text) = std::ffi::CString::new(message) else {
        return;
    };
    // ANDROID_LOG_INFO
    unsafe {
        __android_log_print(4, c"xgview-probe".as_ptr(), c"%s".as_ptr(), text.as_ptr());
    }
}

/// Runs one future to completion on a runtime of its own.
///
/// The two calls that need it happen once, before the loop; a runtime per call
/// is cheaper than keeping one alive for a probe.
fn block_on<F: std::future::Future>(future: F) -> Option<F::Output> {
    let runtime = tokio::runtime::Builder::new_current_thread().build().ok()?;
    Some(runtime.block_on(future))
}

fn run(window: *mut std::ffi::c_void) {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
    // Safety: the window belongs to the `SurfaceView`, which the activity keeps
    // alive for as long as this thread runs - it calls `stop` first. wgpu takes
    // it through the raw-window-handle route, which is the one an Android
    // native window is named by.
    let Some(window) = std::ptr::NonNull::new(window) else {
        return;
    };
    let handle =
        wgpu::rwh::RawWindowHandle::AndroidNdk(wgpu::rwh::AndroidNdkWindowHandle::new(window));
    let surface = match unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: wgpu::rwh::RawDisplayHandle::Android(
                wgpu::rwh::AndroidDisplayHandle::new(),
            ),
            raw_window_handle: handle,
        })
    } {
        Ok(surface) => surface,
        Err(error) => {
            note(&format!("no surface to draw into: {error}"));
            return;
        }
    };

    let adapter = match block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
    })) {
        Some(Ok(adapter)) => adapter,
        other => {
            note(&format!(
                "no adapter for the app's own surface (refused={})",
                other.is_some_and(|result| result.is_err())
            ));
            return;
        }
    };
    let (device, queue) = match block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())) {
        Some(Ok(pair)) => pair,
        other => {
            note(&format!(
                "no device for the app's own surface (refused={})",
                other.is_some_and(|result| result.is_err())
            ));
            return;
        }
    };

    let (width, height) = (WIDTH.load(Ordering::Acquire), HEIGHT.load(Ordering::Acquire));
    let Some(config) = surface.get_default_config(&adapter, width, height) else {
        note(&format!("the surface has no default config at {width}x{height}"));
        return;
    };
    // The whole question: whether this surface can carry alpha at all, and which
    // composite mode it would take. A window's surface and a `SurfaceView`'s are
    // not the same object to the compositor, and only this one is left to ask.
    let caps = surface.get_capabilities(&adapter);
    note(&format!(
        "surface offers alpha={:?} formats={:?}",
        caps.alpha_modes, caps.formats
    ));
    let mut config = wgpu::SurfaceConfiguration {
        alpha_mode: if caps
            .alpha_modes
            .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
        {
            wgpu::CompositeAlphaMode::PreMultiplied
        } else {
            caps.alpha_modes[0]
        },
        ..config
    };
    note(&format!(
        "using alpha={:?} format={:?}",
        config.alpha_mode, config.format
    ));
    surface.configure(&device, &config);

    // The whole point: egui's own renderer, on a surface the app owns.
    let context = egui::Context::default();
    let mut renderer = egui_wgpu::Renderer::new(&device, config.format, None, 1, false);
    note(&format!(
        "egui is drawing into the app's own SurfaceView: {}x{} {:?}",
        config.width, config.height, config.format
    ));

    let started = Instant::now();
    let mut frames: u64 = 0;
    while RUNNING.load(Ordering::Acquire) {
        let (width, height) = (WIDTH.load(Ordering::Acquire), HEIGHT.load(Ordering::Acquire));
        if width != config.width || height != config.height {
            config.width = width;
            config.height = height;
            surface.configure(&device, &config);
        }
        let frame = match surface.get_current_texture() {
            Ok(frame) => frame,
            Err(error) => {
                note(&format!("the surface stopped taking frames: {error}"));
                break;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());

        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [config.width, config.height],
            pixels_per_point: 1.0,
        };
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(config.width as f32, config.height as f32),
            )),
            time: Some(started.elapsed().as_secs_f64()),
            ..Default::default()
        };
        let output = context.run(input, |ctx| {
            // A panel with a fill of its own, in the bottom band: opaque where
            // it draws, transparent everywhere else. That is the whole
            // arrangement under test - the block has to be readable over the
            // picture, and the picture has to be visible around it.
            egui::TopBottomPanel::bottom("bar")
                .frame(
                    egui::Frame::default()
                        .fill(egui::Color32::from_rgb(24, 26, 32))
                        .inner_margin(egui::Margin::same(12)),
                )
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading("controls");
                        ui.label(format!("frame {frames}"));
                        let phase = (started.elapsed().as_secs_f32() * 0.8).sin() * 0.5 + 0.5;
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(240.0, 40.0),
                            egui::Sense::hover(),
                        );
                        ui.painter()
                            .rect_filled(rect, 8.0, egui::Color32::from_rgb(40, 120, 220));
                        ui.painter().rect_filled(
                            egui::Rect::from_min_size(
                                rect.min,
                                egui::vec2(rect.width() * phase, rect.height()),
                            ),
                            8.0,
                            egui::Color32::from_rgb(240, 200, 60),
                        );
                    });
                });
        });

        let jobs = context.tessellate(output.shapes, output.pixels_per_point);
        for (id, delta) in &output.textures_delta.set {
            renderer.update_texture(&device, &queue, *id, delta);
        }
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let prepared = renderer.update_buffers(&device, &queue, &mut encoder, &jobs, &screen);
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("egui"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            // Transparent on purpose: this surface sits over
                            // the picture, and whatever is not drawn here has
                            // to let it through.
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.0,
                                g: 0.0,
                                b: 0.0,
                                a: 0.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                })
                .forget_lifetime();
            renderer.render(&mut pass, &jobs, &screen);
        }
        queue.submit(prepared.into_iter().chain(std::iter::once(encoder.finish())));
        for id in &output.textures_delta.free {
            renderer.free_texture(id);
        }
        frame.present();

        frames = frames.wrapping_add(1);
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
    note(&format!("the probe stopped drawing after {frames} frames"));
}
