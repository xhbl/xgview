package com.xhbl.xgview;

import android.app.Activity;
import android.content.Intent;
import android.os.Handler;
import android.os.Looper;
import android.os.SystemClock;
import android.util.Log;

/**
 * Keeps the wall in front after a start-on-boot.
 *
 * <p>Being covered is the one thing a wall must not accept quietly at boot: the
 * box is unattended, so a wall that lost the race for the foreground stays lost
 * until somebody walks up to it. A boot launch therefore opens a grace window,
 * and a pause inside it schedules the wall to ask for the foreground back.
 *
 * <p>No attempt is made to tell a cover from the viewer pressing Home.
 * {@code onUserLeaveHint} is the documented way to know, and it was tried and
 * measured: on this box it fires when a launcher starts itself at boot with no
 * key pressed at all, so gating on it silenced the case this exists for. A wall
 * that has been up for a while and is then covered by a launcher is also worth
 * answering - Projectivy does take the screen back on its own several seconds
 * in, and obeying that leaves the wall behind with nobody having asked. So the
 * window is the only bound, and the viewer's way out is the one this was always
 * going to be: past the window, or past the limit, a Home press stays pressed.
 *
 * <p>One of these belongs to each front end, because the grace window is about
 * the activity that was launched and the retry is scheduled from its own pause.
 */
final class BootRace {

    /** How long after a boot launch the wall may take the foreground back. */
    private static final long GRACE_MS = 60 * 1000L;

    /**
     * How many times it may try.
     *
     * <p>Two, and the number is measured rather than chosen: an attempt that
     * loses to the launcher's own start by a few hundred milliseconds needs the
     * second one, and a third has never been seen to change the answer.
     */
    private static final int LIMIT = 2;

    /** How long after a pause the first attempt goes, and between attempts. */
    private static final long RETRY_MS = 1500;

    private final Activity self;
    private final Handler ui = new Handler(Looper.getMainLooper());
    private final Runnable again = this::take;

    private long until;
    private int attempts;
    private boolean resumed;
    private long lastResumedAt;

    BootRace(final Activity self) {
        this.self = self;
    }

    /** Opens the grace window. Called once, on a launch the boot receiver made. */
    void arm() {
        until = SystemClock.elapsedRealtime() + GRACE_MS;
    }

    /** Whether this launch was a boot launch, as far as the grace window knows. */
    boolean armed() {
        return until != 0;
    }

    /**
     * The wall is on screen again, so a re-assert scheduled by a cover that has
     * since gone away has nothing left to do.
     */
    void onResume() {
        resumed = true;
        lastResumedAt = SystemClock.elapsedRealtime();
        // Wall clock, and on disk: the boot retry compares it against the boot
        // and cannot read anything this process holds in memory.
        self.getSharedPreferences(MainActivity.PREFS, Activity.MODE_PRIVATE).edit()
                .putLong(MainActivity.KEY_RESUMED_AT, System.currentTimeMillis()).apply();
        ui.removeCallbacks(again);
    }

    /** Notes that something else is on screen, and schedules the wall to ask. */
    void onPause() {
        resumed = false;
        final long now = SystemClock.elapsedRealtime();
        final long shown = lastResumedAt == 0 ? 0 : now - lastResumedAt;
        final long left = until == 0 ? 0 : until - now;
        Log.i("xgview", "paused; shown=" + shown + " graceLeft=" + left
                + " attempts=" + attempts);
        // A wall that is on its way out must not pull itself back: the pause
        // would otherwise be answered with a start that undoes the exit.
        if (self.isFinishing() || self.isDestroyed()) {
            return;
        }
        if (until == 0 || now >= until) {
            return;
        }
        if (attempts >= LIMIT) {
            return;
        }
        ui.removeCallbacks(again);
        ui.postDelayed(again, RETRY_MS);
    }

    /**
     * Takes the foreground back if the wall is still covered, and does nothing
     * if it is not.
     *
     * <p>Runs on the main thread a moment after the pause, so the state it acts
     * on is the settled one rather than a start that was still in flight.
     *
     * <p>The intent re-sent is the launcher intent - {@code LauncherActivity},
     * the same entry the boot receiver uses - not the activity's own. Measured
     * on the device: re-sending the activity is a no-op, because it is already
     * the root of its task and the flags that bring a task forward make
     * {@code ActivityStarter} treat it as "already at the top, deliver
     * {@code onNewIntent}". Two attempts went out and neither produced a
     * resume; the activity was still there, still paused, and the launcher
     * stayed in front. Starting the exported entry instead does bring the task
     * back, and it also picks the right one of the two orientations.
     *
     * <p>Schedules the next attempt itself instead of waiting for another pause.
     * A cover is one pause, and an attempt that loses is still a pause that has
     * already happened - so waiting for the next one made the limit mean "one
     * try, then never again": measured on the device, an attempt that lost to
     * the launcher's own start by 658 ms left the wall behind for the rest of
     * the window with one attempt unspent. The chain stops at {@link #onResume},
     * which drops the callback.
     *
     * <p>Whether the system allows it at all depends on the same whitelist that
     * let the boot receiver start us - {@code SYSTEM_ALERT_WINDOW}; see the
     * manifest. A refusal is not retried beyond the limit.
     */
    private void take() {
        if (resumed) {
            return;
        }
        if (until == 0 || SystemClock.elapsedRealtime() >= until) {
            return;
        }
        if (attempts >= LIMIT) {
            return;
        }
        final Intent back = self.getPackageManager().getLaunchIntentForPackage(self.getPackageName());
        if (back == null) {
            Log.w("xgview", "no launcher intent to take the foreground back with");
            return;
        }
        attempts++;
        Log.i("xgview", "covered during boot; taking the foreground back ("
                + attempts + "/" + LIMIT + ")");
        back.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK
                | Intent.FLAG_ACTIVITY_CLEAR_TOP
                | Intent.FLAG_ACTIVITY_SINGLE_TOP);
        try {
            self.startActivity(back);
        } catch (RuntimeException error) {
            Log.w("xgview", "cannot take the foreground back", error);
        }
        // The answer is whether the wall is resumed, not whether the start was
        // accepted, so look again rather than assuming this one worked.
        ui.postDelayed(again, RETRY_MS);
    }
}
