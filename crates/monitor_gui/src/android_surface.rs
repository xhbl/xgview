//! The wall on a `SurfaceView`, without eframe and without `android-activity`.
//!
//! The reason is in `priv/docs/android-surface-plan.md`: a `NativeActivity` gives
//! its whole window to native drawing, and that makes both of the things this
//! app needs impossible - a view above the picture, and a compositor layer below
//! it. An ordinary activity with `SurfaceView`s has neither problem, but it also
//! hands out no event loop and no window: this module is that loop.
//!
//! What it reproduces from eframe is small, because the app only ever used two
//! things from it - the egui context and the wgpu state - and never the frame
//! handle. Input is the activity's to hand over: keys, one event per touch
//! pointer, and a wheel, pushed from the UI thread into the next frame.
//!
//! Reached from `SurfaceActivity` through `monitor_android`'s JNI entries.

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::app::XgViewApp;
use crate::RunOptions;
use monitor_core::config::AppConfig;

/// Events the activity has handed over, waiting for the next frame.
///
/// The activity pushes from the UI thread and the loop drains at the top of each
/// frame. A mutex rather than a channel because the loop must never block: a
/// frame that waits for input is a frame that stops drawing.
static PENDING: Mutex<Vec<egui::Event>> = Mutex::new(Vec::new());

/// Whether the activity's window has focus, as last reported.
static FOCUSED: AtomicBool = AtomicBool::new(true);

/// The surface's size in pixels, as last reported.
static WIDTH: AtomicU32 = AtomicU32::new(1920);
static HEIGHT: AtomicU32 = AtomicU32::new(1080);

/// The display's density, so the interface is laid out at the size it should be.
static DENSITY: AtomicU32 = AtomicU32::new(2000);

/// The window the loop should be drawing into, or null while there is none.
///
/// The activity's `SurfaceView` hands a surface over, loses it, and gets another
/// one - a relayout is enough - so the loop is told which window is current and
/// attaches or detaches to match, rather than being started and stopped. The app
/// itself is built once and survives all of it.
static WINDOW: std::sync::atomic::AtomicPtr<c_void> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// Whether the drawing thread has been started. There is one for the process.
static STARTED: AtomicBool = AtomicBool::new(false);

/// The window of the bar's own surface, and its size in pixels.
///
/// A second surface rather than part of the wall's: the bar has to sit *over* a
/// decoded picture, and nothing here can be see-through - so it is given a
/// rectangle of its own, on top of the picture, and draws an opaque bar in it.
/// Covering the picture under the bar is the point, not a cost.
static BAR_WINDOW: std::sync::atomic::AtomicPtr<c_void> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
static BAR_WIDTH: AtomicU32 = AtomicU32::new(1920);
static BAR_HEIGHT: AtomicU32 = AtomicU32::new(140);

/// Takes the surface the bar is drawn into, or null when it went away.
pub fn set_bar_window(window: *mut c_void, width: i32, height: i32) {
    if width > 0 {
        BAR_WIDTH.store(width as u32, Ordering::Release);
    }
    if height > 0 {
        BAR_HEIGHT.store(height as u32, Ordering::Release);
    }
    BAR_WINDOW.store(window, Ordering::Release);
}

/// The two directories the activity handed over, kept for the one thread.
static DIRECTORIES: Mutex<Option<(PathBuf, PathBuf)>> = Mutex::new(None);

/// Hands one input event to the next frame.
pub fn push_event(event: egui::Event) {
    if let Ok(mut pending) = PENDING.lock() {
        // Bounded: a frame that has stopped being drawn must not grow a queue
        // nobody reads. The number is generous - a second of held keys.
        if pending.len() < 256 {
            pending.push(event);
        }
    }
}

/// Records whether the window has focus.
pub fn set_focused(focused: bool) {
    FOCUSED.store(focused, Ordering::Release);
}

/// Translates one Android key and hands it to the next frame.
///
/// `meta` is `KeyEvent.getMetaState()`; only the three modifiers the interface
/// can act on are read. A key this does not know is left alone rather than
/// guessed at, because a wrong guess is a control that does something else.
///
/// Returns whether the interface took it. One it did not is one the system
/// should still see - volume, the media keys, the language switch - and the
/// activity answers the key event with this, so that it does.
pub fn push_key(code: i32, pressed: bool, repeat: bool, meta: i32) -> bool {
    // `KeyEvent.META_*_ON`
    const SHIFT: i32 = 0x01;
    const ALT: i32 = 0x02;
    const CTRL: i32 = 0x1000;
    let Some(key) = crate::keymap::from_android(code) else {
        return false;
    };
    push_event(egui::Event::Key {
        key,
        physical_key: None,
        pressed,
        repeat,
        modifiers: egui::Modifiers {
            alt: meta & ALT != 0,
            ctrl: meta & CTRL != 0,
            shift: meta & SHIFT != 0,
            mac_cmd: false,
            command: meta & CTRL != 0,
        },
    });
    true
}

/// How many fingers are on the glass, so a tap and a pinch can be told apart.
static FINGERS: AtomicU32 = AtomicU32::new(0);

/// Translates one Android touch pointer and hands it to the next frame.
///
/// `phase` is the pointer's own phase rather than the event's action, because a
/// `ACTION_MOVE` carries every finger that is down: the activity sends one call
/// per pointer, so what arrives here is already per-finger.
///
/// Two kinds of event come out of one finger. The `Touch` is what a pinch is
/// read from - egui takes the ratio between two of them for itself - and the
/// pointer events are what a tap is read from: the wall's `track_tap` looks at
/// the primary button, so a finger that never moved the pointer would be a
/// picture nothing could be pressed on. With a second finger down the pointer
/// is let go and only the touches continue, or the two would fight over where
/// the pointer is.
pub fn push_touch(phase: i32, id: i32, x: f32, y: f32, force: f32) {
    let phase = match phase {
        0 => egui::TouchPhase::Start,
        1 => egui::TouchPhase::Move,
        2 => egui::TouchPhase::End,
        _ => egui::TouchPhase::Cancel,
    };
    let pixels_per_point = DENSITY.load(Ordering::Acquire) as f32 / 1000.0;
    let pos = egui::pos2(x / pixels_per_point, y / pixels_per_point);
    push_event(egui::Event::Touch {
        device_id: egui::TouchDeviceId(0),
        id: egui::TouchId(id as u64),
        phase,
        pos,
        force: Some(force),
    });

    let before = FINGERS.load(Ordering::Acquire);
    let after = match phase {
        egui::TouchPhase::Start => before.saturating_add(1),
        egui::TouchPhase::End | egui::TouchPhase::Cancel => before.saturating_sub(1),
        egui::TouchPhase::Move => before,
    };
    FINGERS.store(after, Ordering::Release);

    let modifiers = egui::Modifiers::default();
    let button = |pressed: bool| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers,
    };
    if before <= 1 {
        // One finger, and it is the only one - or it is the one being lifted.
        // The lift is the reason this cannot ask for `after == 1`: a finger
        // coming up leaves none behind, so `after` is 0, and the release below
        // was the one event that never arrived. A wall told nothing is a wall
        // that still has the pointer down: it keeps the page where the drag
        // left it - half a swipe, with the outgoing page showing on one side -
        // and it answers no further press, because the finger it believes is
        // on the glass is still there. That is exactly what a swipe did on a
        // phone, and it looked like touch had stopped working.
        match phase {
            egui::TouchPhase::Start => {
                push_event(egui::Event::PointerMoved(pos));
                push_event(button(true));
            }
            egui::TouchPhase::Move => push_event(egui::Event::PointerMoved(pos)),
            egui::TouchPhase::End | egui::TouchPhase::Cancel => push_event(button(false)),
        }
    } else if before == 1 && after == 2 {
        // A second finger arrived: the press is no longer a tap but half a
        // pinch, so the pointer is let go where it stands.
        push_event(button(false));
    }
}

/// Hands over a wheel or trackpad scroll, in pixels of travel.
pub fn push_scroll(dx: f32, dy: f32) {
    if dx == 0.0 && dy == 0.0 {
        return;
    }
    let pixels_per_point = DENSITY.load(Ordering::Acquire) as f32 / 1000.0;
    push_event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(dx / pixels_per_point, dy / pixels_per_point),
        modifiers: egui::Modifiers::default(),
    });
}

/// Records the surface's new size, in pixels.
pub fn resize(width: i32, height: i32) {
    if width > 0 {
        WIDTH.store(width as u32, Ordering::Release);
    }
    if height > 0 {
        HEIGHT.store(height as u32, Ordering::Release);
    }
}

/// Detaches the loop from its surface.
///
/// The app keeps running: a surface that goes away is usually a relayout on its
/// way back, and rebuilding the wall - its channels, its runtime, its audio -
/// for that would drop every camera for a second.
pub fn stop() {
    WINDOW.store(std::ptr::null_mut(), Ordering::Release);
}

/// Runs one future to completion on a runtime of its own.
fn block_on<F: std::future::Future>(future: F) -> Option<F::Output> {
    let runtime = tokio::runtime::Builder::new_current_thread().build().ok()?;
    Some(runtime.block_on(future))
}

/// Starts the wall on `window`, on a thread of its own.
///
/// `internal` and `external` are the two directories the activity hands out: the
/// private one the configuration lives in, and the shared one a transfer can put
/// a language pack in. They come from Java because there is no `AndroidApp` here
/// to ask.
pub fn start(window: *mut c_void, internal: String, external: String, density: f32) {
    if window.is_null() {
        return;
    }
    if density > 0.0 {
        DENSITY.store((density * 1000.0) as u32, Ordering::Release);
    }
    if let Ok(mut directories) = DIRECTORIES.lock() {
        *directories = Some((PathBuf::from(internal), PathBuf::from(external)));
    }
    WINDOW.store(window, Ordering::Release);
    if !STARTED.swap(true, Ordering::AcqRel) {
        std::thread::spawn(|| {
            if let Err(error) = run() {
                tracing::error!(target: "xgview", %error, "the wall could not start on its surface");
            }
        });
    }
}
fn run() -> anyhow::Result<()> {
    crate::init_android_logging();
    crate::install_panic_hook();

    let Some((internal, external)) = DIRECTORIES.lock().ok().and_then(|d| d.clone()) else {
        anyhow::bail!("the activity never handed its directories over");
    };
    // The same two directories `run_android` installs, and for the same reasons:
    // the private one is the only writable place guaranteed to exist, and the
    // shared one is where a transfer can drop a language pack.
    monitor_core::config::set_config_home(internal);
    crate::android::set_external_files(external.clone());
    monitor_codec::dump::set_dir(external.clone());

    let runtime = crate::build_runtime()?;
    let handle = runtime.handle().clone();

    let config_path = AppConfig::default_path();
    let config = AppConfig::load_or_default(&config_path);
    let options = RunOptions {
        config,
        config_path: Some(config_path.clone()),
        fullscreen: true,
        from_autostart: false,
        supervised: false,
        extra_lang_dir: Some(external.join("langs")),
    };

    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
    // Everything that outlives a surface. The activity's `SurfaceView` hands one
    // over, loses it, and gets another; the wall, its device and its textures
    // stay put across all of it.
    let mut built: Option<(egui::Context, egui_wgpu::RenderState, XgViewApp)> = None;
    let mut surface: Option<(wgpu::Surface<'_>, wgpu::SurfaceConfiguration)> = None;
    let mut attached: *mut c_void = std::ptr::null_mut();
    // The bar's own swapchain, over the picture - and its own renderer, because
    // two `egui::Context`s share nothing: each numbers its textures from
    // `Managed(0)`, so the font atlas of one would land on the other's.
    let mut bar: Option<(wgpu::Surface<'_>, wgpu::SurfaceConfiguration, egui_wgpu::Renderer)> =
        None;
    let mut bar_attached: *mut c_void = std::ptr::null_mut();
    let mut last_focus: Option<egui::Id> = None;
    let mut bar_open = false;
    // Whether the overlay had something to say last frame; see the bar's own
    // block below.
    let mut overlay_shown = false;
    // Starts hidden: the wall comes up on the grid, and the picture's view has
    // nothing to show until a viewport is magnified.
    let mut video_shown = false;
    // The rectangle the picture's view was last given, in physical pixels. A
    // relayout is not free - it resizes the surface the codec renders into - so
    // the rectangle is only sent when the wall's answer changes, and forgotten
    // when the view goes away so that the first frame of the next magnification
    // sends it again.
    let mut video_rect: Option<(f32, f32, f32, f32)> = None;
    // How much bigger than its box the picture's view is drawn, and where that
    // drawing starts, as last sent. Same terms as the box: a transform is free
    // to repeat but pointless to.
    let mut video_zoom: Option<(f32, f32, f32)> = None;
    crate::android::set_video_visible(false);

    // The bar has a screen of its own: a surface is a rectangle, so the only way
    // it can show the bar is for the bar to be all that is drawn in it. Same
    // theme, same faces, same pixels per point - a different screen.
    let bar_context = egui::Context::default();
    let bar_pixels = DENSITY.load(Ordering::Acquire) as f32 / 1000.0;
    bar_context.set_pixels_per_point(bar_pixels);
    crate::theme::install(&bar_context);
    crate::fonts::install_for(&bar_context, &monitor_i18n::current_language());

    let started = Instant::now();
    let mut last = Instant::now();
    // Set when the surface under us went away; the option itself cannot be
    // cleared where that is noticed, because the frame's work holds it borrowed.
    let mut lost = false;
    loop {
        if lost {
            lost = false;
            surface = None;
            attached = std::ptr::null_mut();
        }
        let window = WINDOW.load(Ordering::Acquire);
        if window.is_null() {
            // Detached: the surface went away and no new one has arrived. The
            // app stays built, so coming back is a surface, not a restart.
            if surface.is_some() {
                surface = None;
                attached = std::ptr::null_mut();
                tracing::info!(target: "xgview", "the wall let go of its surface");
            }
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        if built.is_none() || attached != window {
            let Some(window_ptr) = std::ptr::NonNull::new(window) else {
                continue;
            };
            let handle_ndk = wgpu::rwh::RawWindowHandle::AndroidNdk(
                wgpu::rwh::AndroidNdkWindowHandle::new(window_ptr),
            );
            // Safety: the activity keeps the `SurfaceView` alive until it calls
            // `stop`, and a new window replaces this one only after that.
            let fresh = unsafe {
                instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: wgpu::rwh::RawDisplayHandle::Android(
                        wgpu::rwh::AndroidDisplayHandle::new(),
                    ),
                    raw_window_handle: handle_ndk,
                })
            };
            let fresh = match fresh {
                Ok(fresh) => fresh,
                Err(error) => {
                    tracing::warn!(target: "xgview", %error, "no surface to draw into");
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
            };
            let (width, height) = (WIDTH.load(Ordering::Acquire), HEIGHT.load(Ordering::Acquire));

            if let Some((_, render_state, _)) = built.as_ref() {
                // A second surface on the same device: the wall carries on.
                let Some(fresh_config) =
                    fresh.get_default_config(&render_state.adapter, width, height)
                else {
                    tracing::warn!(target: "xgview", width, height, "the new surface has no default config");
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                };
                // The same alpha mode the first attach asks for: a surface that
                // comes back is one whose format the platform may have decided
                // again, and `Auto` picks opaque when it can.
                let caps = fresh.get_capabilities(&render_state.adapter);
                let fresh_config = wgpu::SurfaceConfiguration {
                    alpha_mode: if caps
                        .alpha_modes
                        .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
                    {
                        wgpu::CompositeAlphaMode::PreMultiplied
                    } else {
                        caps.alpha_modes[0]
                    },
                    ..fresh_config
                };
                fresh.configure(&render_state.device, &fresh_config);
                tracing::info!(target: "xgview", width, height, "the wall took a new surface");
                surface = Some((fresh, fresh_config));
                attached = window;
                continue;
            }

            let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&fresh),
                force_fallback_adapter: false,
            }))
            .ok_or_else(|| anyhow::anyhow!("no runtime for the adapter request"))?
            .map_err(|error| anyhow::anyhow!("no adapter for the activity's surface: {error}"))?;
            let (device, queue) =
                block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                    .ok_or_else(|| anyhow::anyhow!("no runtime for the device request"))?
                    .map_err(|error| {
                        anyhow::anyhow!("no device for the activity's surface: {error}")
                    })?;

            let Some(fresh_config) = fresh.get_default_config(&adapter, width, height) else {
                // A surface the activity has already let go of offers nothing,
                // and one of those arrives on the way to a good one. Waiting for
                // the next is the whole handling it needs.
                tracing::info!(
                    target: "xgview",
                    width,
                    height,
                    "a surface offered no config; waiting for the next"
                );
                std::thread::sleep(Duration::from_millis(50));
                continue;
            };
            // The picture is on a surface *under* this one, so this one has to
            // let it through where nothing is drawn. `Auto` picks opaque when it
            // can, which would put a black frame over the video however little
            // egui painted - the alpha mode has to be asked for.
            let caps = fresh.get_capabilities(&adapter);
            let fresh_config = wgpu::SurfaceConfiguration {
                alpha_mode: if caps
                    .alpha_modes
                    .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
                {
                    wgpu::CompositeAlphaMode::PreMultiplied
                } else {
                    caps.alpha_modes[0]
                },
                ..fresh_config
            };
            fresh.configure(&device, &fresh_config);
            tracing::info!(
                target: "xgview",
                alpha_mode = ?fresh_config.alpha_mode,
                supported_alpha = ?caps.alpha_modes,
                format = ?fresh_config.format,
                supported_formats = ?caps.formats,
                "the wall's surface"
            );

            let renderer = egui_wgpu::Renderer::new(&device, fresh_config.format, None, 1, false);
            let render_state = egui_wgpu::RenderState {
                adapter,
                available_adapters: Vec::new(),
                device,
                queue,
                target_format: fresh_config.format,
                renderer: std::sync::Arc::new(egui::mutex::RwLock::new(renderer)),
            };

            let context = egui::Context::default();
            let pixels_per_point = DENSITY.load(Ordering::Acquire) as f32 / 1000.0;
            context.set_pixels_per_point(pixels_per_point);

            let app = XgViewApp::new(
                &context,
                &render_state,
                options.clone(),
                config_path.clone(),
                handle.clone(),
            );
            tracing::info!(
                target: "xgview",
                width = fresh_config.width,
                height = fresh_config.height,
                pixels_per_point,
                "the wall is drawing on the activity's surface"
            );
            surface = Some((fresh, fresh_config));
            attached = window;
            built = Some((context, render_state, app));
            continue;
        }

        let Some((context, render_state, app)) = built.as_mut() else {
            continue;
        };
        let Some((surface_ref, surface_config)) = surface.as_mut() else {
            continue;
        };
        let device = render_state.device.clone();
        let queue = render_state.queue.clone();

        let (width, height) = (WIDTH.load(Ordering::Acquire), HEIGHT.load(Ordering::Acquire));
        if width != surface_config.width || height != surface_config.height {
            surface_config.width = width;
            surface_config.height = height;
            surface_ref.configure(&device, surface_config);
        }
        let frame_started = Instant::now();
        let frame = match surface_ref.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Timeout) => continue,
            Err(error) => {
                // The surface under us went away. Drop it and wait for the next
                // one rather than tearing the wall down.
                tracing::info!(target: "xgview", %error, "the surface stopped taking frames");
                lost = true;
                continue;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());

        let events = PENDING
            .lock()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default();
        let now = Instant::now();
        let predicted = now.duration_since(last).as_secs_f32().max(1.0 / 120.0);
        last = now;
        let pixels_per_point = DENSITY.load(Ordering::Acquire) as f32 / 1000.0;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(
                    surface_config.width as f32 / pixels_per_point,
                    surface_config.height as f32 / pixels_per_point,
                ),
            )),
            time: Some(started.elapsed().as_secs_f64()),
            predicted_dt: predicted,
            focused: FOCUSED.load(Ordering::Acquire),
            events,
            ..Default::default()
        };
        let output = context.run(input, |ctx| app.frame(ctx));

        // The picture's view is shown by the same answer that makes the wall
        // leave its area blank: a magnified viewport with a session really
        // rendering into that view. A `SurfaceView` keeps the last frame it was
        // given and is drawn over the wall, so one left visible while the wall
        // paints its own picture covers it - and one shown while empty is a
        // black rectangle over everything.
        let on_layer = app.picture_on_layer();
        if on_layer != video_shown {
            video_shown = on_layer;
            tracing::info!(target: "xgview", visible = on_layer, "the picture's view");
            crate::android::set_video_visible(on_layer);
            // Shown again, the view is placed again: the rectangle it was left
            // at belongs to whatever was magnified then, which may have been
            // another camera with another shape.
            video_rect = None;
            video_zoom = None;
        }
        // Where the picture is. The view the codec renders into is full screen
        // until it is told otherwise, and a full-screen surface stretches the
        // picture into the screen's shape - which is why the fit switch used to
        // change the wall's mind and nothing on the screen. Rounded to a pixel,
        // and sent only when the answer changes: this resizes the surface the
        // codec is drawing into, which is not something to do every frame.
        if on_layer {
            if let Some(rect) = app.picture_rect() {
                let left = (rect.min.x * pixels_per_point).round();
                let top = (rect.min.y * pixels_per_point).round();
                let right = (rect.max.x * pixels_per_point).round();
                let bottom = (rect.max.y * pixels_per_point).round();
                let wanted = (left, top, right, bottom);
                if video_rect != Some(wanted) {
                    video_rect = Some(wanted);
                    tracing::info!(
                        target: "xgview",
                        rect = ?wanted,
                        "the picture's rectangle"
                    );
                    crate::android::set_video_rect(left, top, right - left, bottom - top);
                }
            }
            // What the picture shows of itself: the window a pinch has left it
            // at, drawn as a scale of the view - bigger than the box, offset so
            // that the part being looked at is the part in it. The window is in
            // the picture's own coordinates, so where the picture's corner goes
            // is that window's corner times the box.
            if let Some(box_) = app.picture_rect() {
                let zoom = app.picture_zoom();
                let window = zoom.window();
                let scale = zoom.scale;
                let dx = (-window.min.x * box_.width() * pixels_per_point).round();
                let dy = (-window.min.y * box_.height() * pixels_per_point).round();
                let wanted = (scale, dx, dy);
                if video_zoom != Some(wanted) {
                    video_zoom = Some(wanted);
                    crate::android::set_video_zoom(scale, dx, dy);
                }
            }
        }

        let jobs = context.tessellate(output.shapes, output.pixels_per_point);
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [surface_config.width, surface_config.height],
            pixels_per_point,
        };
        {
            let mut renderer = render_state.renderer.write();
            for (id, delta) in &output.textures_delta.set {
                renderer.update_texture(&device, &queue, *id, delta);
            }
        }
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let prepared = render_state.renderer.write().update_buffers(
            &device,
            &queue,
            &mut encoder,
            &jobs,
            &screen,
        );
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("wall"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.0,
                                g: 0.0,
                                b: 0.0,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                })
                .forget_lifetime();
            render_state.renderer.read().render(&mut pass, &jobs, &screen);
        }
        queue.submit(prepared.into_iter().chain(std::iter::once(encoder.finish())));
        {
            let mut renderer = render_state.renderer.write();
            for id in &output.textures_delta.free {
                renderer.free_texture(id);
            }
        }
        frame.present();

        // The bar and the corner display, over the picture. Their own surface
        // and their own swapchain, presented only while there is something to
        // put on them - a grid has neither, and a transparent frame every
        // sixtieth of a second is a full-screen layer for nothing.
        let bar_window = BAR_WINDOW.load(Ordering::Acquire);
        if bar_window.is_null() {
            bar = None;
            bar_attached = std::ptr::null_mut();
        } else if bar.is_none() || bar_attached != bar_window {
            if let Some(window_ptr) = std::ptr::NonNull::new(bar_window) {
                let handle_ndk = wgpu::rwh::RawWindowHandle::AndroidNdk(
                    wgpu::rwh::AndroidNdkWindowHandle::new(window_ptr),
                );
                // Safety: the activity keeps the `SurfaceView` alive until it
                // calls `stop`, and a new window replaces this one only after.
                let fresh = unsafe {
                    instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                        raw_display_handle: wgpu::rwh::RawDisplayHandle::Android(
                            wgpu::rwh::AndroidDisplayHandle::new(),
                        ),
                        raw_window_handle: handle_ndk,
                    })
                };
                if let Ok(fresh) = fresh {
                    let (width, height) =
                        (BAR_WIDTH.load(Ordering::Acquire), BAR_HEIGHT.load(Ordering::Acquire));
                    if let Some(config) = fresh.get_default_config(&render_state.adapter, width, height)
                    {
                        fresh.configure(&render_state.device, &config);
                        let caps = fresh.get_capabilities(&render_state.adapter);
                        tracing::info!(
                            target: "xgview",
                            width = config.width,
                            height = config.height,
                            alpha_mode = ?config.alpha_mode,
                            supported_alpha = ?caps.alpha_modes,
                            format = ?config.format,
                            "the bar has a surface of its own"
                        );
                        // The overlay is drawn over the picture, so it is only
                        // an overlay if it can be see-through. A device whose
                        // surfaces offer nothing but `Opaque` would paint a
                        // black rectangle over the picture instead - the one
                        // failure this arrangement cannot absorb - and it says
                        // so here, because nothing else on that device would.
                        if caps
                            .alpha_modes
                            .iter()
                            .all(|mode| *mode == wgpu::CompositeAlphaMode::Opaque)
                        {
                            tracing::warn!(
                                target: "xgview",
                                "this surface cannot be see-through: the overlay will cover the picture"
                            );
                        }
                        let renderer = egui_wgpu::Renderer::new(
                            &render_state.device,
                            config.format,
                            None,
                            1,
                            false,
                        );
                        bar = Some((fresh, config, renderer));
                        bar_attached = bar_window;
                    }
                }
            }
        }
        // Something to say, or one frame left to say nothing in: the bar that
        // was up has to be painted away, and after that the surface is left
        // alone - it keeps its last frame, which is the empty one - until it is
        // wanted again. It is wanted only while the wall's frame is dropped;
        // otherwise the wall draws its own corners and bar.
        let overlay_wanted = app.picture_on_layer();
        let overlay_drawn = overlay_wanted || overlay_shown;
        overlay_shown = overlay_wanted;
        if let Some((bar_surface, bar_config, bar_renderer)) =
            bar.as_mut().filter(|_| overlay_drawn)
        {
            match bar_surface.get_current_texture() {
                Ok(bar_frame) => {
                    let view = bar_frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
                    let mut encoder =
                        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
                    // The bar's own frame. It reads no input - the presses are
                    // found in the wall's frame, where the pointer and the
                    // remote already are - so this is a paint, not an update.
                    let bar_rect = egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(
                            bar_config.width as f32 / pixels_per_point,
                            bar_config.height as f32 / pixels_per_point,
                        ),
                    );
                    let bar_input = egui::RawInput {
                        screen_rect: Some(bar_rect),
                        time: Some(started.elapsed().as_secs_f64()),
                        ..Default::default()
                    };
                    // The ring the remote puts on a control is the one thing a
                    // paint cannot work out for itself: egui draws it for
                    // whatever its own memory says has the focus, and the focus
                    // lives in the wall's context - the one that reads the
                    // arrows. So it is carried across, every frame, and the
                    // strip's copy of the bar lights the same control the
                    // wall's copy would.
                    if let Some(focused) = context.memory(|memory| memory.focused()) {
                        bar_context.memory_mut(|memory| memory.request_focus(focused));
                    }
                    {
                        let now_focused = context.memory(|memory| memory.focused());
                        if now_focused != last_focus {
                            last_focus = now_focused;
                            tracing::debug!(
                                target: "xgview::tilebar",
                                focused = ?now_focused,
                                "the focus the strip was given"
                            );
                        }
                    }
                    // The picture's rectangle in the strip's own coordinates: the
                    // screen's size, shifted up by the strip's height so that the
                    // picture's bottom edge is the strip's. The bar is laid out
                    // against it, which is where the wall lays it out too - so
                    // what is drawn here is in the place the wall's copy takes
                    // presses.
                    //
                    // The *screen*, not the picture the fit switch leaves inside
                    // it: the corners and the bar are laid out against the tile,
                    // here and in the wall's own copy, and the wall is what reads
                    // the presses. Laid out against a letterboxed picture, the bar
                    // would be drawn where no press could reach it.
                    let screen_w = WIDTH.load(Ordering::Acquire) as f32 / pixels_per_point;
                    let screen_h = HEIGHT.load(Ordering::Acquire) as f32 / pixels_per_point;
                    let strip_h = bar_config.height as f32 / pixels_per_point;
                    let picture = egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(screen_w, screen_h),
                    )
                    .translate(egui::vec2(0.0, strip_h - screen_h));
                    let open = app.bar_open();
                    if open != bar_open {
                        bar_open = open;
                        tracing::info!(
                            target: "xgview",
                            open,
                            strip = ?bar_rect,
                            "the bar's own frame"
                        );
                    }
                    let bar_output =
                        bar_context.run(bar_input, |ctx| app.draw_overlay_only(ctx, picture));
                    let bar_jobs =
                        bar_context.tessellate(bar_output.shapes, bar_output.pixels_per_point);
                    let bar_screen = egui_wgpu::ScreenDescriptor {
                        size_in_pixels: [bar_config.width, bar_config.height],
                        pixels_per_point,
                    };
                    {
                        for (id, delta) in &bar_output.textures_delta.set {
                            bar_renderer.update_texture(&device, &queue, *id, delta);
                        }
                    }
                    let prepared = bar_renderer.update_buffers(
                        &device,
                        &queue,
                        &mut encoder,
                        &bar_jobs,
                        &bar_screen,
                    );
                    {
                        let mut pass = encoder
                            .begin_render_pass(&wgpu::RenderPassDescriptor {
                                label: Some("bar"),
                                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                    view: &view,
                                    resolve_target: None,
                                    ops: wgpu::Operations {
                                        // Nothing under the bar is the bar's:
                                        // the strip is see-through wherever it
                                        // has nothing to say, so the picture
                                        // runs under it as it does everywhere
                                        // else.
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
                        bar_renderer.render(&mut pass, &bar_jobs, &bar_screen);
                    }
                    queue.submit(prepared.into_iter().chain(std::iter::once(encoder.finish())));
                    for id in &bar_output.textures_delta.free {
                        bar_renderer.free_texture(id);
                    }
                    bar_frame.present();
                }
                Err(error) => {
                    tracing::info!(target: "xgview", %error, "the bar's surface stopped taking frames");
                    bar = None;
                    bar_attached = std::ptr::null_mut();
                }
            }
        }

        // The pictures arriving from the cameras are not something egui knows
        // about, so every frame is drawn whether or not one was asked for. The
        // wait is what is left of a sixtieth of a second rather than a sixtieth
        // on top of the frame - a fixed sleep after the work is a wall that
        // cannot reach sixty, and a key that waits for it.
        let spent = frame_started.elapsed();
        let budget = Duration::from_micros(16_667);
        if spent < budget {
            std::thread::sleep(budget - spent);
        }
    }
}
