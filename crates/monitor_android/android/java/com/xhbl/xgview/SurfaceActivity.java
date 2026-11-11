package com.xhbl.xgview;

import android.app.Activity;
import android.content.res.AssetManager;
import android.os.Bundle;
import android.util.Log;
import android.view.KeyEvent;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.ViewGroup;
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

    /** The boot-launch grace window and its retry chain; see {@link BootRace}. */
    private final BootRace boot = new BootRace(this);

    private static final String TAG = "xgview-surface";

    private SurfaceView canvas;
    /**
     * Two views for the decoded picture, between the wall and the bar.
     *
     * <p>A {@code SurfaceView} keeps the last frame it was given. Stepping out
     * of a camera and back should show the frame that camera left, not the
     * small sub-stream texture the wall has, so the view holding it is kept
     * aside while another one takes the next session. Only {@link #video} is
     * bound to the decoder; the other one is left alone with its picture.
     */
    private static final int VIDEO_SLOTS = 2;
    private final SurfaceView[] videoViews = new SurfaceView[VIDEO_SLOTS];
    private final String[] videoOwners = new String[VIDEO_SLOTS];
    private final boolean[] videoFilled = new boolean[VIDEO_SLOTS];
    private int videoSlot = 0;
    /** The decoded picture, between the wall and the bar: the active one. */
    private SurfaceView video;
    /** The camera native is asking the surface for; null outside a magnification. */
    private String videoOwner;
    /** The controls, over the picture. */
    private SurfaceView bar;
    /** Whether the picture's view is on screen; see {@link #setVideoVisible}. */
    private boolean videoShown = true;
    /** Whether the surface path is suspended; see {@link #setVideoSuspended}. */
    private boolean videoSuspended;
    /** The last placement reported, so only a change of it reaches the log. */
    private boolean placedShown = true;
    private int placedSlot;
    /** How much bigger than its box the picture is drawn; see {@link #setVideoZoom}. */
    private float videoScale = 1f;
    /** Where that drawing starts, relative to the box's corner, in pixels. */
    private float videoDx = 0f;
    private float videoDy = 0f;
    /** How far a moving page has taken the view, in pixels; see {@link #setVideoShift}. */
    private float videoShift;

    /**
     * Which hand-over of each surface is the one this activity last made.
     *
     * <p>There is one wall per process but not one activity: turning from
     * landscape to portrait starts the other surface activity beside this one,
     * and until the old one is destroyed both are getting surface callbacks.
     * The native side keeps only one window for the wall, one for the picture
     * and one for the bar, so a {@code surfaceDestroyed} - or the {@code onDestroy}
     * that follows it - from an activity that has already been replaced must not
     * detach the new one's. Each hand-over is given a number; a resize or a stop
     * carries the number it was given and is refused when it is no longer up.
     * Zero means this view has never had a surface.
     */
    private long wallToken;
    private long videoToken;
    private long barToken;

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
     * What one hand-over of the picture's surface is. The native side sees two
     * non-null `ANativeWindow*` in both a re-bind and a step to the other view,
     * so it cannot tell them apart - and confusing them is what made a channel
     * step forget the picture and flash the wall's 480p fallback.
     */
    private static final int VIDEO_SURFACE_REBOUND = 0;
    private static final int VIDEO_SURFACE_SWITCHED = 1;
    private static final int VIDEO_SURFACE_NEW = 2;

    /**
     * What the surface over the picture carries.
     *
     * <p>Everything the wall would draw over the picture and cannot, because the
     * picture is on a surface above it: the bar, and the corner display. It is
     * the whole screen rather than a strip along the bottom because the corners
     * are the screen's corners; it is see-through, so where it draws nothing the
     * picture shows, and an empty frame costs nothing at all - the compositor
     * drops a layer with nothing opaque in it.
     */

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        instance = this;
        // The calls that need no activity at all - a renderer restart, the
        // configuration paths - go through the application context, which until
        // now only the other front end ever handed over.
        MainActivity.useApplicationContext(getApplicationContext());
        // A boot launch races every other app that starts itself, and the wall
        // is the one nobody is watching: it has to ask for the foreground back.
        final android.content.Intent launch = getIntent();
        if (launch != null
                && launch.getBooleanExtra("com.xhbl.xgview.FROM_AUTOSTART", false)) {
            MainActivity.markAutostart();
            boot.arm();
        }
        // The same gate the other front end applies, and the same moment: the
        // file it writes is read once by the native side, when it builds the
        // renderer, and this activity starts that renderer from a surface
        // callback - well after here. A device whose Vulkan driver is older than
        // 1.1 loses its device while the first pipeline is made, so asking
        // first is the difference between a working wall and a crash loop.
        MainActivity.applyVulkanGate(this);
        // This activity is started on its own, so nothing else has loaded the
        // library for it. The native methods the window insets are reported
        // through live on `MainActivity`, whose class loader this load serves
        // just as well.
        System.loadLibrary("monitor_android");
        MainActivity.markNativeLoaded();

        // The language packs travel in the APK's assets. Both front ends copy
        // them through this one entry point into <files>/xgview/langs, which
        // is the directory the language catalogue scans.
        copyLanguagePacks(this);

        final FrameLayout root = new FrameLayout(this);

        // The picture's view, created before the wall's but added after it, so
        // the picture is drawn over the wall. See where `root` gets its views.
        for (int slot = 0; slot < VIDEO_SLOTS; slot++) {
            videoViews[slot] = new SurfaceView(this);
            videoViews[slot].getHolder().addCallback(new SurfaceHolder.Callback() {
                @Override
                public void surfaceCreated(SurfaceHolder holder) {
                    // A callback from an activity the launcher has already
                    // replaced must not take a new identity: it would claim the
                    // wall's single window for a view that is going away. The
                    // stop that follows is left alone - see `superseded`.
                    if (superseded()) {
                        return;
                    }
                    // Only the view the decoder is bound to reports a window.
                    // The other one is keeping a camera's last frame; binding
                    // it here would let the next session paint over that frame.
                    if (EXTRA_SURFACES && isActiveVideo(holder)) {
                        Log.i(TAG, "video surface created");
                        videoToken = nativeStartVideo(holder.getSurface(), VIDEO_SURFACE_NEW);
                    }
                }

                @Override
                public void surfaceChanged(SurfaceHolder holder, int format, int width, int height) {
                    if (superseded()) {
                        return;
                    }
                    if (EXTRA_SURFACES && isActiveVideo(holder)) {
                        // The same surface re-bound, whatever size the new
                        // picture is: the decoder stays on it and the picture
                        // it holds is not forgotten.
                        videoToken = nativeStartVideo(holder.getSurface(), VIDEO_SURFACE_REBOUND);
                    }
                }

                @Override
                public void surfaceDestroyed(SurfaceHolder holder) {
                    if (EXTRA_SURFACES && isActiveVideo(holder)) {
                        Log.i(TAG, "video surface destroyed");
                        nativeStopVideo(videoToken);
                        videoToken = 0;
                    }
                }
            });
        }
        video = videoViews[videoSlot];
        canvas = new SurfaceView(this);
        canvas.getHolder().addCallback(new SurfaceHolder.Callback() {
            @Override
            public void surfaceCreated(SurfaceHolder holder) {
                if (superseded()) {
                    return;
                }
                Log.i(TAG, "surface created");
                wallToken = nativeStartWall(
                        holder.getSurface(),
                        getFilesDir().getAbsolutePath(),
                        externalFilesDir(),
                        getResources().getDisplayMetrics().density,
                        SurfaceActivity.this,
                        MainActivity.class);
            }

            @Override
            public void surfaceChanged(SurfaceHolder holder, int format, int width, int height) {
                if (superseded()) {
                    return;
                }
                nativeResizeWall(wallToken, width, height);
            }

            @Override
            public void surfaceDestroyed(SurfaceHolder holder) {
                nativeStopWall(wallToken);
                wallToken = 0;
            }
        });
        root.addView(canvas, new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.MATCH_PARENT));

        // Over the wall, not under it: the wall cannot be see-through, so the
        // picture has to cover the area the wall leaves for it. The bar is added
        // after this one, which puts it over both.
        if (EXTRA_SURFACES) {
            for (int slot = 0; slot < VIDEO_SLOTS; slot++) {
                root.addView(videoViews[slot], new FrameLayout.LayoutParams(
                        ViewGroup.LayoutParams.MATCH_PARENT,
                        ViewGroup.LayoutParams.MATCH_PARENT));
            }
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
                if (superseded()) {
                    return;
                }
                if (EXTRA_SURFACES) {
                    Log.i(TAG, "bar surface created");
                    final android.graphics.Rect frame = holder.getSurfaceFrame();
                    barToken = nativeStartBar(holder.getSurface(), frame.width(), frame.height());
                }
            }

            @Override
            public void surfaceChanged(SurfaceHolder holder, int format, int width, int height) {
                if (superseded()) {
                    return;
                }
                if (EXTRA_SURFACES) {
                    // The callback's figures are the surface's own size. The
                    // view's getWidth() can still answer the layout from before
                    // a turn, and the bar is laid out against the whole screen:
                    // a stale size left the overlay in a corner and painted its
                    // opaque bands over the picture it is meant to sit on.
                    barToken = nativeStartBar(holder.getSurface(), width, height);
                }
            }

            @Override
            public void surfaceDestroyed(SurfaceHolder holder) {
                if (EXTRA_SURFACES) {
                    Log.i(TAG, "bar surface destroyed");
                    nativeStopBar(barToken);
                    barToken = 0;
                }
            }
        });
        if (EXTRA_SURFACES) {
            // The whole screen, not a strip along the bottom: it carries the
            // corner display as well as the bar, and the corners of the picture
            // are the corners of the screen. See `draw_overlay_only`.
            root.addView(bar, new FrameLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT,
                    ViewGroup.LayoutParams.MATCH_PARENT));
        }

        // Last, and outside the view the wall is drawn on: the one pixel the
        // input method types into. It draws nothing and is not focusable until
        // a field asks for it.
        installTextInput(root);

        setContentView(root);

        // The bars are the viewer's to keep or to hide, and the insets they
        // claim are the wall's to leave free. Both are applied to this window,
        // not to the `NativeActivity`'s: an immersive `hide` here used to be
        // the only thing this front end ever did to its system bars, which is
        // why "keep the navigation bar" worked on the old path and not here.
        MainActivity.applySystemBars(this);
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        nativeWallFocus(hasFocus);
        if (hasFocus) {
            MainActivity.applySystemBars(this);
        }
    }

    /**
     * Keys the interface knows are taken; the rest are left to the system.
     *
     * <p>A hardware key is a control <em>and</em> a character. The control is
     * its key code; the character is what the device's key map makes of that
     * code with the modifiers held, which is the only way a Shift, a non-Latin
     * layout or a numpad arrives correctly. Both go to the native side, which
     * decides whether the character is text a field should take. The soft
     * keyboard has its own path ({@code sendKeyEvent} below): a key the input
     * method consumes is committed there and never reaches this method, while
     * one it lets through is handled here. Either way a character is typed
     * once, not twice.
     *
     * <p>Answering {@code true} for everything would be simpler and is wrong:
     * the wall maps the remote's own buttons and a keyboard's keys, and nothing
     * else - so a key it does not map would stop there, and the volume keys, the
     * media keys and the language switch would do nothing at all. The native
     * side answers whether it took the key, and that answer is what this
     * returns. Returning true for both directions also keeps the activity from
     * treating a key as navigation, which would move the focus twice.
     */
    @Override
    public boolean onKeyDown(int keyCode, KeyEvent event) {
        return nativeKey(keyCode, true, event.getRepeatCount() > 0,
                event.getMetaState(), event.getUnicodeChar());
    }

    @Override
    public boolean onKeyUp(int keyCode, KeyEvent event) {
        return nativeKey(keyCode, false, false,
                event.getMetaState(), event.getUnicodeChar());
    }

    /**
     * Where the wall's surface starts inside the window, in pixels.
     *
     * <p>The window's corner is not always the picture's: a display cutout this
     * activity lays out around - or any front end that keeps a system bar -
     * puts the surface below or beside the window's own origin, while the
     * events that reach {@link #onTouchEvent} are measured from the window's.
     * The native side works in the picture's own coordinates - egui's screen
     * begins at the surface's top-left - so the difference is taken off here.
     * It is zero in the ordinary full screen case, which is why this was not
     * needed until the window could be laid out any other way.
     */
    private final int[] contentOrigin = new int[2];

    /**
     * Fingers, one call per pointer.
     *
     * <p>The native side works in egui's points, not pixels, so nothing is
     * scaled here - the density travels once, at start-up. Two fingers down are
     * simply two pointers; the pinch is the ratio egui reads between them.
     */
    @Override
    public boolean onTouchEvent(android.view.MotionEvent event) {
        // See `contentOrigin`: everything below is in the picture's coordinates,
        // and the window's are one status bar lower when the bars are kept.
        if (canvas != null) {
            canvas.getLocationInWindow(contentOrigin);
        }
        final float dx = contentOrigin[0];
        final float dy = contentOrigin[1];
        switch (event.getActionMasked()) {
            case android.view.MotionEvent.ACTION_DOWN:
            case android.view.MotionEvent.ACTION_POINTER_DOWN: {
                final int index = event.getActionIndex();
                nativeTouch(0, event.getPointerId(index),
                        event.getX(index) - dx, event.getY(index) - dy, event.getPressure(index));
                return true;
            }
            case android.view.MotionEvent.ACTION_MOVE: {
                for (int i = 0; i < event.getPointerCount(); i++) {
                    nativeTouch(1, event.getPointerId(i),
                            event.getX(i) - dx, event.getY(i) - dy, event.getPressure(i));
                }
                return true;
            }
            case android.view.MotionEvent.ACTION_UP:
            case android.view.MotionEvent.ACTION_POINTER_UP: {
                final int index = event.getActionIndex();
                nativeTouch(2, event.getPointerId(index),
                        event.getX(index) - dx, event.getY(index) - dy, event.getPressure(index));
                return true;
            }
            case android.view.MotionEvent.ACTION_CANCEL: {
                for (int i = 0; i < event.getPointerCount(); i++) {
                    nativeTouch(3, event.getPointerId(i),
                            event.getX(i) - dx, event.getY(i) - dy, event.getPressure(i));
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
    protected void onResume() {
        super.onResume();
        boot.onResume();
        // The same two repairs the other front end asks for on every resume: a
        // backoff grown while the process was in the background, and a window
        // the system may have left in an orientation the wall is not laid out
        // for. Both live on the native side, so this is the whole of the call.
        MainActivity.notifyResumed();
    }

    @Override
    protected void onPause() {
        super.onPause();
        boot.onPause();
    }

    @Override
    protected void onDestroy() {
        if (instance == this) {
            instance = null;
        }
        releaseWakeLock();
        // The same three stops the surface callbacks make, for a view hierarchy
        // that is taken apart without them: `finish()` and a system recycle
        // need not dispatch `surfaceDestroyed`. The tokens are what make this
        // safe both here and there - a second stop, or a late one from an
        // activity the launcher has already replaced, is refused on the native
        // side rather than detaching the wall that is up now.
        if (wallToken != 0) {
            nativeStopWall(wallToken);
            wallToken = 0;
        }
        if (videoToken != 0) {
            nativeStopVideo(videoToken);
            videoToken = 0;
        }
        if (barToken != 0) {
            nativeStopBar(barToken);
            barToken = 0;
        }
        super.onDestroy();
    }

    /**
     * The document an import picked.
     *
     * <p>The picker is started by whichever activity is on screen, and the
     * answer comes back to that one; reading it is the same work either way, so
     * it lives where the import does and this only hands it over.
     */
    @Override
    protected void onActivityResult(int requestCode, int resultCode, android.content.Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        MainActivity.readPickedDocument(this, requestCode, resultCode, data);
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

    /**
     * Whether another surface wall has taken this process over.
     *
     * <p>Turning the wall starts the other orientation's activity beside this
     * one, and the two overlap: the new one sets {@link #instance} in its
     * {@code onCreate}, while the old one's surface callbacks are still queued.
     * A late <em>start</em> from the old one would claim the process's single
     * set of windows for views that are going away, so those callbacks answer
     * here. The native ticket is the second line of defence - it also covers a
     * callback that was already in flight when {@code instance} changed.
     *
     * <p>A <em>stop</em> is deliberately not gated. Its token says which
     * surface it belongs to: before the new wall has attached, detaching the
     * old surface is the right answer; afterwards the token no longer matches
     * and the call is refused. Gating it would leave the renderer drawing into
     * a surface the system has already destroyed.
     */
    private boolean superseded() {
        return instance != this;
    }

    /**
     * Shows or hides the view the picture is scanned out of.
     *
     * <p>A {@code SurfaceView} keeps the last frame it was given and is drawn
     * over the wall, so one left standing after a return to the grid covers it
     * with a still picture - nine channels decoding underneath, and the screen
     * showing one frozen one. Nothing can clear the buffer (it is the codec's),
     * so the view goes away and comes back with the magnified viewport.
     */
    public static void setVideoVisible(final boolean visible) {
        final SurfaceActivity self = instance;
        if (self == null || self.video == null) {
            return;
        }
        self.runOnUiThread(() -> {
            // Moved out of the way rather than made `GONE`: hiding a
            // `SurfaceView` destroys its surface, and the surface coming back
            // rebuilds every layer above it - which is how the bar stopped being
            // drawn after a return to the grid. A translation leaves the surface
            // alive and only takes it off the screen.
            self.videoShown = visible;
            if (visible && self.videoOwner != null) {
                // A picture really has been rendered for this camera: the view
                // now holds a frame worth keeping when the viewer steps away.
                self.videoOwners[self.videoSlot] = self.videoOwner;
                self.videoFilled[self.videoSlot] = true;
            }
            Log.i(TAG, "video visible=" + visible + " owner=" + self.videoOwner
                    + " slot=" + self.videoSlot + " filled=" + self.videoFilled[self.videoSlot]);
            self.applyVideoPlacement();
        });
    }

    /**
     * Says whether the surface path is suspended, which is not the same answer
     * as "the view is hidden".
     *
     * <p>Two quite different things make the picture's view be hidden, and only
     * one of them may leave the view standing with the frame it holds:
     *
     * <ul>
     *   <li>a <em>new session</em> is about to fill it (a step to another
     *       camera, a seek) - the frame in it is the first picture of the step,
     *       so it stays in the box until the new session paints over it;</li>
     *   <li>the surface path is <em>suspended</em> - the picture turned a
     *       quarter turn, an opaque-only overlay, or a window floating over the
     *       picture. The codec stops rendering into this surface, so the view
     *       keeps the pre-turn frame for ever, and because it is above the wall
     *       that frame sits in the middle of the wall's turned picture.</li>
     * </ul>
     *
     * <p>{@link #setVideoVisible} cannot tell the two apart - both arrive as
     * {@code false} - so the native side says this as well. Without it a turned
     * portrait picture keeps the upright frame it was turned away from.
     */
    public static void setVideoSuspended(final boolean suspended) {
        final SurfaceActivity self = instance;
        if (self == null) {
            return;
        }
        self.runOnUiThread(() -> {
            if (self.videoSuspended == suspended) {
                return;
            }
            self.videoSuspended = suspended;
            Log.i(TAG, "video suspended=" + suspended);
            self.applyVideoPlacement();
        });
    }

    /**
     * Moves the picture's view along with the page it is on, in pixels.
     *
     * <p>A page's travel changes every frame, and moving this view by its
     * rectangle would mean a layout pass per frame - resizing the surface the
     * codec renders into, and landing a frame or two behind the page. The
     * picture then trails its own page, and the frame left behind reads as a
     * second copy of the picture at a slightly different offset. A translation
     * is applied by the compositor instead, so the box stays where the page will
     * settle and only the offset moves. Zero once the page is at rest.
     */
    public static void setVideoShift(final float shift) {
        final SurfaceActivity self = instance;
        if (self == null || self.video == null) {
            return;
        }
        self.runOnUiThread(() -> {
            if (self.videoShift == shift) {
                return;
            }
            self.videoShift = shift;
            self.applyVideoPlacement();
        });
    }

    /**
     * Says which camera the magnified viewport is asking the surface for.
     *
     * <p>An empty string means the viewport is gone. That is the important
     * answer: it puts every view out of the way and forgets the frames they
     * kept, so going back to the grid behaves exactly as it did with one view.
     */
    public static void setVideoOwner(final String owner) {
        final SurfaceActivity self = instance;
        if (self == null || self.video == null) {
            return;
        }
        self.runOnUiThread(() -> self.selectVideoOwner(owner));
    }

    /** Whether a holder belongs to the one view the decoder is bound to. */
    private boolean isActiveVideo(SurfaceHolder holder) {
        return video != null && holder == video.getHolder();
    }

    private void selectVideoOwner(String owner) {
        final String wanted = (owner == null || owner.isEmpty()) ? null : owner;
        videoOwner = wanted;
        if (wanted == null) {
            // Leaving the magnification: the kept frames are of no use over a
            // grid, and a view left standing would cover it with a still.
            for (int slot = 0; slot < VIDEO_SLOTS; slot++) {
                videoOwners[slot] = null;
                videoFilled[slot] = false;
            }
            applyVideoPlacement();
            return;
        }
        int chosen = -1;
        for (int slot = 0; slot < VIDEO_SLOTS; slot++) {
            if (wanted.equals(videoOwners[slot]) && videoFilled[slot]) {
                // Step back to a camera already seen: its view still holds the
                // frame it left, and that is the first picture of this step.
                chosen = slot;
                break;
            }
        }
        if (chosen < 0) {
            // A camera not seen before. An empty view is preferred over one
            // that is holding another camera's last frame.
            int empty = -1;
            for (int slot = 0; slot < VIDEO_SLOTS; slot++) {
                if (!videoFilled[slot]) {
                    empty = slot;
                    break;
                }
            }
            chosen = empty >= 0 ? empty : (videoSlot + 1) % VIDEO_SLOTS;
            videoOwners[chosen] = wanted;
            videoFilled[chosen] = false;
        }
        if (chosen != videoSlot) {
            videoSlot = chosen;
            video = videoViews[videoSlot];
            final android.view.Surface surface = video.getHolder().getSurface();
            if (surface != null && surface.isValid()) {
                // The other view is a different `ANativeWindow`, so the codec
                // has to be reopened on it rather than left rendering into the
                // view that is keeping the camera being stepped away from. But
                // it is not a *new* surface: forgetting the picture here would
                // make the wall paint its fallback until the next frame lands.
                videoToken = nativeStartVideo(surface, VIDEO_SURFACE_SWITCHED);
            }
        }
        videoOwners[videoSlot] = wanted;
        Log.i(TAG, "video owner=" + wanted + " slot=" + videoSlot
                + " filled=" + videoFilled[videoSlot]
                + " slots=" + java.util.Arrays.toString(videoOwners));
        applyVideoPlacement();
    }

    /**
     * Places the view the picture is scanned out of, in physical pixels.
     *
     * <p>Full screen until this says otherwise: a codec rendering into a
     * full-screen surface stretches its picture into the screen's shape, so the
     * fit switch used to change the wall's mind about where the picture is and
     * nothing on the screen. The rectangle is where the wall would have drawn
     * the picture itself, and it arrives already rounded.
     *
     * <p>The layout is moved rather than the picture scaled, for this step: a
     * relayout resizes the surface the codec renders into, which is a thing to
     * do when the picture's shape changes and not while a pinch is moving.
     *
     * <p>A picture turned a quarter turn is not sent here: the surface path is
     * suspended for it, and the wall draws the frames itself. See
     * {@code monitor_codec::amediacodec::set_surface_suspended}.
     */
    public static void setVideoRect(final float x, final float y, final float width, final float height) {
        final SurfaceActivity self = instance;
        if (self == null || self.video == null) {
            return;
        }
        final int left = Math.round(x);
        final int top = Math.round(y);
        final int w = Math.round(width);
        final int h = Math.round(height);
        if (w <= 0 || h <= 0) {
            return;
        }
        self.runOnUiThread(() -> {
            // Both views keep the same box: the one brought back for a camera
            // already seen must show its last frame at the shape that camera
            // had, before the next session gets around to placing it again.
            for (int slot = 0; slot < VIDEO_SLOTS; slot++) {
                final FrameLayout.LayoutParams params = new FrameLayout.LayoutParams(w, h);
                params.setMargins(left, top, 0, 0);
                self.videoViews[slot].setLayoutParams(params);
            }
            self.applyVideoPlacement();
        });
    }

    /**
     * Draws the picture bigger than its box, or back at its own size.
     *
     * <p>A pinch pushes the picture in by showing a part of it: the view keeps
     * its box and draws the whole picture {@code scale} times bigger, moved so
     * that the part being looked at is the part in the box. A scale and not
     * another box, because a relayout resizes the surface the codec is
     * rendering into, and that is not something to do while a pinch is moving.
     *
     * <p>{@code dx} and {@code dy} are where the picture's top-left corner goes,
     * relative to the box's - so a window off the picture's top-left is a
     * negative offset, and one on the middle of it is half the box times one
     * minus the scale.
     */
    public static void setVideoZoom(final float scale, final float dx, final float dy) {
        final SurfaceActivity self = instance;
        if (self == null || self.video == null) {
            return;
        }
        self.runOnUiThread(() -> {
            self.videoScale = scale > 0f ? scale : 1f;
            self.videoDx = dx;
            self.videoDy = dy;
            // About the corner the offsets are measured from, so that `dx`/`dy`
            // are the whole of where the picture starts. Both views take the
            // same pinch, so a view brought back is not left at the zoom of
            // the camera that was magnified before it.
            for (int slot = 0; slot < VIDEO_SLOTS; slot++) {
                self.videoViews[slot].setPivotX(0f);
                self.videoViews[slot].setPivotY(0f);
                self.videoViews[slot].setScaleX(self.videoScale);
                self.videoViews[slot].setScaleY(self.videoScale);
            }
            self.applyVideoPlacement();
        });
    }

    /**
     * Puts the picture's view where the rectangle it was last given says, or out
     * of the way when it is hidden.
     *
     * <p>The distance it is moved by when hidden is its own top plus its own
     * height as scaled, not its height alone: a letterboxed picture does not
     * start at the top of the screen, and one pushed in is taller than its box -
     * leave either out and the bottom of it still shows in the band above the
     * grid.
     */
    private void applyVideoPlacement() {
        if (video == null) {
            return;
        }
        final ViewGroup.LayoutParams raw = video.getLayoutParams();
        int height = raw != null && raw.height > 0 ? raw.height : video.getHeight();
        if (height <= 0) {
            // Before the first layout the view has no measured height, and a
            // hidden view moved by nothing is a full-screen black rectangle
            // over the wall while the first camera connects.
            height = getResources().getDisplayMetrics().heightPixels;
        }
        final int top = raw instanceof FrameLayout.LayoutParams
                ? ((FrameLayout.LayoutParams) raw).topMargin
                : 0;
        final float scale = Math.max(videoScale, 1f);
        // One of the two draws the picture, never both. The native side says
        // whether the wall is leaving this view's area blank - that is the only
        // question, and `true` here means it is. A view left up for anything
        // else stacks a second copy of the picture over the wall's own: the
        // frame this one is holding, at the shape and place of the camera before
        // the step, over the one the wall is painting at the shape and place of
        // the camera being stepped to. That double picture was what a fast swipe
        // between two cameras looked like. It is the suspension that has to
        // override this, not a kept frame: while the surface path is suspended
        // nothing renders into this view again, so the frame in it would stay
        // for ever over the wall's turned picture.
        final boolean shown = videoShown && !videoSuspended;
        for (int slot = 0; slot < VIDEO_SLOTS; slot++) {
            final SurfaceView view = videoViews[slot];
            if (view == null) {
                continue;
            }
            final boolean up = slot == videoSlot && shown;
            view.setTranslationX(up ? videoDx + videoShift : 0f);
            view.setTranslationY(up ? videoDy : -(top + height * scale));
        }
        // Only when the answer changes: a swipe re-places the view every frame,
        // and a line per frame would bury everything else in the log.
        if (shown != placedShown || videoSlot != placedSlot) {
            placedShown = shown;
            placedSlot = videoSlot;
            Log.i(TAG, "placement shown=" + shown + " suspended=" + videoSuspended
                    + " slot=" + videoSlot + " top=" + top + " height=" + height);
        }
    }

    private android.os.PowerManager.WakeLock wakeLock;

    // ------------------------------------------------------------ soft keyboard
    //
    // The one-pixel editable view is shared with the other front end through
    // TextInputHost. Only the root view and the JNI entry points are ours.

    /** The one-pixel editable view and everything that goes with it. */
    private TextInputHost textInput;

    /**
     * Called from the native side when a text field takes or loses the focus.
     * The native side's calls are {@code MainActivity}'s, which forwards them
     * here when this front end owns the wall.
     *
     * <p>{@code multiline} is the shape of the field being edited.
     */
    public static void setKeyboardWanted(final boolean wanted, final boolean multiline) {
        final SurfaceActivity self = instance;
        if (self == null || self.textInput == null) {
            return;
        }
        self.textInput.setWanted(wanted, multiline);
    }

    /** Adds the one-pixel editable view to this activity's own root. */
    private void installTextInput(final FrameLayout root) {
        textInput = new TextInputHost(this, new TextInputHost.Native() {
            @Override
            public void commit(String text) {
                nativeCommit(text);
            }

            @Override
            public void backspace(int count) {
                nativeBackspace(count);
            }

            @Override
            public void enter() {
                nativeEnter();
            }

            @Override
            public void keyboardHidden() {
                nativeKeyboardHidden();
            }
        });
        textInput.install(root);
    }

    /** The insets listener reports to whichever front end owns the window. */
    void onImeVisibility(boolean visible) {
        // An insets dispatch can arrive before onCreate has built the host;
        // keep it harmless rather than letting it throw.
        if (textInput != null) {
            textInput.onImeVisibility(visible);
        }
    }

    private String externalFilesDir() {
        final File dir = getExternalFilesDir(null);
        return dir != null ? dir.getAbsolutePath() : getFilesDir().getAbsolutePath();
    }

    /**
     * Copies the language packs out of the APK into the configuration directory.
     *
     * <p>The packs sit at the **root** of the assets, not in a {@code langs/}
     * folder: the build points `assets.srcDirs` at the repository's `langs`
     * directory and its contents become the root (see
     * `android/app/build.gradle.kts`). Both front ends call this one copy:
     * list the root, take what ends in {@code .ftl}, write them to the same
     * place. There used to be a second implementation in Rust on the
     * {@code NativeActivity} path, and the two destinations drifted once.
     *
     * <p>That place is the catalogue's configuration directory: {@code
     * config_dir()} on Android is {@code <files>/xgview}, so the packs must land
     * in {@code <files>/xgview/langs} - the directory the Rust side scans when it
     * builds the language catalogue (see `XgViewApp::new`). A destination without
     * the {@code xgview} segment puts them where nothing looks, which is a wall
     * in English on a clean install and looks like a working copy on a device the
     * other front end has already run on.
     *
     * <p>Silent on failure: a missing pack costs a language, not the wall, and
     * English is compiled in.
     */
    static void copyLanguagePacks(final Activity host) {
        final AssetManager assets = host.getAssets();
        final File dest = new File(host.getFilesDir(), "xgview/langs");
        final String[] names;
        try {
            names = assets.list("");
        } catch (IOException error) {
            Log.w(TAG, "cannot list the language pack assets", error);
            return;
        }
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
            // Each pack stands on its own. One that cannot be opened or written
            // costs a language, not every language listed after it: that is how
            // the Rust unpacker this replaced behaved, and the NativeActivity
            // front end used to run that one.
            try (InputStream in = assets.open(name);
                 OutputStream out = new FileOutputStream(new File(dest, name))) {
                final byte[] buffer = new byte[8192];
                int read;
                while ((read = in.read(buffer)) > 0) {
                    out.write(buffer, 0, read);
                }
            } catch (IOException error) {
                Log.w(TAG, "cannot copy language pack " + name, error);
            }
        }
    }

    /** Takes the wall's surface; the answer identifies this hand-over. */
    private static native long nativeStartWall(
            android.view.Surface surface,
            String internal,
            String external,
            float density,
            SurfaceActivity activity,
            Class<?> bridge);

    private static native void nativeResizeWall(long token, int width, int height);

    private static native void nativeStopWall(long token);

    private static native void nativeWallFocus(boolean focused);

    private static native boolean nativeKey(
            int keyCode, boolean pressed, boolean repeat, int meta, int unicode);

    private static native void nativeTouch(
            int phase, int id, float x, float y, float force);

    private static native void nativeScroll(float dx, float dy);

    /** `mode` is one of the three `VIDEO_SURFACE_*` hand-overs above. */
    private static native long nativeStartVideo(android.view.Surface surface, int mode);

    private static native void nativeStopVideo(long token);

    private static native long nativeStartBar(
            android.view.Surface surface, int width, int height);

    private static native void nativeStopBar(long token);

    /** What the input method typed; see `installTextInput`. */
    private static native void nativeCommit(String text);

    private static native void nativeBackspace(int count);

    private static native void nativeEnter();

    /** The keyboard went away without the application asking. */
    private static native void nativeKeyboardHidden();
}
