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
 * <p>With a {@code SurfaceView} the native side gets a window of its own and the
 * view hierarchy stays the activity's. What it cannot yet do is share the screen
 * with a decoded picture: a wgpu surface is always opaque here, so the wall
 * cannot be drawn over one and let it show through, and every arrangement that
 * would put the picture underneath is closed (the plan document has each one and
 * why). This is therefore the first step only - the wall draws here, and the
 * video still takes the byte-buffer path.
 */
public class SurfaceActivity extends Activity {
    /**
     * The one this process has, if it has one.
     *
     * <p>The native side reaches Java through static methods on a class, because
     * that is all a JNI call knows; with two front ends in one process those
     * statics have to find whichever activity is actually on screen.
     */
    private static SurfaceActivity instance;

    private static final String TAG = "xgview-surface";

    private SurfaceView canvas;
    /** The decoded picture, between the wall and the bar. */
    private SurfaceView video;
    /** The controls, over the picture. */
    private SurfaceView bar;

    /**
     * Whether the picture and the bar get surfaces of their own.
     *
     * <p>On, and the arrangement under test: three opaque surfaces stacked -
     * the wall, the picture over it, and the bar over that. Nothing here needs
     * to be see-through, which is the whole reason it can work at all; the bar
     * covers the picture where the bar is, and that is what "floating over"
     * means.
     */
    private static final boolean EXTRA_SURFACES = true;

    /**
     * How tall the bar's own surface is, in pixels.
     *
     * <p>Deep enough for the deepest bar there is: a recording carries two rows
     * of controls and a row of times under them - 156 points in all, plus the
     * 12 it keeps from the screen's edge - and a strip that could not hold that
     * would have its timeline refused. Nothing is lost by the strip being
     * deeper than the bar: it is see-through, so the extra height is picture,
     * and the bar is docked to the screen's edge wherever the strip starts.
     */
    private static final int BAR_HEIGHT = 360;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        instance = this;
        // This activity is started on its own, so nothing else has loaded the
        // library for it.
        System.loadLibrary("monitor_android");

        // The language packs travel in the APK's assets. The `NativeActivity`
        // path extracts them from native code, which has the asset manager; this
        // one has no `AndroidApp` to ask, so it copies them itself - the same
        // destination, `<files>/langs`, which is where the catalogue looks.
        copyLanguagePacks();

        final FrameLayout root = new FrameLayout(this);

        // The picture, first, so the wall is drawn over it.
        video = new SurfaceView(this);
        video.getHolder().addCallback(new SurfaceHolder.Callback() {
            @Override
            public void surfaceCreated(SurfaceHolder holder) {
                if (EXTRA_SURFACES) {
                    Log.i(TAG, "video surface created");
                    nativeStartVideo(holder.getSurface());
                }
            }

            @Override
            public void surfaceChanged(SurfaceHolder holder, int format, int width, int height) {
                if (EXTRA_SURFACES) {
                    nativeStartVideo(holder.getSurface());
                }
            }

            @Override
            public void surfaceDestroyed(SurfaceHolder holder) {
                if (EXTRA_SURFACES) {
                    nativeStopVideo();
                }
            }
        });
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

        // Over the wall, not under it: the wall cannot be see-through, so the
        // picture has to cover the area the wall leaves for it. The bar is added
        // after this one, which puts it over both.
        if (EXTRA_SURFACES) {
            root.addView(video, new FrameLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT,
                    ViewGroup.LayoutParams.MATCH_PARENT));
        }
        // The bar, last, so it is drawn over the picture.
        bar = new SurfaceView(this);
        // See-through, which the picture cannot be: the codec refuses to render
        // into an `RGBA_8888` surface on this chip, but the bar draws nothing
        // the codec touches, and a translucent bar is a bar over the picture
        // rather than a band cut out of it.
        bar.getHolder().setFormat(android.graphics.PixelFormat.TRANSLUCENT);
        bar.getHolder().addCallback(new SurfaceHolder.Callback() {
            @Override
            public void surfaceCreated(SurfaceHolder holder) {
                if (EXTRA_SURFACES) {
                    Log.i(TAG, "bar surface created");
                    nativeStartBar(holder.getSurface(), bar.getWidth(), bar.getHeight());
                }
            }

            @Override
            public void surfaceChanged(SurfaceHolder holder, int format, int width, int height) {
                if (EXTRA_SURFACES) {
                    nativeStartBar(holder.getSurface(), bar.getWidth(), bar.getHeight());
                }
            }

            @Override
            public void surfaceDestroyed(SurfaceHolder holder) {
                if (EXTRA_SURFACES) {
                    nativeStopBar();
                }
            }
        });
        if (EXTRA_SURFACES) {
            root.addView(bar, new FrameLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT, BAR_HEIGHT,
                    android.view.Gravity.BOTTOM));
        }

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
        if (instance == this) {
            instance = null;
        }
        releaseWakeLock();
        nativeStopWall();
        super.onDestroy();
    }

    /**
     * Keeps the screen on, or lets it go, for the native side.
     *
     * <p>The same call `MainActivity` answers, and answered here for the same
     * reason: a surveillance wall that goes to sleep is not a wall. The native
     * side reaches Java through `MainActivity`'s statics, which hand it on to
     * whichever activity is on screen - see {@link #current}.
     */
    public static void setKeepScreenOn(final boolean wanted) {
        final SurfaceActivity self = instance;
        if (self == null) {
            return;
        }
        self.runOnUiThread(() -> {
            if (wanted) {
                self.getWindow().addFlags(android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            } else {
                self.getWindow().clearFlags(android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            }
        });
    }

    /** Holds the CPU awake, or lets it go; see {@link #setKeepScreenOn}. */
    public static void setPreventSleep(final boolean wanted) {
        final SurfaceActivity self = instance;
        if (self == null) {
            return;
        }
        self.runOnUiThread(() -> {
            if (!wanted) {
                self.releaseWakeLock();
                return;
            }
            if (self.wakeLock == null) {
                final android.os.PowerManager manager = (android.os.PowerManager)
                        self.getSystemService(android.content.Context.POWER_SERVICE);
                if (manager == null) {
                    return;
                }
                self.wakeLock = manager.newWakeLock(
                        android.os.PowerManager.PARTIAL_WAKE_LOCK, "xgview:wall");
                self.wakeLock.setReferenceCounted(false);
            }
            self.wakeLock.acquire();
        });
    }

    private void releaseWakeLock() {
        final android.os.PowerManager.WakeLock held = wakeLock;
        wakeLock = null;
        if (held != null && held.isHeld()) {
            held.release();
        }
    }

    /** The wall on screen in this process, or {@code null}. */
    public static SurfaceActivity current() {
        return instance;
    }

    private android.os.PowerManager.WakeLock wakeLock;

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

    private static native void nativeStartVideo(android.view.Surface surface);

    private static native void nativeStopVideo();

    private static native void nativeStartBar(
            android.view.Surface surface, int width, int height);

    private static native void nativeStopBar();
}
