//! The soft keyboard on Android, and the path its text takes into egui.
//!
//! Nothing below the application carries a soft keyboard's text here: winit's
//! Android backend forwards `set_ime_allowed` to `AndroidApp::show_soft_input`
//! and has no input method events at all, so a keyboard raised that way would
//! have nowhere to type. The text is taken the way the game ports take it - a
//! view in the activity whose input connection forwards every commit to
//! native (see `MainActivity.installTextInput`) - and lands here, where it is
//! handed to egui as the ordinary events a text field already understands.
//!
//! Everything in this module is therefore only compiled on Android.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use egui::{Event, Id, Key, Modifiers};
use jni::objects::{GlobalRef, JClass, JString, JValue};
use jni::sys::jint;
use jni::{JNIEnv, JavaVM};

/// One thing the keyboard did, waiting for the next frame to pick it up.
enum Typed {
    Text(String),
    Backspace(usize),
    Enter,
    /// Ctrl+A: the field's own select-all, so that what is typed next replaces it.
    SelectAll,
    /// Text that was pasted, which a field takes in one piece.
    Paste(String),
}

/// What `MainActivity` has reported since the last frame.
///
/// It is a queue rather than a direct call into egui because the activity
/// reports whenever the input method feels like it, which is not necessarily
/// while a frame is being built.
static TYPED: Mutex<Vec<Typed>> = Mutex::new(Vec::new());

/// The keyboard went away on its own - Back, or the input method's own hide
/// button - since the application last looked. See [`take_hidden`].
static HIDDEN: AtomicBool = AtomicBool::new(false);

/// The egui context, kept from the first frame so that what arrives from Java
/// can wake the loop: the wall repaints on a timer, which would leave a
/// keystroke or a hidden keyboard waiting for the next tick.
static CONTEXT: OnceLock<egui::Context> = OnceLock::new();

/// The Java VM, kept from the one call the activity makes when it is ready.
static VM: OnceLock<JavaVM> = OnceLock::new();

/// The activity's own class, kept from the same call.
///
/// It cannot be looked up later: `android_main` runs on a thread the runtime did
/// not create, where `FindClass` only reaches the system class loader and knows
/// nothing of the application's classes. The pending `ClassNotFoundException`
/// that leaves behind aborts the process on the next JNI call - so the activity
/// hands its class over on the way in instead, and that is what is used here.
static ACTIVITY: OnceLock<GlobalRef> = OnceLock::new();

/// What the activity was last asked for: whether the keyboard is wanted,
/// which text field wanted it, and whether that field holds several lines.
///
/// Keyed on all three, not on the answer alone: moving the focus from one
/// field straight to the next keeps the answer at "wanted", but the keyboard
/// has to be asked for again - with the new field's action key - or it does
/// not come back for the second field.
static LAST: Mutex<Option<(bool, Option<Id>, bool)>> = Mutex::new(None);

/// Hands everything the keyboard typed since the last frame to egui.
///
/// Called at the top of the frame, before any widget is drawn, so that a text
/// field reads the text in the same frame it arrived in.
pub fn drain(ctx: &egui::Context) {
    CONTEXT.get_or_init(|| ctx.clone());
    let typed = match TYPED.lock() {
        Ok(mut queue) => std::mem::take(&mut *queue),
        // A poisoned queue means another thread panicked while holding it; the
        // keystrokes are not worth panicking the renderer over.
        Err(_) => return,
    };
    if typed.is_empty() {
        return;
    }
    ctx.input_mut(|input| {
        for item in typed {
            match item {
                Typed::Text(text) => input.events.push(Event::Text(text)),
                Typed::Paste(text) => input.events.push(Event::Paste(text)),
                Typed::SelectAll => input.events.push(Event::Key {
                    key: Key::A,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers { ctrl: true, command: true, ..Default::default() },
                }),
                Typed::Backspace(0) => {}
                Typed::Backspace(count) => {
                    for _ in 0..count {
                        input.events.push(Event::Key {
                            key: Key::Backspace,
                            physical_key: None,
                            pressed: true,
                            repeat: false,
                            modifiers: Default::default(),
                        });
                    }
                }
                // Not a newline: the fields are single line, so Enter is what
                // takes the text and gives the focus back - the same thing the
                // keyboard's own Done key does on the desktop.
                Typed::Enter => input.events.push(Event::Key {
                    key: Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Default::default(),
                }),
            }
        }
    });
}

/// Asks the activity to raise or drop the keyboard.
///
/// Called with the state of the focus every frame, and only passed on when it
/// changes - see [`LAST`] for why the focused field and its shape are part of
/// that. `multiline` is what the input method builds its action key from: a
/// single-line field gets a Done that takes the text, a multi-line one gets a
/// newline, because Enter cannot both break the line and finish the field.
pub fn set_wanted(wanted: bool, focused: Option<Id>, multiline: bool) {
    match LAST.lock() {
        Ok(last) if *last == Some((wanted, focused, multiline)) => return,
        Ok(mut last) => *last = Some((wanted, focused, multiline)),
        Err(_) => return,
    }
    let (Some(vm), Some(class)) = (VM.get(), ACTIVITY.get()) else {
        // The activity has not called `nativeReady` yet: nothing to talk to.
        return;
    };
    if wanted {
        // A keyboard that went away before this ask is old news.
        HIDDEN.store(false, Ordering::SeqCst);
    }
    // Permanently, not `attach_current_thread`: this runs on the UI thread,
    // which the runtime owns. The plain form returns a guard that detaches on
    // drop, and detaching a thread the runtime still believes it owns brings
    // the process down on the next Java call.
    let Ok(mut env) = vm.attach_current_thread_permanently() else {
        return;
    };
    let _ = env.call_static_method(
        class,
        "setKeyboardWanted",
        "(ZZ)V",
        &[JValue::Bool(wanted as u8), JValue::Bool(multiline as u8)],
    );
}

/// Selects the whole text of the field being typed into.
///
/// What the floating input box's buttons call: they act on the field through
/// the same queue the keyboard does, so a field cannot tell the difference.
pub fn select_all() {
    push(Typed::SelectAll);
}

/// Replaces nothing and inserts `text` as a paste.
pub fn paste(text: String) {
    if !text.is_empty() {
        push(Typed::Paste(text));
    }
}

/// Empties the field: select everything, then delete the selection.
pub fn clear() {
    push(Typed::SelectAll);
    push(Typed::Backspace(1));
}

/// Queues one thing the keyboard did.
fn push(item: Typed) {
    if let Ok(mut queue) = TYPED.lock() {
        queue.push(item);
    }
    wake();
}

/// Asks for a frame, so that what was just queued is read now.
fn wake() {
    if let Some(ctx) = CONTEXT.get() {
        ctx.request_repaint();
    }
}

/// The keyboard went away without the application asking.
///
/// Called from the JNI entry point in `monitor_android`.
pub fn java_keyboard_hidden() {
    HIDDEN.store(true, Ordering::SeqCst);
    wake();
}

/// Whether the keyboard went away on its own since the last call.
///
/// Back while the keyboard is up, and the input method's hide button, are
/// handled by the input method: the application sees no key. Without this the
/// field would stay marked as being edited - the floating input box on screen,
/// and no way to raise the keyboard again by touching the field, because the
/// activity was never told it had gone.
pub fn take_hidden() -> bool {
    HIDDEN.swap(false, Ordering::SeqCst)
}

/// The activity is up and can be talked to.
///
/// These four are called from the JNI entry points in `monitor_android`. The
/// `#[no_mangle]` symbols themselves have to live in the cdylib crate: a
/// symbol exported from a dependency is hidden by the linker, and the activity
/// dies of `UnsatisfiedLinkError` before the first frame.
pub fn java_ready(env: &JNIEnv, class: &JClass) {
    if let Ok(vm) = env.get_java_vm() {
        let _ = VM.set(vm);
    }
    if let Ok(class) = env.new_global_ref(class) {
        let _ = ACTIVITY.set(class);
    }
}

/// The keyboard committed some text.
pub fn java_commit(env: &mut JNIEnv, text: &JString) {
    let Ok(text) = env.get_string(text) else {
        return;
    };
    let text: String = text.into();
    if !text.is_empty() {
        push(Typed::Text(text));
    }
}

/// The keyboard deleted some characters.
pub fn java_backspace(count: jint) {
    if count > 0 {
        push(Typed::Backspace(count as usize));
    }
}

/// The keyboard's Done / Enter key was pressed.
pub fn java_enter() {
    push(Typed::Enter);
}

/// The activity's class, so that another Android-side helper can call into it -
/// see [`crate::android`]. `None` until the activity has reported itself ready.
pub(crate) fn activity_class() -> Option<&'static GlobalRef> {
    ACTIVITY.get()
}

/// The Java VM, as above.
pub(crate) fn vm() -> Option<&'static JavaVM> {
    VM.get()
}
