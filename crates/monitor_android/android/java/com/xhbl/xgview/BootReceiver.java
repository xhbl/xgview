package com.xhbl.xgview;

import android.app.AlarmManager;
import android.app.PendingIntent;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.os.SystemClock;
import android.util.Log;

/**
 * Start-on-boot entry point for XGView.
 *
 * <p>The Android TV box is expected to run unattended: after a power cut it must
 * come back on the surveillance wall without anybody touching the remote. This
 * receiver is registered for {@code BOOT_COMPLETED} (and the vendor specific
 * {@code QUICKBOOT_POWERON} variants) in {@code AndroidManifest.xml}.
 *
 * <p>It is not direct boot aware, so it runs only once the device is usable.
 * The configuration lives in credential-encrypted storage, so before the user
 * unlocks there is no camera list to draw and nothing this receiver could
 * usefully start - the manifest says the rest.
 *
 * <p>Because the UI is a {@code NativeActivity}, the receiver does not need to
 * know anything about the native library: it simply asks the package manager for
 * the launcher intent of this package, which respects the activity declared in
 * the manifest and the user's "default launcher app" choice.
 *
 * <p>The receiver is exported, which lets any app on the device send it an
 * intent and bring the viewer to the foreground. That is a deliberate trade -
 * the manifest says what it costs, why an action check would not help, and what
 * would have to be true to set {@code exported="false"} instead. Keep the work
 * done here to starting the launcher intent: that bound is what makes the trade
 * acceptable.
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
 *
 * <p><b>One start is not always enough.</b> Measured on the SHIELD: a boot under
 * load logged {@code Start proc ... for broadcast BootReceiver}, then
 * {@code START ... LauncherActivity} with the {@code SYSTEM_ALERT_WINDOW}
 * allowance, and then nothing at all - no activity was ever created, not one log
 * line came out of the process, and seven seconds later the system killed it as
 * {@code empty #17} along with eight other processes. The launch had been lost
 * in the boot, and nothing noticed. So the boot path now leaves a retry behind:
 * an alarm eight seconds later that starts the activity again if
 * {@link MainActivity#KEY_RESUMED_AT} has not moved past the boot. Two retries,
 * and only while the wall has never reached the foreground, so a box where the
 * wall came up is never disturbed and a viewer who pressed Home is never
 * overruled.
 */
public class BootReceiver extends BroadcastReceiver {

    private static final String TAG = "XGView.BootReceiver";

    /** Sent by the alarm this receiver schedules for itself; not in the manifest. */
    private static final String ACTION_RETRY = "com.xhbl.xgview.RETRY_START";

    /** When the boot broadcast arrived, as wall clock milliseconds. */
    private static final String KEY_BOOT_AT = "boot_at";

    /** How many retries this boot has already used. */
    private static final String KEY_RETRIES = "boot_retries";

    /**
     * How long to wait before checking whether the start took.
     *
     * <p>Long enough for the boot's own storm - the launchers, the media
     * providers, the accessibility service - to have settled, so the retry is
     * not lost the same way the first start was.
     */
    private static final long RETRY_DELAY_MS = 8000;

    /** How many times the start may be repeated. */
    private static final int RETRY_LIMIT = 2;

    @Override
    public void onReceive(Context context, Intent intent) {
        final String action = intent == null ? null : intent.getAction();
        if (ACTION_RETRY.equals(action)) {
            retry(context);
            return;
        }
        Log.i(TAG, "received " + action + ", relaunching XGView");

        final SharedPreferences prefs =
                context.getSharedPreferences(MainActivity.PREFS, Context.MODE_PRIVATE);
        // Committed before the launch, not after: the retry compares the wall's
        // resume time against this, and it must not be later than the start it
        // is judging.
        prefs.edit()
                .putLong(KEY_BOOT_AT, System.currentTimeMillis())
                .putInt(KEY_RETRIES, 0)
                .commit();

        start(context);
        scheduleRetry(context);
    }

    /**
     * Starts the viewer, and reports nothing: whether it worked is answered by
     * whether the activity reaches the foreground, which is a different
     * question and is asked by {@link #retry}.
     */
    private void start(Context context) {
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

    /**
     * Starts the viewer again if the boot's first start never reached the
     * foreground, and does nothing if it did.
     *
     * <p>The question is answered by {@link MainActivity#KEY_RESUMED_AT}: the
     * activity stamps it in {@code onResume}, so a value newer than the boot is
     * proof that the wall came up - whatever happened to it afterwards is the
     * activity's business, and this must not interfere. A value older than the
     * boot is the case the retry exists for.
     */
    private void retry(Context context) {
        final SharedPreferences prefs =
                context.getSharedPreferences(MainActivity.PREFS, Context.MODE_PRIVATE);
        final long bootAt = prefs.getLong(KEY_BOOT_AT, 0);
        final long resumedAt = prefs.getLong(MainActivity.KEY_RESUMED_AT, 0);
        if (bootAt == 0) {
            // No boot was recorded - the preferences were cleared, or this is
            // the first run - so there is nothing to judge. Starting the viewer
            // on a broadcast that answers no boot would be a start nobody asked
            // for.
            Log.i(TAG, "no boot recorded; nothing to retry");
            return;
        }
        if (resumedAt >= bootAt) {
            Log.i(TAG, "the wall came up " + (resumedAt - bootAt) + "ms after the boot");
            return;
        }
        final int used = prefs.getInt(KEY_RETRIES, 0) + 1;
        Log.w(TAG, "the wall never came up after the boot; starting it again ("
                + used + "/" + RETRY_LIMIT + ")");
        prefs.edit().putInt(KEY_RETRIES, used).commit();
        start(context);
        if (used < RETRY_LIMIT) {
            scheduleRetry(context);
        }
    }

    /**
     * Leaves one retry behind.
     *
     * <p>An alarm rather than a posted callback: the process this receiver runs
     * in is a cached one with nothing in it once {@code onReceive} returns, and
     * on the boot that lost the start it was killed as exactly that.
     *
     * <p>A plain {@code set} rather than an exact alarm, for the same reason
     * {@code MainActivity.restartSoon} uses one: an exact alarm needs the
     * permission Android 12 added, and a retry a second or two late is still a
     * retry.
     */
    private void scheduleRetry(Context context) {
        final Intent retry = new Intent(context, BootReceiver.class);
        retry.setAction(ACTION_RETRY);
        final PendingIntent pending = PendingIntent.getBroadcast(context, 0, retry,
                PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
        final AlarmManager alarms =
                (AlarmManager) context.getSystemService(Context.ALARM_SERVICE);
        if (alarms == null) {
            Log.w(TAG, "no alarm service to leave a retry with");
            return;
        }
        alarms.set(AlarmManager.ELAPSED_REALTIME_WAKEUP,
                SystemClock.elapsedRealtime() + RETRY_DELAY_MS, pending);
    }
}
