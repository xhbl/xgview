package com.xhbl.xgview;

import android.app.Activity;
import android.content.Context;
import android.os.Build;
import android.text.InputType;
import android.util.Log;
import android.view.KeyEvent;
import android.view.View;
import android.view.ViewGroup;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;

import java.lang.reflect.Method;

/**
 * The one-pixel editable view the input method types into, and everything that
 * goes with it.
 *
 * <p>There used to be two copies of this, one in {@link MainActivity} and one
 * in {@link SurfaceActivity}. They started out identical and then diverged:
 * the {@code SDK_INT < R} gate on the height poll was added to one and not the
 * other, which is how a keyboard came up on a television box and was pulled
 * down again a poll later. The two activities cannot share a base class - one
 * is a {@code NativeActivity}, the other a plain {@code Activity} - and an
 * interface cannot hold the state this needs, so they each hold one of these
 * and keep only their own root view and their own JNI methods.
 *
 * <p>Java only carries the characters; the text itself lives in the native
 * side. The four {@link Native} callbacks are the whole difference between the
 * two front ends: their JNI entry points have different names but the same
 * bodies.
 */
final class TextInputHost {

    private static final String TAG = "XGView.TextInput";

    /** What the native side is asked to do with what was typed. */
    interface Native {
        void commit(String text);

        void backspace(int count);

        void enter();

        void keyboardHidden();
    }

    private final Activity activity;
    private final Native nativeSide;
    private final InputMethodManager inputMethod;
    private final View inputView;

    /** Whether the native side wants the keyboard up. */
    private boolean keyboardWanted;

    /** Whether the field being edited holds several lines. */
    private boolean keyboardMultiline;

    /**
     * Whether the keyboard has been seen on screen since it was last asked for.
     * A report that it is not showing means nothing until it has shown: the
     * input method takes a moment to come up, and the first answer is always
     * "not yet".
     */
    private boolean imeSeenVisible;

    /** Text the input method has spelled out but not settled on yet. */
    private String composing = "";

    /** The hidden call that reports the keyboard's height, found once. */
    private Method imeHeightMethod;

    /**
     * Asks the input method how tall it is, on the releases whose insets carry
     * no keyboard source. On Android 11 and newer the insets listener is the
     * only witness that can be trusted: the hidden height call answers zero on
     * plenty of devices while the keyboard is up, which reads as the viewer
     * having put it away. The two must never both speak.
     */
    private final Runnable imePoll = new Runnable() {
        @Override
        public void run() {
            if (!keyboardWanted || Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
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

    TextInputHost(final Activity activity, final Native nativeSide) {
        this.activity = activity;
        this.nativeSide = nativeSide;
        inputMethod = (InputMethodManager) activity.getSystemService(Context.INPUT_METHOD_SERVICE);
        inputView = new View(activity) {
            @Override
            public boolean onCheckIsTextEditor() {
                return true;
            }

            /**
             * Back while the keyboard is up belongs to the input method: it
             * puts itself away and the key never reaches the activity, so the
             * native side would go on believing the keyboard is there. This is
             * the one place that sees the key first.
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
            public InputConnection onCreateInputConnection(EditorInfo out) {
                // The kind of field the native side is editing decides the
                // action key. A single-line field finishes on Done; a
                // multi-line one must keep Enter as a newline, because that
                // key cannot both break the line and take the text - Back, or
                // the floating box's Done, is how a multi-line field is left.
                if (keyboardMultiline) {
                    out.inputType = InputType.TYPE_CLASS_TEXT
                            | InputType.TYPE_TEXT_FLAG_MULTI_LINE
                            | InputType.TYPE_TEXT_VARIATION_URI;
                    out.imeOptions = EditorInfo.IME_ACTION_NONE
                            | EditorInfo.IME_FLAG_NO_ENTER_ACTION
                            | EditorInfo.IME_FLAG_NO_FULLSCREEN;
                } else {
                    out.inputType = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_URI;
                    out.imeOptions = EditorInfo.IME_ACTION_DONE | EditorInfo.IME_FLAG_NO_FULLSCREEN;
                }
                return new BaseInputConnection(this, false) {
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
                            nativeSide.backspace(beforeLength);
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
                                nativeSide.backspace(1);
                                return true;
                            case KeyEvent.KEYCODE_ENTER:
                            case KeyEvent.KEYCODE_NUMPAD_ENTER:
                                composing = "";
                                nativeSide.enter();
                                return true;
                            default:
                                final int unicode = event.getUnicodeChar();
                                if (unicode != 0) {
                                    // A character from the keyboard itself
                                    // rather than a commit - some input methods
                                    // spell a word out this way.
                                    composing = "";
                                    nativeSide.commit(String.valueOf((char) unicode));
                                    return true;
                                }
                                // Not ours: let the remote's buttons reach
                                // the activity.
                                return false;
                        }
                    }
                };
            }
        };
        // Focusable only while the keyboard is wanted. Left focusable, the
        // window hands this view the focus the moment it opens and the input
        // method comes up over the wall unasked - and while it has the focus
        // it takes the remote's buttons too.
        inputView.setFocusable(false);
        inputView.setFocusableInTouchMode(false);
    }

    /**
     * Adds the view to whichever root this front end owns. It is one pixel in
     * the corner and draws nothing: what matters is that it is editable and
     * can hold the focus. Keys it does not take fall through to the activity.
     */
    void install(final ViewGroup root) {
        root.addView(inputView, new ViewGroup.LayoutParams(1, 1));
    }

    /**
     * Called from the native side when a text field takes or loses the focus,
     * so that the keyboard follows what the viewer is doing in the UI. The
     * bridge that reaches this method lives on the activities, because only
     * they know which front end is on screen.
     */
    void setWanted(final boolean wanted, final boolean multiline) {
        activity.runOnUiThread(() -> {
            // Read before they are overwritten: the action key can only be
            // changed by remaking the input connection, and that is worth
            // doing only when the keyboard is already up on a field of the
            // other kind.
            final boolean wasWanted = keyboardWanted;
            final boolean optionsChanged = keyboardMultiline != multiline;
            // Told before the keyboard is asked to move, so that what it does
            // in answer to this is not taken for the viewer putting it away.
            keyboardWanted = wanted;
            keyboardMultiline = multiline;
            imeSeenVisible = false;
            inputView.removeCallbacks(imePoll);
            Log.i(TAG, "keyboard wanted=" + wanted + " multiline=" + multiline);
            if (wanted) {
                // Only the releases whose insets carry no keyboard source get
                // the height poll; see imePoll.
                if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) {
                    inputView.postDelayed(imePoll, 600);
                }
                inputView.setFocusable(true);
                inputView.setFocusableInTouchMode(true);
                inputView.requestFocus();
                inputMethod.showSoftInput(inputView, 0);
                if (wasWanted && optionsChanged) {
                    // The action key is fixed when the input connection is
                    // made, so a move between a single-line and a multi-line
                    // field while the keyboard stays up needs it remade.
                    inputMethod.restartInput(inputView);
                }
            } else {
                inputMethod.hideSoftInputFromWindow(inputView.getWindowToken(), 0);
                inputView.clearFocus();
                inputView.setFocusable(false);
                inputView.setFocusableInTouchMode(false);
            }
        });
    }

    /** The insets listener, or the height poll, says the keyboard came or went. */
    void onImeVisibility(boolean visible) {
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
        nativeSide.keyboardHidden();
    }

    /** The input method spelled something out: replace what it had composed. */
    private void onComposing(String text) {
        if (!composing.isEmpty()) {
            nativeSide.backspace(composing.length());
        }
        composing = text;
        if (!text.isEmpty()) {
            nativeSide.commit(text);
        }
    }

    /** The input method settled on something, and it stays. */
    private void onCommit(String text) {
        if (!composing.isEmpty()) {
            final String previous = composing;
            composing = "";
            if (text.contentEquals(previous)) {
                // Already in the field, put there by `onComposing`. Sending a
                // backspace and the text again would move the cursor back and
                // retype it - wrong when a selection or a hardware key landed
                // in between, and wrong by one for a surrogate pair.
                return;
            }
            nativeSide.backspace(previous.length());
        }
        if (!text.isEmpty()) {
            nativeSide.commit(text);
        }
    }

    /** The input method's height in pixels, 0 when hidden, -1 when it will not say. */
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
}
