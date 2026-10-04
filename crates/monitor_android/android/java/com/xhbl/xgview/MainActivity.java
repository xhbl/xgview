package com.xhbl.xgview;

import android.app.NativeActivity;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.pm.ResolveInfo;
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

/**
 * XGView's activity: the {@link NativeActivity} the whole UI runs in, with the
 * system bars kept hidden and a soft keyboard that can be typed into.
 *
 * <p>The bars have to be hidden from Java: a {@code NativeActivity} tells the
 * native library where to draw but says nothing about the system UI, and a
 * window that merely fills the screen still has the navigation bar drawn over
 * it. On a phone in landscape that bar is a strip along the right edge, laid
 * over the toolbar - a control underneath it looks live and never receives a
 * touch.
 *
 * <p>Hiding it is what the full screen setting is for on Android. It stays
 * hidden until a swipe from an edge asks for it back, which is why the bars are
 * hidden again whenever the activity regains the focus.
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

    /** The one pixel view the input method types into. */
    private View inputView;

    private InputMethodManager inputMethod;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        instance = this;
        hideSystemBars();
        installTextInput();
        // The activity loads the native library itself, with a bare `dlopen`
        // that leaves its symbols out of the global scope the runtime searches
        // when it resolves a `native` method. Without this second load - which
        // is what registers the library with this class loader - the call below
        // dies of `UnsatisfiedLinkError` before the first frame.
        System.loadLibrary("monitor_android");
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
            hideSystemBars();
        }
    }

    /**
     * Called from the native side when a text field takes or loses the focus, so
     * that the keyboard follows what the viewer is doing in the UI.
     */
    public static void setKeyboardWanted(final boolean wanted) {
        final MainActivity self = instance;
        if (self == null || self.inputView == null) {
            return;
        }
        self.runOnUiThread(() -> {
            if (wanted) {
                self.inputView.setFocusable(true);
                self.inputView.setFocusableInTouchMode(true);
                self.inputView.requestFocus();
                self.inputMethod.showSoftInput(self.inputView, 0);
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

    /** Hides the status and navigation bars, and keeps them hidden. */
    private void hideSystemBars() {
        final Window window = getWindow();
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            // Android 11 and newer. The transient behaviour brings the bars back
            // as an overlay for a moment rather than resizing the window, which
            // would drop a frame of every video on screen.
            window.setDecorFitsSystemWindows(false);
            final WindowInsetsController controller = window.getInsetsController();
            if (controller != null) {
                controller.setSystemBarsBehavior(
                        WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE);
                controller.hide(WindowInsets.Type.systemBars());
            }
            return;
        }
        // Android 10 and older: the flags, which is also what the manifest's
        // fullscreen theme sets before the first frame.
        window.getDecorView().setSystemUiVisibility(
                View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                        | View.SYSTEM_UI_FLAG_FULLSCREEN
                        | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                        | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                        | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                        | View.SYSTEM_UI_FLAG_LAYOUT_STABLE);
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

            @Override
            public InputConnection onCreateInputConnection(EditorInfo out) {
                out.inputType = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_URI;
                out.imeOptions = EditorInfo.IME_ACTION_DONE | EditorInfo.IME_FLAG_NO_FULLSCREEN;
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
}
