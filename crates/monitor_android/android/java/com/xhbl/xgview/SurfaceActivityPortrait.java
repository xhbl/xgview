package com.xhbl.xgview;

/**
 * The wall on surfaces of its own, standing upright: {@link SurfaceActivity}
 * with a portrait-fixed {@code screenOrientation}, so the window comes up
 * already turned.
 *
 * <p>Launched by {@link LauncherActivity} when the remembered orientation is
 * portrait. Everything else is inherited: the three surfaces, the codec's
 * window and the controls do not care which way the wall stands, and a viewer
 * who turns it from the toolbar stays in this activity - {@code setOrientation}
 * asks for the other direction without recreating it.
 *
 * <p>The class needs no native declarations of its own. The JNI bridge holds
 * {@code MainActivity.class}, and the static methods it calls forward to
 * {@link SurfaceActivity}, whose {@code current()} is this instance too.
 */
public class SurfaceActivityPortrait extends SurfaceActivity {
}
