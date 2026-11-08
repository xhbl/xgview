package com.xhbl.xgview;

import android.app.Activity;
import android.graphics.Color;
import android.os.Bundle;
import android.util.Log;
import android.view.Gravity;
import android.view.KeyEvent;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.ViewGroup;
import android.widget.FrameLayout;
import android.widget.TextView;

/**
 * Measurement scaffolding: two surfaces in one ordinary activity.
 *
 * <p>The wall runs in a {@code NativeActivity}, and that has turned out to be
 * the reason the decoder cannot be given a surface of its own: a window given
 * to native drawing cannot also host a view hierarchy (the framework's drawing
 * wipes the native one), and a separate window takes the activity's input with
 * it. This activity answers the question those two failures leave open -
 * whether an ordinary activity can host a {@code SurfaceView} the renderer
 * draws into, with a view above it that keeps its place and its keys.
 *
 * <p>Nothing in the app launches it. It is started by hand:
 * {@code adb shell am start -n com.xhbl.xgview/.ProbeActivity}.
 */
public class ProbeActivity extends Activity {
    private static final String TAG = "xgview-probe";

    /** The label over the picture: it counts keys, so input can be seen to work. */
    private TextView label;
    private int keys;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        // This activity is started on its own, so nothing else has loaded the
        // library for it.
        System.loadLibrary("monitor_android");

        final FrameLayout root = new FrameLayout(this);
        // The renderer's surface, underneath.
        final SurfaceView canvas = new SurfaceView(this);
        canvas.getHolder().addCallback(new SurfaceHolder.Callback() {
            @Override
            public void surfaceCreated(SurfaceHolder holder) {
                Log.i(TAG, "canvas created");
                nativeStart(holder.getSurface());
            }

            @Override
            public void surfaceChanged(SurfaceHolder holder, int format, int width, int height) {
                nativeResize(width, height);
            }

            @Override
            public void surfaceDestroyed(SurfaceHolder holder) {
                nativeStop();
            }
        });
        root.addView(canvas, new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.MATCH_PARENT));

        // A view over it: if it is visible, the order is right, and if its
        // counter moves, the keys still arrive here.
        label = new TextView(this);
        label.setText("probe: 0 keys");
        label.setTextColor(Color.WHITE);
        label.setTextSize(28f);
        label.setGravity(Gravity.TOP | Gravity.START);
        label.setPadding(48, 48, 48, 48);
        root.addView(label, new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT));

        setContentView(root);
    }

    @Override
    public boolean onKeyDown(int keyCode, KeyEvent event) {
        keys += 1;
        label.setText("probe: " + keys + " keys, last " + keyCode);
        Log.i(TAG, "key " + keyCode);
        return true;
    }

    @Override
    protected void onDestroy() {
        nativeStop();
        super.onDestroy();
    }

    /** Hands the renderer a surface of the app's own, not the activity's window. */
    private static native void nativeStart(android.view.Surface surface);

    private static native void nativeResize(int width, int height);

    private static native void nativeStop();
}
