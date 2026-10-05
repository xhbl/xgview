package com.xhbl.xgview;

import android.app.NativeActivity;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.pm.ResolveInfo;
import android.graphics.Insets;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.provider.Settings;
import android.text.InputType;
import android.view.KeyEvent;
import android.view.View;
import android.view.ViewGroup;
import android.view.Window;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;

import java.lang.reflect.Method;

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

    /** The window insets, in pixels: the width of the navigation bar's strip. */
    private static native void nativeInsets(int left, int right);
}
