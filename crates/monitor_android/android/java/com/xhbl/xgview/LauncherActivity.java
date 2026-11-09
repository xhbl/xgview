package com.xhbl.xgview;

import android.app.Activity;
import android.content.ComponentName;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.os.Bundle;
import android.util.Log;

import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileInputStream;
import java.nio.charset.StandardCharsets;

/**
 * The transparent entry point that picks the orientation the wall comes up in.
 *
 * <p>Android fixes an activity's window orientation from the manifest at
 * creation time, before any code runs, so changing it from {@code onCreate}
 * always produces a visible rotation. To come up already turned the right way,
 * two fixed-orientation activities are declared in the manifest - one
 * landscape, one portrait - and this launcher reads the remembered preference
 * from {@code xgvconfig.json} and starts the matching one. It has no UI and
 * finishes at once, so the only thing the viewer sees is the wall in the right
 * orientation.
 */
public class LauncherActivity extends Activity {

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        final boolean portrait = shouldStartPortrait();
        // A wall already alive is brought back rather than a second activity of
        // the other orientation started beside it: the two would race for the
        // foreground, and the one that lost would clear the static the native
        // side talks to. Its class, not the remembered preference, is what the
        // running wall actually is.
        //
        // Both front ends count, and the surface one has to be asked first-hand:
        // it is not a `MainActivity`, so `MainActivity.current()` is null while
        // it is on screen. That is how a boot retry started a `NativeActivity`
        // into a process that was already running the wall on a `SurfaceView` -
        // and the Android context can only be installed once per process, so
        // the second one aborted it. See `monitor_gui::android::CONTEXT`.
        final MainActivity running = MainActivity.current();
        final SurfaceActivity surface = SurfaceActivity.current();
        final String target;
        if (running != null) {
            target = running.getClass().getName();
        } else if (surface != null) {
            target = "com.xhbl.xgview.SurfaceActivity";
        } else if (portrait) {
            // Upright, the wall keeps the `NativeActivity` it has always had:
            // the three-surface arrangement is the landscape one, and a phone
            // held upright has a picture that fits its window without help.
            target = "com.xhbl.xgview.MainActivityPortrait";
        } else {
            // Landscape goes to the wall on surfaces of its own. The picture is
            // decoded straight into a `SurfaceView` and never read back, which
            // is the difference between 26 frames a second and 12 for a 1080p
            // camera on a box that cannot afford the copy.
            target = "com.xhbl.xgview.SurfaceActivity";
        }
        final Intent intent = new Intent();
        intent.setComponent(new ComponentName(getPackageName(), target));
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK
                | Intent.FLAG_ACTIVITY_CLEAR_TOP
                | Intent.FLAG_ACTIVITY_SINGLE_TOP);
        final Intent incoming = getIntent();
        if (incoming != null) {
            intent.putExtras(incoming);
        }
        startActivity(intent);
        finish();
    }

    private boolean shouldStartPortrait() {
        final PackageManager packages = getPackageManager();
        final boolean touch = packages != null
                && packages.hasSystemFeature(PackageManager.FEATURE_TOUCHSCREEN);
        final boolean television = packages != null
                && (packages.hasSystemFeature(PackageManager.FEATURE_LEANBACK)
                        || packages.hasSystemFeature(PackageManager.FEATURE_TELEVISION));
        if (!touch || television) {
            return false;
        }
        final File config = new File(new File(getFilesDir(), "xgview"), "xgvconfig.json");
        if (!config.isFile()) {
            return false;
        }
        try (FileInputStream in = new FileInputStream(config)) {
            final ByteArrayOutputStream buf = new ByteArrayOutputStream();
            final byte[] block = new byte[4096];
            int n;
            while ((n = in.read(block)) > 0) {
                buf.write(block, 0, n);
            }
            final String json = buf.toString(StandardCharsets.UTF_8.name());
            final org.json.JSONObject obj = new org.json.JSONObject(json);
            return "portrait".equals(obj.optString("android_orientation", "landscape"));
        } catch (Exception e) {
            Log.w("xgview", "cannot read orientation from " + config, e);
            return false;
        }
    }
}