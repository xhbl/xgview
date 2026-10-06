package com.xhbl.xgview;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.util.Log;

/**
 * Start-on-boot entry point for XGView.
 *
 * <p>The Android TV box is expected to run unattended: after a power cut it must
 * come back on the surveillance wall without anybody touching the remote. This
 * receiver is registered for {@code BOOT_COMPLETED} (and the vendor specific
 * {@code QUICKBOOT_POWERON} variants) in {@code AndroidManifest.xml}.
 *
 * <p>Because the UI is a {@code NativeActivity}, the receiver does not need to
 * know anything about the native library: it simply asks the package manager for
 * the launcher intent of this package, which respects the activity declared in
 * the manifest and the user's "default launcher app" choice.
 *
 * <p>Note on Android 10 (API 29) and newer: background activity starts are
 * restricted. The launch below therefore only succeeds when one of the following
 * holds, which is the normal situation on a dedicated signage / surveillance
 * box:
 *
 * <ul>
 *   <li>XGView is the device owner / has been granted
 *       {@code SYSTEM_ALERT_WINDOW}, or</li>
 *   <li>the OEM ROM whitelists the app for auto start (most Amlogic / Rockchip
 *       TV boxes expose such an option), or</li>
 *   <li>the box runs a foreground service / launcher replacement of ours.</li>
 * </ul>
 *
 * <p>If the activity is refused, the user can still enable XGView as the default
 * launcher, or add it to the auto-start whitelist of the box.
 */
public class BootReceiver extends BroadcastReceiver {

    private static final String TAG = "XGView.BootReceiver";

    @Override
    public void onReceive(Context context, Intent intent) {
        final String action = intent == null ? null : intent.getAction();
        Log.i(TAG, "received " + action + ", relaunching XGView");

        final Intent launch = context.getPackageManager()
                .getLaunchIntentForPackage(context.getPackageName());
        if (launch == null) {
            Log.w(TAG, "no launcher intent for " + context.getPackageName());
            return;
        }

        // Start the existing task instead of stacking a new activity: the
        // activity is declared with launchMode="singleTask".
        launch.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK
                | Intent.FLAG_ACTIVITY_CLEAR_TOP
                | Intent.FLAG_ACTIVITY_SINGLE_TOP);
        // Marks this launch as the boot receiver's, so the toast the viewer
        // shows on start can tell it apart from a manual open.
        launch.putExtra("com.xhbl.xgview.FROM_AUTOSTART", true);

        try {
            context.startActivity(launch);
        } catch (RuntimeException error) {
            // Typically a background-start restriction (Android 10+): log it so
            // the reason shows up in `adb logcat -s XGView.BootReceiver`.
            Log.w(TAG, "could not start the activity", error);
        }
    }
}
