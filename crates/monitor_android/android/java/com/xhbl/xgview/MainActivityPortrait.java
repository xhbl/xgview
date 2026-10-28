package com.xhbl.xgview;

/**
 * The wall in portrait: {@link MainActivity} with a portrait-fixed
 * {@code screenOrientation}, so the window comes up already turned.
 *
 * <p>Launched by {@link LauncherActivity} when the remembered orientation is
 * portrait; everything else is inherited. The {@code private static native}
 * methods declared on {@code MainActivity} are resolved through that class by
 * JNI, so the subclass needs no declarations of its own.
 */
public class MainActivityPortrait extends MainActivity {
}