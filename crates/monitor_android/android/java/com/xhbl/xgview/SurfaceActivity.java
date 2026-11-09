package com.xhbl.xgview;

import android.app.Activity;
import android.content.res.AssetManager;
import android.os.Build;
import android.os.Bundle;
import android.util.Log;
import android.view.KeyEvent;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.View;
import android.view.ViewGroup;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.view.inputmethod.InputMethodManager;
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
    /** The decoded picture, between the wall and the bar. */
    private SurfaceView video;
    /** The controls, over the picture. */
    private SurfaceView bar;
    /** Whether the picture's view is on screen; see {@link #setVideoVisible}. */
    private boolean videoShown = true;
    /** How much bigger than its box the picture is drawn; see {@link #setVideoZoom}. */
    private float videoScale = 1f;
    /** Where that drawing starts, relative to the box's corner, in pixels. */
    private float videoDx = 0f;
    private float videoDy = 0f;

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
                    Log.i(TAG, "video surface destroyed");
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
                    Log.i(TAG, "bar surface destroyed");
                    nativeStopBar();
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
    /**
     * Keys the interface knows are taken; the rest are left to the system.
     *
     * <p>Answering {@code true} for everything would be simpler and is wrong:
     * the wall maps the remote's own buttons and a keyboard's letters, and
     * nothing else - so a key it does not map would stop there, and the volume
     * keys, the media keys and the language switch would do nothing at all. The
     * native side answers whether it took the key, and that answer is what this
     * returns.
     */
    @Override
    public boolean onKeyDown(int keyCode, KeyEvent event) {
        return nativeKey(keyCode, true, event.getRepeatCount() > 0, event.getMetaState());
    }

    @Override
    public boolean onKeyUp(int keyCode, KeyEvent event) {
        return nativeKey(keyCode, false, false, event.getMetaState());
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
    protected void onResume() {
        super.onResume();
        boot.onResume();
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
        nativeStopWall();
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
            self.applyVideoPlacement();
        });
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
            final FrameLayout.LayoutParams params = new FrameLayout.LayoutParams(w, h);
            params.setMargins(left, top, 0, 0);
            self.video.setLayoutParams(params);
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
            // are the whole of where the picture starts.
            self.video.setPivotX(0f);
            self.video.setPivotY(0f);
            self.video.setScaleX(self.videoScale);
            self.video.setScaleY(self.videoScale);
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
        final int height = raw != null && raw.height > 0 ? raw.height : video.getHeight();
        final int top = raw instanceof FrameLayout.LayoutParams
                ? ((FrameLayout.LayoutParams) raw).topMargin
                : 0;
        video.setTranslationX(videoShown ? videoDx : 0f);
        video.setTranslationY(videoShown ? videoDy : -(top + height * videoScale));
    }

    private android.os.PowerManager.WakeLock wakeLock;

    // ------------------------------------------------------------ soft keyboard
    //
    // The same one-pixel editable view `MainActivity` installs, for the same
    // reason: Android shows a keyboard for a focused editable view and for
    // nothing else. The text is never kept here - every commit goes to the
    // native side, which owns the string.

    /** Text the input method has spelled out but not settled on yet. */
    private String composing = "";

    /** The one pixel view the input method types into. */
    private View inputView;

    private InputMethodManager inputMethod;

    /** Whether the native side wants the keyboard up, as last told by {@link #setKeyboardWanted}. */
    private boolean keyboardWanted;

    /** Whether the field being edited holds several lines; see {@link #setKeyboardWanted}. */
    private boolean keyboardMultiline;

    /**
     * Whether the keyboard has been seen on screen since it was last asked for.
     *
     * <p>A report that it is not showing means nothing until it has shown: the
     * input method takes a moment to come up, and the first answer is always
     * "not yet".
     */
    private boolean imeSeenVisible;

    /** The hidden call that reports the keyboard's height, found once. */
    private java.lang.reflect.Method imeHeightMethod;

    /** Asks the input method how tall it is, while the native side wants it up. */
    private final Runnable imePoll = new Runnable() {
        @Override
        public void run() {
            if (!keyboardWanted) {
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

    /**
     * Called from the native side when a text field takes or loses the focus, so
     * that the keyboard follows what the viewer is doing in the UI.
     *
     * <p>{@code multiline} is the shape of the field being edited: see
     * {@link #keyboardMultiline}.
     */
    public static void setKeyboardWanted(final boolean wanted, final boolean multiline) {
        final SurfaceActivity self = instance;
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
                self.inputView.postDelayed(self.imePoll, 600);
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

    /**
     * Adds the view the input method attaches to.
     *
     * <p>It is one pixel in the corner and draws nothing: what matters is that
     * it is editable and can hold the focus. Keys it does not take - the arrows,
     * the remote's own buttons - fall through to the activity as usual, so the
     * viewer can still walk the form while the keyboard is up.
     */
    private void installTextInput(final FrameLayout root) {
        inputMethod = (InputMethodManager) getSystemService(android.content.Context.INPUT_METHOD_SERVICE);
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
            public android.view.inputmethod.InputConnection onCreateInputConnection(
                    android.view.inputmethod.EditorInfo out) {
                // The kind of field the native side is editing decides the
                // action key. A single-line field finishes on Done; a multi-line
                // one must keep Enter as a newline, because that key cannot both
                // break the line and take the text.
                if (keyboardMultiline) {
                    out.inputType = android.text.InputType.TYPE_CLASS_TEXT
                            | android.text.InputType.TYPE_TEXT_FLAG_MULTI_LINE
                            | android.text.InputType.TYPE_TEXT_VARIATION_URI;
                    out.imeOptions = android.view.inputmethod.EditorInfo.IME_ACTION_NONE
                            | android.view.inputmethod.EditorInfo.IME_FLAG_NO_ENTER_ACTION
                            | android.view.inputmethod.EditorInfo.IME_FLAG_NO_FULLSCREEN;
                } else {
                    out.inputType = android.text.InputType.TYPE_CLASS_TEXT
                            | android.text.InputType.TYPE_TEXT_VARIATION_URI;
                    out.imeOptions = android.view.inputmethod.EditorInfo.IME_ACTION_DONE
                            | android.view.inputmethod.EditorInfo.IME_FLAG_NO_FULLSCREEN;
                }
                return new android.view.inputmethod.BaseInputConnection(this, false) {
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
        root.addView(inputView, new FrameLayout.LayoutParams(1, 1));
    }

    /**
     * The input method spelled something out: whatever it had composed so far
     * is replaced by this.
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
                imeHeightMethod = InputMethodManager.class
                        .getMethod("getInputMethodWindowVisibleHeight");
            }
            return (Integer) imeHeightMethod.invoke(inputMethod);
        } catch (Throwable unsupported) {
            return -1;
        }
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

    private static native boolean nativeKey(
            int keyCode, boolean pressed, boolean repeat, int meta);

    private static native void nativeTouch(
            int phase, int id, float x, float y, float force);

    private static native void nativeScroll(float dx, float dy);

    private static native void nativeStartVideo(android.view.Surface surface);

    private static native void nativeStopVideo();

    private static native void nativeStartBar(
            android.view.Surface surface, int width, int height);

    private static native void nativeStopBar();

    /** What the input method typed; see `installTextInput`. */
    private static native void nativeCommit(String text);

    private static native void nativeBackspace(int count);

    private static native void nativeEnter();

    /** The keyboard went away without the application asking. */
    private static native void nativeKeyboardHidden();
}
