package com.xhbl.xgview;

import android.app.NativeActivity;
import android.os.Build;
import android.os.Bundle;
import android.view.View;
import android.view.Window;
import android.view.WindowInsets;
import android.view.WindowInsetsController;

/**
 * XGView's activity: the {@link NativeActivity} the whole UI runs in, with the
 * system bars kept hidden.
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
 */
public class MainActivity extends NativeActivity {

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        hideSystemBars();
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (hasFocus) {
            hideSystemBars();
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
}
