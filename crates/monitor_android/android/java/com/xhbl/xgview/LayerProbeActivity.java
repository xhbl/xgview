package com.xhbl.xgview;

import android.app.Activity;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.Paint;
import android.graphics.PixelFormat;
import android.os.Bundle;
import android.util.Log;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.ViewGroup;
import android.widget.FrameLayout;

/**
 * The one arrangement that would let the control bar float over the picture.
 *
 * <p>Every Android video player that shows controls over a hardware-decoded
 * picture does it the same way: the picture is a {@code SurfaceView} *below* the
 * window, the controls are drawn into the *window's* surface, and the window is
 * translucent, so the picture shows through where the controls are not. Our wall
 * could not use that, because it draws with wgpu into a {@code SurfaceView} of
 * its own rather than into the window - and wgpu's surfaces here are always
 * opaque.
 *
 * <p>This asks whether wgpu can take the window's own surface instead. If the
 * colour below shows through and the block above does not go black, the
 * arrangement works and the control bar can float over the picture.
 *
 * <p>Nothing here decodes anything: the "picture" is a colour painted with a
 * {@code Canvas}, because the compositing question is the same whatever is being
 * painted, and the decoder's half was answered on its own (§13, §14). The colour
 * moves, so a frozen surface cannot be mistaken for a live one.
 *
 * <p>Started by hand:
 * {@code adb shell am start -n com.xhbl.xgview/.LayerProbeActivity}.
 */
public class LayerProbeActivity extends Activity {
    private static final String TAG = "xgview-layer";


    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        System.loadLibrary("monitor_android");

        // The window's surface has to carry alpha, or the picture below it can
        // never show through.
        getWindow().setFormat(PixelFormat.TRANSLUCENT);

        // One view, and nothing in it. A window with no content is a window the
        // framework takes the surface back from - the last run had
        // `window surface created` immediately followed by
        // `window surface destroyed`, so nothing was ever drawn. This keeps the
        // surface alive without putting anything in front of the renderer.
        setContentView(new android.view.View(this));

        // The window's own surface, taken away from the view system so native
        // code can draw into it. This is what an ordinary app's window gives its
        // views; here it is given to wgpu.
        getWindow().takeSurface(new SurfaceHolder.Callback2() {
            @Override
            public void surfaceCreated(SurfaceHolder holder) {
                Log.i(TAG, "window surface created");
                nativeStartUi(holder.getSurface());
            }

            @Override
            public void surfaceChanged(SurfaceHolder holder, int format, int width, int height) {
                Log.i(TAG, "window surface " + width + "x" + height);
            }

            @Override
            public void surfaceDestroyed(SurfaceHolder holder) {
                Log.i(TAG, "window surface destroyed");
                nativeStopUi();
            }

            @Override
            public void surfaceRedrawNeeded(SurfaceHolder holder) {}
        });

    }

    @Override
    protected void onDestroy() {
        nativeStopUi();
        super.onDestroy();
    }

    private static native void nativeStartUi(android.view.Surface surface);

    private static native void nativeStopUi();
}
