package com.xhbl.xgview;

import android.app.Activity;
import android.content.res.AssetManager;
import android.os.Build;
import android.os.Bundle;
import android.util.Log;
import android.view.KeyEvent;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.ViewGroup;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.widget.FrameLayout;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;

/**
 * The wall, on a surface this activity owns.
 *
 * <p>An ordinary activity rather than a {@code NativeActivity}, because a
 * {@code NativeActivity} gives its whole window to native drawing and that makes
 * two things impossible at once: a view above the picture, and a compositor
 * layer below it. Both were measured - see
 * {@code priv/docs/android-surface-plan.md} and, for the numbers,
 * {@code playback-decode-cost.md} §13-16.
 *
 * <p>With a {@code SurfaceView} the native side gets a window of its own, the
 * view hierarchy stays the activity's, and a second surface can carry a decoded
 * picture straight to the compositor. This activity is the first step of that
 * move: the wall draws here and the video still takes the old path.
 *
 * <p>Two things are handed over rather than inherited. The activity itself,
 * because the audio sink needs a real Android context. And {@code MainActivity},
 * because the native side talks to Java through the static methods on it -
 * power, rotation, the file picker - and those live there, not here. There is no
 * {@code MainActivity} instance behind this one, so those calls do nothing;
 * giving both activities one shared bridge is part of the move, not of this
 * step.
 */
public class SurfaceActivity extends Activity {
    private static final String TAG = "xgview-surface";

    private SurfaceView canvas;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        // This activity is started on its own, so nothing else has loaded the
        // library for it.
        System.loadLibrary("monitor_android");

        // The language packs travel in the APK's assets. The `NativeActivity`
        // path extracts them from native code, which has the asset manager; this
        // one has no `AndroidApp` to ask, so it copies them itself - the same
        // destination, `<files>/langs`, which is where the catalogue looks.
        copyLanguagePacks();

        final FrameLayout root = new FrameLayout(this);
        canvas = new SurfaceView(this);
        canvas.getHolder().addCallback(new SurfaceHolder.Callback() {
            @Override
            public void surfaceCreated(SurfaceHolder holder) {
                Log.i(TAG, "surface created");
                nativeStartWall(
                        holder.getSurface(),
                        getFilesDir().getAbsolutePath(),
                        externalFilesDir(),
                        getResources().getDisplayMetrics().density,
                        SurfaceActivity.this,
                        MainActivity.class);
            }

            @Override
            public void surfaceChanged(SurfaceHolder holder, int format, int width, int height) {
                nativeResizeWall(width, height);
            }

            @Override
            public void surfaceDestroyed(SurfaceHolder holder) {
                nativeStopWall();
            }
        });
        root.addView(canvas, new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.MATCH_PARENT));
        setContentView(root);

        hideSystemBars();
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        nativeWallFocus(hasFocus);
        if (hasFocus) {
            hideSystemBars();
        }
    }

    /**
     * Keys go straight to the native side, which translates them for egui.
     *
     * <p>Returning true for both directions keeps the activity from also
     * treating a key as navigation, which would move the focus twice.
     */
    @Override
    public boolean onKeyDown(int keyCode, KeyEvent event) {
        nativeKey(keyCode, true, event.getRepeatCount() > 0, event.getMetaState());
        return true;
    }

    @Override
    public boolean onKeyUp(int keyCode, KeyEvent event) {
        nativeKey(keyCode, false, false, event.getMetaState());
        return true;
    }

    /**
     * Fingers, one call per pointer.
     *
     * <p>The native side works in egui's points, not pixels, so nothing is
     * scaled here - the density travels once, at start-up. Two fingers down are
     * simply two pointers; the pinch is the ratio egui reads between them.
     */
    @Override
    public boolean onTouchEvent(android.view.MotionEvent event) {
        switch (event.getActionMasked()) {
            case android.view.MotionEvent.ACTION_DOWN:
            case android.view.MotionEvent.ACTION_POINTER_DOWN: {
                final int index = event.getActionIndex();
                nativeTouch(0, event.getPointerId(index),
                        event.getX(index), event.getY(index), event.getPressure(index));
                return true;
            }
            case android.view.MotionEvent.ACTION_MOVE: {
                for (int i = 0; i < event.getPointerCount(); i++) {
                    nativeTouch(1, event.getPointerId(i),
                            event.getX(i), event.getY(i), event.getPressure(i));
                }
                return true;
            }
            case android.view.MotionEvent.ACTION_UP:
            case android.view.MotionEvent.ACTION_POINTER_UP: {
                final int index = event.getActionIndex();
                nativeTouch(2, event.getPointerId(index),
                        event.getX(index), event.getY(index), event.getPressure(index));
                return true;
            }
            case android.view.MotionEvent.ACTION_CANCEL: {
                for (int i = 0; i < event.getPointerCount(); i++) {
                    nativeTouch(3, event.getPointerId(i),
                            event.getX(i), event.getY(i), event.getPressure(i));
                }
                return true;
            }
            default:
                return super.onTouchEvent(event);
        }
    }

    /** A wheel, or a trackpad's scroll. */
    @Override
    public boolean onGenericMotionEvent(android.view.MotionEvent event) {
        if (event.getActionMasked() == android.view.MotionEvent.ACTION_SCROLL) {
            nativeScroll(event.getAxisValue(android.view.MotionEvent.AXIS_HSCROLL) * 48f,
                    event.getAxisValue(android.view.MotionEvent.AXIS_VSCROLL) * 48f);
            return true;
        }
        return super.onGenericMotionEvent(event);
    }

    @Override
    protected void onDestroy() {
        nativeStopWall();
        super.onDestroy();
    }

    private String externalFilesDir() {
        final File dir = getExternalFilesDir(null);
        return dir != null ? dir.getAbsolutePath() : getFilesDir().getAbsolutePath();
    }

    private void hideSystemBars() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            final WindowInsetsController controller = getWindow().getInsetsController();
            if (controller != null) {
                controller.hide(WindowInsets.Type.systemBars());
                controller.setSystemBarsBehavior(
                        WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE);
            }
        }
    }

    /**
     * Copies the language packs out of the APK into the configuration directory.
     *
     * <p>Silent on failure: a missing pack costs a language, not the wall, and
     * English is compiled in.
     */
    private void copyLanguagePacks() {
        final AssetManager assets = getAssets();
        final File dest = new File(getFilesDir(), "langs");
        try {
            final String[] names = assets.list("langs");
            if (names == null || names.length == 0) {
                return;
            }
            if (!dest.isDirectory() && !dest.mkdirs()) {
                Log.w(TAG, "cannot create " + dest);
                return;
            }
            for (String name : names) {
                if (!name.endsWith(".ftl")) {
                    continue;
                }
                try (InputStream in = assets.open("langs/" + name);
                     OutputStream out = new FileOutputStream(new File(dest, name))) {
                    final byte[] buffer = new byte[8192];
                    int read;
                    while ((read = in.read(buffer)) > 0) {
                        out.write(buffer, 0, read);
                    }
                }
            }
        } catch (IOException error) {
            Log.w(TAG, "cannot copy the language packs", error);
        }
    }

    private static native void nativeStartWall(
            android.view.Surface surface,
            String internal,
            String external,
            float density,
            SurfaceActivity activity,
            Class<?> bridge);

    private static native void nativeResizeWall(int width, int height);

    private static native void nativeStopWall();

    private static native void nativeWallFocus(boolean focused);

    private static native void nativeKey(int keyCode, boolean pressed, boolean repeat, int meta);

    private static native void nativeTouch(
            int phase, int id, float x, float y, float force);

    private static native void nativeScroll(float dx, float dy);
}
