package com.xhbl.xgview;

import android.app.AlarmManager;
import android.app.NativeActivity;
import android.app.PendingIntent;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.ContentResolver;
import android.content.ContentUris;
import android.content.ContentValues;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.pm.ResolveInfo;
import android.database.Cursor;
import android.graphics.Insets;
import android.media.MediaScannerConnection;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Environment;
import android.os.PowerManager;
import android.os.SystemClock;
import android.provider.MediaStore;
import android.provider.Settings;
import android.text.InputType;
import android.view.KeyEvent;
import android.view.View;
import android.view.ViewGroup;
import android.view.Window;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.view.WindowManager;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;

import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.lang.reflect.Method;
import java.nio.charset.StandardCharsets;

/**
 * XGView's activity: the {@link NativeActivity} the whole UI runs in, with the
 * status bar kept hidden, the navigation bar left in place, and a soft keyboard
 * that can be typed into.
 *
 * <p>The status bar has to be hidden from Java: a {@code NativeActivity} tells
 * the native library where to draw but says nothing about the system UI. The
 * navigation bar is deliberately *not* hidden. On a landscape phone it is a
 * strip along the right edge; hiding it lets the wall run under it, and keeping
 * it means the wall is narrower - which is what the viewer wants, with the
 * native side mirroring the same width on the left so the wall sits centred.
 * Its width is reported with the window insets; a television and an external
 * display have no navigation bar, so there it is zero and the wall stays edge
 * to edge.
 *
 * <p>Note that a device using gesture navigation has no bar to hide, and keeps
 * a back-gesture band along each edge of the screen instead. That band is not
 * the window's to take; the toolbar keeps a margin on its right for it.
 *
 * <p>The keyboard is the other half of what Java has to do. Nothing below the
 * application carries a soft keyboard's text on Android - winit's backend
 * forwards {@code set_ime_allowed} and has no input method events at all - so
 * the typing is taken the way the game ports take it: a one pixel view that
 * the input method can attach to, whose input connection forwards every commit
 * to the native side, which hands it to egui as ordinary text events. See
 * {@code monitor_gui::keyboard}.
 */
public class MainActivity extends NativeActivity {

    /** The activity the native side talks back to, set in {@link #onCreate}. */
    private static MainActivity instance;

    /** Whether {@code monitor_android} is loaded, so a native method may be called. */
    private boolean nativeLoaded;

    /**
     * Whether the navigation bar keeps its strip, or the wall hides it and draws
     * full screen. Set from the native side; see {@link #setReserveNavigationBar}
     * and {@link #applySystemBars}.
     */
    private boolean reserveNavigationBar = true;

    /** Whether the activity was started by the boot receiver, read from the launch intent. */
    private static boolean fromAutostart = false;

    /** The one pixel view the input method types into. */
    private View inputView;

    private InputMethodManager inputMethod;

    /** Whether the native side wants the keyboard up, as last told by {@link #setKeyboardWanted}. */
    private boolean keyboardWanted;

    /**
     * Whether the field being edited holds several lines, as last told by
     * {@link #setKeyboardWanted}. It decides the input method's action key: a
     * single-line field gets a Done that takes the text, a multi-line one gets
     * a newline, because Enter cannot both break the line and finish the field.
     */
    private boolean keyboardMultiline;

    /**
     * Whether the keyboard has been seen on screen since it was last asked for.
     *
     * <p>A report that it is not showing means nothing until it has shown: the
     * input method takes a moment to come up, and the first answer is always
     * "not yet".
     */
    private boolean imeSeenVisible;

    /** The hidden call that reports the keyboard's height, found once; see {@link #imeHeight}. */
    private Method imeHeightMethod;

    /** Asks the input method how tall it is, on the Android versions that have no insets for it. */
    private final Runnable imePoll = new Runnable() {
        @Override
        public void run() {
            if (!keyboardWanted || Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                return;
            }
            final int height = imeHeight();
            if (height < 0) {
                // This device does not answer: nothing more to learn by asking.
                return;
            }
            onImeVisibility(height > 0);
            inputView.postDelayed(this, 300);
        }
    };

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        Intent launch = getIntent();
        fromAutostart = launch != null && launch.getBooleanExtra("com.xhbl.xgview.FROM_AUTOSTART", false);
        // Before super.onCreate, and so before android-activity starts the
        // native thread: the marker it writes is what the native side reads to
        // choose its backend, and the choice has to be made before the renderer
        // exists. Anything after super.onCreate races the native thread.
        applyVulkanGate();
        super.onCreate(savedInstanceState);
        instance = this;
        applySystemBars();
        installTextInput();
        // The activity loads the native library itself, with a bare `dlopen`
        // that leaves its symbols out of the global scope the runtime searches
        // when it resolves a `native` method. Without this second load - which
        // is what registers the library with this class loader - the call below
        // dies of `UnsatisfiedLinkError` before the first frame.
        System.loadLibrary("monitor_android");
        nativeLoaded = true;
        nativeReady();
    }

    @Override
    protected void onDestroy() {
        // A wake lock is not released by losing the object that holds it, and
        // the native side only lets go on its own quit path - so an activity
        // the system destroys has to release it here, or it is a leak the
        // battery pays for.
        releaseWakeLock();
        if (instance == this) {
            instance = null;
        }
        super.onDestroy();
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (hasFocus) {
            applySystemBars();
        }
    }

    /** Whether the activity was started by the boot receiver; see {@link BootReceiver}. */
    public static boolean getFromAutostart() {
        return fromAutostart;
    }

    /**
     * Called from the native side when a text field takes or loses the focus, so
     * that the keyboard follows what the viewer is doing in the UI.
     *
     * <p>{@code multiline} is the shape of the field being edited: see
     * {@link #keyboardMultiline}.
     */
    public static void setKeyboardWanted(final boolean wanted, final boolean multiline) {
        final MainActivity self = instance;
        if (self == null || self.inputView == null) {
            return;
        }
        self.runOnUiThread(() -> {
            // Read before they are overwritten: the action key can only be
            // changed by remaking the input connection, and that is worth doing
            // only when the keyboard is already up on a field of the other kind.
            final boolean wasWanted = self.keyboardWanted;
            final boolean optionsChanged = self.keyboardMultiline != multiline;
            // Told before the keyboard is asked to move, so that what it does
            // in answer to this is not taken for the viewer putting it away.
            self.keyboardWanted = wanted;
            self.keyboardMultiline = multiline;
            self.imeSeenVisible = false;
            self.inputView.removeCallbacks(self.imePoll);
            if (wanted) {
                if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) {
                    self.inputView.postDelayed(self.imePoll, 600);
                }
                self.inputView.setFocusable(true);
                self.inputView.setFocusableInTouchMode(true);
                self.inputView.requestFocus();
                self.inputMethod.showSoftInput(self.inputView, 0);
                if (wasWanted && optionsChanged) {
                    // The action key of an input method is fixed when its input
                    // connection is made, so a move between a single-line and a
                    // multi-line field while the keyboard stays up needs the
                    // connection remade for the new editor options.
                    self.inputMethod.restartInput(self.inputView);
                }
            } else {
                self.inputMethod.hideSoftInputFromWindow(self.inputView.getWindowToken(), 0);
                self.inputView.clearFocus();
                // Focusable only while the keyboard is wanted. Left focusable,
                // the window hands this view the focus the moment it opens and
                // the input method comes up over the wall unasked - and while
                // it has the focus it takes the remote's buttons too.
                self.inputView.setFocusable(false);
                self.inputView.setFocusableInTouchMode(false);
            }
        });
    }

    /** Text the input method has spelled out but not settled on yet. */
    private String composing = "";

    /**
     * The input method spelled something out: whatever it had composed so far
     * is replaced by this.
     *
     * <p>A word being typed arrives this way, one keystroke at a time, and is
     * only committed once the input method is sure of it. The native side owns
     * the string, so the previous attempt is taken back out of it and the new
     * one put in, which is what makes the field show what is being typed
     * rather than nothing until the very end.
     */
    private void onComposing(String text) {
        if (!composing.isEmpty()) {
            nativeBackspace(composing.length());
        }
        composing = text;
        if (!text.isEmpty()) {
            nativeCommit(text);
        }
    }

    /** The input method settled on something, and it stays. */
    private void onCommit(String text) {
        if (!composing.isEmpty()) {
            final String previous = composing;
            composing = "";
            if (text.contentEquals(previous)) {
                // Already in the field, put there by `onComposing`.
                return;
            }
            nativeBackspace(previous.length());
        }
        if (!text.isEmpty()) {
            nativeCommit(text);
        }
    }

    /** The keyboard is, or is not, on screen. Acts only on the way from showing to gone. */
    private void onImeVisibility(boolean visible) {
        if (!keyboardWanted) {
            imeSeenVisible = false;
            return;
        }
        if (visible) {
            imeSeenVisible = true;
        } else if (imeSeenVisible) {
            keyboardDismissed();
        }
    }

    /** The keyboard went away without the native side asking. */
    private void keyboardDismissed() {
        imeSeenVisible = false;
        nativeKeyboardHidden();
    }

    /** The input method's height in pixels, 0 when hidden, -1 when this device will not say. */
    private int imeHeight() {
        try {
            if (imeHeightMethod == null) {
                imeHeightMethod = InputMethodManager.class.getMethod("getInputMethodWindowVisibleHeight");
            }
            return (Integer) imeHeightMethod.invoke(inputMethod);
        } catch (Throwable unsupported) {
            return -1;
        }
    }

    /**
     * Applies the bars for the mode {@link #setReserveNavigationBar} chose, and
     * reports the navigation bar's width to the native side.
     *
     * <p>With the bar reserved (the default) the wall is drawn edge to edge under
     * the bars - {@code setDecorFitsSystemWindows(false)}, and the matching
     * layout flags on older Android - but only the status bar is hidden. The
     * navigation bar stays, so a landscape phone keeps its strip along one side;
     * the insets say how wide that strip is, and the wall leaves that width free
     * on the same side. A television and an external display have no navigation
     * bar, so there the insets are zero and the wall is edge to edge.
     *
     * <p>With the bar hidden (immersive) both bars are hidden as they were, and
     * nothing is reserved.
     *
     * <p>Also the only place the keyboard's own coming and going is seen, on
     * Android 11 and newer - before that it is asked its height, see {@link
     * #imePoll}.
     */
    private void applySystemBars() {
        final Window window = getWindow();
        // Reported on every change: the navigation bar can move to the other
        // side, appear, or go as the keyboard comes and goes.
        window.getDecorView().setOnApplyWindowInsetsListener((view, insets) -> {
            final int left;
            final int right;
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                final Insets bars = insets.getInsets(WindowInsets.Type.navigationBars());
                left = bars.left;
                right = bars.right;
                onImeVisibility(insets.isVisible(WindowInsets.Type.ime()));
            } else {
                left = insets.getSystemWindowInsetLeft();
                right = insets.getSystemWindowInsetRight();
            }
            // The library is loaded late in `onCreate`, and the first insets can
            // arrive before that. Nothing is reserved while the bar is hidden.
            if (nativeLoaded) {
                nativeInsets(reserveNavigationBar ? left : 0, reserveNavigationBar ? right : 0);
            }
            return view.onApplyWindowInsets(insets);
        });
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            // Android 11 and newer. The window extends under the bars.
            window.setDecorFitsSystemWindows(false);
            final WindowInsetsController controller = window.getInsetsController();
            if (controller != null) {
                if (reserveNavigationBar) {
                    // Only the status bar goes. The navigation bar has to be
                    // asked back explicitly: it was hidden by the immersive mode
                    // - hiding something else does not bring it out - and the
                    // transient behaviour is dropped with it.
                    controller.setSystemBarsBehavior(WindowInsetsController.BEHAVIOR_DEFAULT);
                    controller.show(WindowInsets.Type.navigationBars());
                    controller.hide(WindowInsets.Type.statusBars());
                } else {
                    // The transient behaviour brings the bars back as an overlay
                    // for a moment rather than resizing the window, which would
                    // drop a frame of every video on screen.
                    controller.setSystemBarsBehavior(
                            WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE);
                    controller.hide(WindowInsets.Type.systemBars());
                }
            }
        } else if (reserveNavigationBar) {
            // Android 10 and older: the flags, which are also what the manifest's
            // fullscreen theme sets before the first frame. The navigation bar is
            // laid out under (`LAYOUT_HIDE_NAVIGATION`) but not hidden, so it
            // keeps its strip while the wall runs beneath it.
            window.getDecorView().setSystemUiVisibility(
                    View.SYSTEM_UI_FLAG_FULLSCREEN
                            | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                            | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                            | View.SYSTEM_UI_FLAG_LAYOUT_STABLE);
        } else {
            window.getDecorView().setSystemUiVisibility(
                    View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                            | View.SYSTEM_UI_FLAG_FULLSCREEN
                            | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                            | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                            | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                            | View.SYSTEM_UI_FLAG_LAYOUT_STABLE);
        }
        // A switch between the two modes changes what may be reserved, and the
        // bars coming or going is what reports it - ask for a fresh dispatch.
        window.getDecorView().requestApplyInsets();
    }

    /**
     * Called from the native side when the setting changes: whether the
     * navigation bar keeps its strip ({@code true}) or the wall hides it and
     * draws full screen ({@code false}).
     */
    public static void setReserveNavigationBar(final boolean reserve) {
        final MainActivity self = instance;
        if (self == null) {
            return;
        }
        self.runOnUiThread(() -> {
            if (self.reserveNavigationBar == reserve) {
                return;
            }
            self.reserveNavigationBar = reserve;
            self.applySystemBars();
        });
    }

    // ------------------------------------------------------- graphics backend
    //
    // Which renderer the native side may build is decided here, before the
    // native thread starts, because a backend cannot be swapped once the event
    // loop exists. One old driver family needs it: a device whose Vulkan is
    // older than 1.1 - Adreno on Android 8.x, Vulkan 1.0 - enumerates an
    // adapter, hands out a device, then loses that device while the first
    // pipeline is built, which panics inside wgpu and, without the marker
    // below, costs a launch. The version, unlike the behaviour, is knowable in
    // advance, and `PackageManager` is where it is known. See
    // `monitor_gui::run_android`.

    /** `VK_MAKE_VERSION(1, 1, 0)`: the oldest Vulkan this viewer will try. */
    private static final int VULKAN_1_1 = 0x401000;

    /** The file whose presence tells the native side not to try Vulkan. */
    private static final String VULKAN_GATE_MARKER = "xgview/wgpu-vulkan-unsupported";

    /**
     * Marks the launch for the GL backend when the device's Vulkan is too old
     * to keep a device, and clears the mark when it is new enough.
     *
     * <p>Called from {@link #onCreate} before {@code super.onCreate}, so that
     * the marker is on disk before the native thread - which android-activity
     * starts during {@code super.onCreate} - can look for it. The native side
     * never asks about versions itself: {@code PackageManager} is a Java API,
     * and this is the one moment the answer can be acted on without racing the
     * renderer.
     *
     * <p>The delete is for a device whose driver was updated out from under the
     * marker: the gate is not meant to outlive the reason for it.
     */
    private void applyVulkanGate() {
        final PackageManager packages = getPackageManager();
        final boolean vulkan11 = packages != null
                && packages.hasSystemFeature(PackageManager.FEATURE_VULKAN_HARDWARE_VERSION, VULKAN_1_1);
        final File marker = new File(getFilesDir(), VULKAN_GATE_MARKER);
        if (vulkan11) {
            if (marker.exists() && !marker.delete()) {
                android.util.Log.w("xgview", "cannot clear " + marker);
            }
            return;
        }
        android.util.Log.i("xgview",
                "no Vulkan 1.1 on this device; the GL backend is marked for this launch");
        try {
            final File dir = marker.getParentFile();
            if (dir != null && !dir.isDirectory() && !dir.mkdirs()) {
                android.util.Log.w("xgview", "cannot create " + dir);
                return;
            }
            if (!marker.exists() && !marker.createNewFile()) {
                android.util.Log.w("xgview", "cannot create " + marker);
            }
        } catch (IOException error) {
            android.util.Log.w("xgview", "cannot write the Vulkan gate marker", error);
        }
    }

    /**
     * Brings the activity back up a moment from now, in a fresh process.
     *
     * <p>Called from the native side when a panic in the renderer was caught and
     * the process is about to exit: an activity cannot start itself from a
     * process that is going away, so the relaunch is handed to
     * {@code AlarmManager}, whose alarm is held by the system and fires after
     * this process is gone. The next launch reads the marker that panic wrote
     * and takes the GL backend.
     *
     * <p>Called on the native thread, and must return before that thread exits
     * the process - so it does no posting to the UI thread. Everything it
     * touches (alarms, pending intents, the package manager) is usable from any
     * thread, and posting would only risk the process dying before the alarm
     * was set.
     */
    public static void restartSoon() {
        final MainActivity self = instance;
        if (self == null) {
            return;
        }
        try {
            final Intent launch = self.getPackageManager()
                    .getLaunchIntentForPackage(self.getPackageName());
            if (launch == null) {
                android.util.Log.w("xgview", "no launcher intent to restart with");
                return;
            }
            launch.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK
                    | Intent.FLAG_ACTIVITY_CLEAR_TOP
                    | Intent.FLAG_ACTIVITY_SINGLE_TOP);
            final PendingIntent pending = PendingIntent.getActivity(self, 0, launch,
                    PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
            final AlarmManager alarms =
                    (AlarmManager) self.getSystemService(Context.ALARM_SERVICE);
            if (alarms == null) {
                android.util.Log.w("xgview", "no alarm service to restart with");
                return;
            }
            // A plain `set` rather than an exact alarm: an exact one needs the
            // permission Android 12 added, and a few hundred milliseconds either
            // way does not matter for bringing the wall back.
            alarms.set(AlarmManager.ELAPSED_REALTIME,
                    SystemClock.elapsedRealtime() + 700, pending);
            android.util.Log.i("xgview", "restarting XGView on the GL backend");
        } catch (RuntimeException error) {
            android.util.Log.w("xgview", "cannot schedule a restart", error);
        }
    }

    // ----------------------------------------------------------------- power
    //
    // The two halves of `monitor_core::power` on this platform, called from
    // `monitor_gui::android`. `FLAG_KEEP_SCREEN_ON` is a window flag and needs
    // no permission; a `PARTIAL_WAKE_LOCK` keeps the CPU running with the
    // screen off, and uses the WAKE_LOCK permission the manifest declares.

    /** The CPU wake lock, held only while the native side asks for it. */
    private static PowerManager.WakeLock wakeLock;

    /**
     * Keeps the CPU running. The screen may still go off, which is the point of
     * this half: a wall whose display is dark keeps pulling its streams.
     */
    public static void setPreventSleep(final boolean wanted) {
        final MainActivity self = instance;
        if (self == null) {
            return;
        }
        self.runOnUiThread(() -> {
            if (!wanted) {
                releaseWakeLock();
                return;
            }
            if (wakeLock == null) {
                final PowerManager manager =
                        (PowerManager) self.getSystemService(Context.POWER_SERVICE);
                if (manager == null) {
                    return;
                }
                wakeLock = manager.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "xgview:wall");
            }
            if (!wakeLock.isHeld()) {
                wakeLock.acquire();
            }
        });
    }

    /**
     * Keeps the screen on.
     *
     * <p>A window flag rather than a wake lock, so it goes away with the window
     * and needs no matching release.
     */
    public static void setKeepScreenOn(final boolean wanted) {
        final MainActivity self = instance;
        if (self == null) {
            return;
        }
        self.runOnUiThread(() -> {
            if (wanted) {
                self.getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            } else {
                self.getWindow().clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            }
        });
    }

    /** Lets go of the wake lock, whoever is holding it. */
    private static void releaseWakeLock() {
        if (wakeLock != null && wakeLock.isHeld()) {
            wakeLock.release();
        }
    }

    /**
     * Adds the view the input method attaches to.
     *
     * <p>It is one pixel in the corner and draws nothing: what matters is that
     * it is editable and can hold the focus, because that is the only thing
     * Android will show a keyboard for. Its own text is never kept - every
     * commit is forwarded to the native side, which owns the string.
     *
     * <p>Keys it does not take (the arrows, the remote's own buttons) fall
     * through to the activity as usual, so the viewer can still walk the form
     * while the keyboard is up.
     */
    private void installTextInput() {
        inputMethod = (InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE);
        inputView = new View(this) {
            @Override
            public boolean onCheckIsTextEditor() {
                return true;
            }

            /**
             * Back while the keyboard is up belongs to the input method: it puts
             * itself away and the key never reaches the activity, so the native
             * side would go on believing the keyboard is there. This is the one
             * place that sees the key first.
             */
            @Override
            public boolean onKeyPreIme(int keyCode, KeyEvent event) {
                if (keyCode == KeyEvent.KEYCODE_BACK
                        && event.getAction() == KeyEvent.ACTION_UP
                        && keyboardWanted) {
                    keyboardDismissed();
                }
                return super.onKeyPreIme(keyCode, event);
            }

            @Override
            public InputConnection onCreateInputConnection(EditorInfo out) {
                // The kind of field the native side is editing decides the
                // action key. A single-line field finishes on Done; a multi-line
                // one must keep Enter as a newline (IME_ACTION_NONE with
                // IME_FLAG_NO_ENTER_ACTION), because that key cannot both break
                // the line and take the text - Back, or the floating box's Done,
                // is how a multi-line field is left.
                if (keyboardMultiline) {
                    out.inputType = InputType.TYPE_CLASS_TEXT
                            | InputType.TYPE_TEXT_FLAG_MULTI_LINE
                            | InputType.TYPE_TEXT_VARIATION_URI;
                    out.imeOptions = EditorInfo.IME_ACTION_NONE
                            | EditorInfo.IME_FLAG_NO_ENTER_ACTION
                            | EditorInfo.IME_FLAG_NO_FULLSCREEN;
                } else {
                    out.inputType = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_URI;
                    out.imeOptions = EditorInfo.IME_ACTION_DONE | EditorInfo.IME_FLAG_NO_FULLSCREEN;
                }
                return new BaseInputConnection(this, false) {
                    @Override
                    public boolean setComposingText(CharSequence text, int newCursorPosition) {
                        onComposing(text.toString());
                        return true;
                    }

                    @Override
                    public boolean finishComposingText() {
                        composing = "";
                        return true;
                    }

                    @Override
                    public boolean commitText(CharSequence text, int newCursorPosition) {
                        onCommit(text.toString());
                        return true;
                    }

                    @Override
                    public boolean deleteSurroundingText(int beforeLength, int afterLength) {
                        composing = "";
                        if (beforeLength > 0) {
                            nativeBackspace(beforeLength);
                        }
                        return true;
                    }

                    @Override
                    public boolean sendKeyEvent(KeyEvent event) {
                        if (event.getAction() != KeyEvent.ACTION_DOWN) {
                            return true;
                        }
                        switch (event.getKeyCode()) {
                            case KeyEvent.KEYCODE_DEL:
                                composing = "";
                                nativeBackspace(1);
                                return true;
                            case KeyEvent.KEYCODE_ENTER:
                            case KeyEvent.KEYCODE_NUMPAD_ENTER:
                                composing = "";
                                nativeEnter();
                                return true;
                            default:
                                final int unicode = event.getUnicodeChar();
                                if (unicode != 0) {
                                    // A character from the keyboard itself
                                    // rather than a commit - some input methods
                                    // spell a word out this way.
                                    composing = "";
                                    nativeCommit(String.valueOf((char) unicode));
                                    return true;
                                }
                                // Not ours: let the remote's buttons reach the
                                // activity.
                                return false;
                        }
                    }
                };
            }
        };
        inputView.setFocusable(false);
        inputView.setFocusableInTouchMode(false);
        final ViewGroup content = findViewById(android.R.id.content);
        content.addView(inputView, new ViewGroup.LayoutParams(1, 1));
    }

    // ---------------------------------------------------------------- clipboard
    //
    // For the buttons of the floating input box (copy and paste), called from
    // `monitor_gui::android`. Any thread may call them. A device that refuses
    // the clipboard - some TV builds do - gives an empty string, and a copy
    // that goes nowhere, instead of an exception on the native side.

    /** The text on the clipboard, or an empty string. */
    public static String getClipboardText() {
        final MainActivity self = instance;
        if (self == null) {
            return "";
        }
        try {
            final ClipboardManager manager =
                    (ClipboardManager) self.getSystemService(Context.CLIPBOARD_SERVICE);
            final ClipData data = manager == null ? null : manager.getPrimaryClip();
            if (data == null || data.getItemCount() == 0) {
                return "";
            }
            final CharSequence text = data.getItemAt(0).coerceToText(self);
            return text == null ? "" : text.toString();
        } catch (RuntimeException refused) {
            android.util.Log.w("XGView.Clipboard", "clipboard read refused", refused);
            return "";
        }
    }

    /** Puts {@code text} on the clipboard. */
    public static void setClipboardText(final String text) {
        final MainActivity self = instance;
        if (self == null) {
            return;
        }
        try {
            final ClipboardManager manager =
                    (ClipboardManager) self.getSystemService(Context.CLIPBOARD_SERVICE);
            if (manager != null) {
                manager.setPrimaryClip(ClipData.newPlainText("XGView", text));
            }
        } catch (RuntimeException refused) {
            android.util.Log.w("XGView.Clipboard", "clipboard write refused", refused);
        }
    }

    /**
     * Opens a URL - {@code http:}, {@code mailto:}, ... - in whatever
     * application handles it.
     *
     * <p>egui's own opener is compiled out on Android, so a link clicked in the
     * About tab is handed here instead; without this the click would do nothing.
     * A device with no handler for the scheme is logged, not an error.
     */
    public static void openUri(final String url) {
        final MainActivity self = instance;
        if (self == null) {
            return;
        }
        try {
            final Uri uri = Uri.parse(url);
            // A `mailto:` is registered by mail clients under ACTION_SENDTO, not
            // ACTION_VIEW - asking for a view there often finds no handler - so
            // the action follows the scheme.
            final Intent intent = "mailto".equalsIgnoreCase(uri.getScheme())
                    ? new Intent(Intent.ACTION_SENDTO, uri)
                    : new Intent(Intent.ACTION_VIEW, uri);
            intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
            self.startActivity(intent);
        } catch (RuntimeException none) {
            android.util.Log.w("XGView.Link", "no app handles " + url, none);
        }
    }

    // ------------------------------------------------------- config import / export
    //
    // The About tab's export and import. Android has no file dialog of the
    // desktop kind, so the export goes to a fixed place any file manager can
    // reach - Download/xgview/config.json - while the import is handed to the
    // system's own document picker. The picker is not a preference: from
    // Android 11 a plain path into shared storage is closed to an app for any
    // file it did not create itself, which is exactly the case for a
    // configuration carried over from another device.

    /** Folder the export is written to, under the public Downloads directory. */
    private static final String EXPORT_FOLDER = "Download/xgview";

    /** Request code for the storage permission Android 9 needs to write there. */
    private static final int REQUEST_WRITE_STORAGE = 0x5847;

    /** Request code for the document picker an import opens. */
    private static final int REQUEST_IMPORT_DOCUMENT = 0x5848;

    /**
     * Writes the exported configuration into {@link #EXPORT_FOLDER} and returns
     * the path to show the viewer.
     *
     * <p>Returns null when the write could not even be attempted because the
     * storage permission Android 9 needs is still missing - the system dialog
     * has been asked for, and the viewer is to try again. Everything else that
     * goes wrong is thrown, and reaches the viewer as an export failure.
     *
     * <p>Called from the native side, on the UI thread.
     */
    public static String writeDownloadFile(String name, String text) throws Exception {
        if (instance == null) {
            throw new IllegalStateException("the activity is not up");
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            return writeViaMediaStore(name, text);
        }
        return writeViaPublicDownload(name, text);
    }

    /**
     * Android 10+: the Downloads collection, which lets an app put a file in
     * the public Downloads folder without holding any storage permission. The
     * permission that route needs nothing of is the very one Android 9 has no
     * substitute for; see {@link #writeViaPublicDownload}.
     */
    private static String writeViaMediaStore(String name, String text) throws Exception {
        final MainActivity self = instance;
        final ContentResolver resolver = self.getContentResolver();
        // `MediaStore` wants the relative path without the trailing separator.
        final String folder = Environment.DIRECTORY_DOWNLOADS + "/xgview";
        final Uri collection = MediaStore.Downloads.EXTERNAL_CONTENT_URI;
        final byte[] bytes = text.getBytes(StandardCharsets.UTF_8);

        // Exporting twice must replace the file rather than leave a
        // "config (1).json" beside it, which is what an insert would do. The
        // row may carry that suffix already; see `findDownload`.
        final Uri existing = findDownload(resolver, folder, name);
        if (existing != null) {
            try (OutputStream stream = resolver.openOutputStream(existing, "wt")) {
                if (stream == null) {
                    throw new IOException("cannot open " + existing);
                }
                stream.write(bytes);
            }
            return EXPORT_FOLDER + "/" + downloadName(resolver, existing, name);
        }

        final ContentValues values = new ContentValues();
        values.put(MediaStore.Downloads.DISPLAY_NAME, name);
        values.put(MediaStore.Downloads.RELATIVE_PATH, folder);
        values.put(MediaStore.Downloads.MIME_TYPE, "application/json");
        // Pending until the bytes are all there, so a reader never sees half a
        // configuration.
        values.put(MediaStore.Downloads.IS_PENDING, 1);
        final Uri target = resolver.insert(collection, values);
        if (target == null) {
            throw new IOException("cannot create " + EXPORT_FOLDER + "/" + name);
        }
        boolean written = false;
        try {
            try (OutputStream stream = resolver.openOutputStream(target, "wt")) {
                if (stream == null) {
                    throw new IOException("cannot open " + target);
                }
                stream.write(bytes);
                written = true;
            }
        } finally {
            if (!written) {
                // Nothing to publish, and a pending entry would only be litter.
                resolver.delete(target, null, null);
            } else {
                final ContentValues publish = new ContentValues();
                publish.put(MediaStore.Downloads.IS_PENDING, 0);
                resolver.update(target, publish, null, null);
            }
        }
        // `MediaStore` keeps the name it was given unless a file of that name
        // already sits in the folder without being in its index - a
        // configuration dropped there by hand, say - and then it makes the name
        // unique: "config (1).json". That is the name the viewer is told about,
        // and the one the next export has to look for; see `findDownload`.
        return EXPORT_FOLDER + "/" + downloadName(resolver, target, name);
    }

    /** The name `MediaStore` gave an entry, or `fallback` when it will not say. */
    private static String downloadName(ContentResolver resolver, Uri uri, String fallback) {
        final String[] columns = { MediaStore.Downloads.DISPLAY_NAME };
        try (Cursor cursor = resolver.query(uri, columns, null, null, null)) {
            if (cursor != null && cursor.moveToFirst() && !cursor.isNull(0)) {
                final String name = cursor.getString(0);
                if (name != null && !name.isEmpty()) {
                    return name;
                }
            }
        } catch (RuntimeException error) {
            android.util.Log.w("xgview", "cannot read the name of " + uri, error);
        }
        return fallback;
    }

    /** The entry `MediaStore` already holds for that file, if any. */
    private static Uri findDownload(ContentResolver resolver, String folder, String name) {
        final Uri collection = MediaStore.Downloads.EXTERNAL_CONTENT_URI;
        final String[] columns = { MediaStore.Downloads._ID };
        // The wanted name, or the one `MediaStore` made unique out of it. The
        // rows in this folder are only ever the app's own - an install holds no
        // storage permission, so it sees nothing else - which is what makes the
        // "config (…).json" pattern safe to match: it can only be a previous
        // export of this same file.
        final int dot = name.lastIndexOf('.');
        final String base = dot > 0 ? name.substring(0, dot) : name;
        final String extension = dot > 0 ? name.substring(dot) : "";
        final String where = MediaStore.Downloads.RELATIVE_PATH + "=? AND ("
                + MediaStore.Downloads.DISPLAY_NAME + "=? OR "
                + MediaStore.Downloads.DISPLAY_NAME + " LIKE ?)";
        // The relative path `MediaStore` reports keeps the trailing separator.
        final String[] args = { folder + "/", name, base + " (%" + extension };
        try (Cursor cursor = resolver.query(collection, columns, where, args, null)) {
            if (cursor != null && cursor.moveToFirst()) {
                return ContentUris.withAppendedId(collection, cursor.getLong(0));
            }
        } catch (RuntimeException error) {
            android.util.Log.w("xgview", "cannot look for an existing " + name, error);
        }
        return null;
    }

    /**
     * Android 9: the public Downloads directory itself, which needs the storage
     * permission. There is no MediaStore route at that API level, so this is
     * the one version whose export can ask the viewer for something.
     *
     * <p>Returns null when the permission is missing, having asked for it.
     */
    @SuppressWarnings("deprecation")
    private static String writeViaPublicDownload(String name, String text) throws Exception {
        final MainActivity self = instance;
        if (self.checkSelfPermission(android.Manifest.permission.WRITE_EXTERNAL_STORAGE)
                != PackageManager.PERMISSION_GRANTED) {
            self.runOnUiThread(() -> self.requestPermissions(
                    new String[] { android.Manifest.permission.WRITE_EXTERNAL_STORAGE },
                    REQUEST_WRITE_STORAGE));
            return null;
        }
        final File folder = new File(
                Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS), "xgview");
        if (!folder.isDirectory() && !folder.mkdirs()) {
            throw new IOException("cannot create " + folder);
        }
        final File file = new File(folder, name);
        try (OutputStream stream = new FileOutputStream(file)) {
            stream.write(text.getBytes(StandardCharsets.UTF_8));
        }
        // So a media transfer and the Downloads app see it without a reboot.
        MediaScannerConnection.scanFile(self, new String[] { file.getAbsolutePath() }, null, null);
        return EXPORT_FOLDER + "/" + name;
    }

    /**
     * Opens the system's document picker for an import.
     *
     * <p>Called from the native side. The answer - the file's text, or why it
     * could not be read - comes back through {@link #onActivityResult}.
     */
    public static void pickConfigFile() {
        final MainActivity self = instance;
        if (self == null) {
            return;
        }
        self.runOnUiThread(() -> {
            final Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT)
                    .addCategory(Intent.CATEGORY_OPENABLE)
                    .setType("*/*")
                    // A picker honouring the filter shows only these, which keeps
                    // a wall of unrelated files out of the way.
                    .putExtra(Intent.EXTRA_MIME_TYPES, new String[] {
                            "application/json", "text/plain", "application/octet-stream"
                    });
            try {
                self.startActivityForResult(intent, REQUEST_IMPORT_DOCUMENT);
            } catch (RuntimeException none) {
                // Some television builds have no document provider at all.
                android.util.Log.w("xgview", "no document picker on this device", none);
                nativeConfigPicked(null, "no document picker on this device");
            }
        });
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode != REQUEST_IMPORT_DOCUMENT) {
            return;
        }
        final Uri uri = resultCode == RESULT_OK && data != null ? data.getData() : null;
        if (uri == null) {
            // Cancelled: nothing happened, and nothing is said about it.
            nativeConfigPicked(null, null);
            return;
        }
        try (InputStream stream = getContentResolver().openInputStream(uri)) {
            if (stream == null) {
                throw new IOException("cannot open " + uri);
            }
            final ByteArrayOutputStream buffer = new ByteArrayOutputStream();
            final byte[] chunk = new byte[8192];
            int read;
            while ((read = stream.read(chunk)) > 0) {
                buffer.write(chunk, 0, read);
            }
            nativeConfigPicked(new String(buffer.toByteArray(), StandardCharsets.UTF_8), null);
        } catch (Exception error) {
            android.util.Log.w("xgview", "cannot read the picked file", error);
            nativeConfigPicked(null, String.valueOf(error));
        }
    }

    // ---------------------------------------------------------------- boot start
    //
    // Android 10+ refuses to start an activity from the background, and the
    // BOOT_COMPLETED receiver is a background start: without one of the two
    // conditions below the system aborts the relaunch ("Abort background
    // activity starts"), silently as far as the viewer is concerned. The
    // native side reads them and opens the screen that sets each.

    /**
     * Whether XGView may draw over other apps.
     *
     * <p>Holding this permission is one of the conditions that lift the
     * background-start refusal, which is what lets {@code BootReceiver}
     * relaunch the wall after boot.
     */
    public static boolean overlayAllowed() {
        final MainActivity self = instance;
        return self != null && Settings.canDrawOverlays(self);
    }

    /** Opens the system screen that grants {@link #overlayAllowed()}. */
    public static void openOverlaySettings() {
        final MainActivity self = instance;
        if (self != null) {
            self.startSettingsOrDetails(new Intent(Settings.ACTION_MANAGE_OVERLAY_PERMISSION,
                    Uri.parse("package:" + self.getPackageName())));
        }
    }

    /** Whether XGView is the device's home app - the other way to start at boot. */
    public static boolean isHomeApp() {
        final MainActivity self = instance;
        if (self == null) {
            return false;
        }
        final Intent home = new Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_HOME);
        final ResolveInfo resolved =
                self.getPackageManager().resolveActivity(home, PackageManager.MATCH_DEFAULT_ONLY);
        return resolved != null
                && resolved.activityInfo != null
                && self.getPackageName().equals(resolved.activityInfo.packageName);
    }

    /** Opens the system screen that chooses the device's home app. */
    public static void openHomeSettings() {
        final MainActivity self = instance;
        if (self != null) {
            self.startSettingsOrDetails(new Intent(Settings.ACTION_HOME_SETTINGS));
        }
    }

    /**
     * Starts a settings screen, falling back to this app's own details page
     * when the screen does not exist on the device - some TV builds omit one of
     * them - so a button is never left doing nothing.
     */
    private void startSettingsOrDetails(Intent intent) {
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        try {
            startActivity(intent);
        } catch (RuntimeException missing) {
            android.util.Log.w("XGView.BootReceiver", "no settings screen for " + intent, missing);
            try {
                startActivity(new Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                        Uri.parse("package:" + getPackageName()))
                        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK));
            } catch (RuntimeException error) {
                android.util.Log.w("XGView.BootReceiver", "no app details screen either", error);
            }
        }
    }

    /** Called once, so that the native side can find this class later. */
    private static native void nativeReady();

    /** The keyboard typed something. */
    private static native void nativeCommit(String text);

    /** The keyboard deleted {@code count} characters. */
    private static native void nativeBackspace(int count);

    /** The keyboard asked for the text to be taken (the Done / Enter key). */
    private static native void nativeEnter();

    /** The keyboard went away on its own - Back, or its own hide button. */
    private static native void nativeKeyboardHidden();

    /**
     * The document an import picked, or why it could not be read.
     *
     * <p>{@code text} is the file's content and {@code error} says what went
     * wrong when it is not null; both null means the viewer cancelled, which is
     * not a failure and is not reported as one.
     */
    private static native void nativeConfigPicked(String text, String error);

    /** The window insets, in pixels: the width of the navigation bar's strip. */
    private static native void nativeInsets(int left, int right);
}
