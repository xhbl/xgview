//! Android's key codes, translated once.
//!
//! A remote and a keyboard both arrive as `KeyEvent` codes, and the interface
//! wants egui's keys and text. The mapping lives here rather than beside the
//! loop that uses it because it is the one part of that loop with no device in
//! it: it is a table and a few rules, and both can be tested on any machine.

/// The ten digits, shared by the row above the letters and the numpad.
const DIGITS: [egui::Key; 10] = [
    egui::Key::Num0,
    egui::Key::Num1,
    egui::Key::Num2,
    egui::Key::Num3,
    egui::Key::Num4,
    egui::Key::Num5,
    egui::Key::Num6,
    egui::Key::Num7,
    egui::Key::Num8,
    egui::Key::Num9,
];

/// The function keys of a hardware keyboard.
const FUNCTION_KEYS: [egui::Key; 12] = [
    egui::Key::F1,
    egui::Key::F2,
    egui::Key::F3,
    egui::Key::F4,
    egui::Key::F5,
    egui::Key::F6,
    egui::Key::F7,
    egui::Key::F8,
    egui::Key::F9,
    egui::Key::F10,
    egui::Key::F11,
    egui::Key::F12,
];

/// Android's `KEYCODE_*` to egui's key, for the ones a remote or a keyboard
/// sends. `None` for everything else - media keys, volume, the language switch.
///
/// `None` is not the same as "ignore": the caller hands an unclaimed key back to
/// the system, which is what keeps the volume buttons working on a remote. A
/// key with no name here can still be text - a punctuation key, for one - and
/// the caller decides that from the key's Unicode character, not from this
/// table.
pub fn from_android(code: i32) -> Option<egui::Key> {
    use egui::Key;
    Some(match code {
        // KeyEvent.KEYCODE_A ..= KEYCODE_Z
        29..=54 => match code - 29 {
            0 => Key::A,
            1 => Key::B,
            2 => Key::C,
            3 => Key::D,
            4 => Key::E,
            5 => Key::F,
            6 => Key::G,
            7 => Key::H,
            8 => Key::I,
            9 => Key::J,
            10 => Key::K,
            11 => Key::L,
            12 => Key::M,
            13 => Key::N,
            14 => Key::O,
            15 => Key::P,
            16 => Key::Q,
            17 => Key::R,
            18 => Key::S,
            19 => Key::T,
            20 => Key::U,
            21 => Key::V,
            22 => Key::W,
            23 => Key::X,
            24 => Key::Y,
            _ => Key::Z,
        },
        // KEYCODE_0 ..= KEYCODE_9, and KEYCODE_NUMPAD_0 ..= KEYCODE_NUMPAD_9.
        // Both rows are the same ten controls to the interface.
        7..=16 => DIGITS[(code - 7) as usize],
        144..=153 => DIGITS[(code - 144) as usize],
        19 => Key::ArrowUp,
        20 => Key::ArrowDown,
        21 => Key::ArrowLeft,
        22 => Key::ArrowRight,
        // A remote's centre button and a keyboard's enter are the same control.
        23 | 66 => Key::Enter,
        // Back, and a keyboard's escape: the interface unwinds a layer on both.
        4 | 111 => Key::Escape,
        62 => Key::Space,
        61 => Key::Tab,
        67 => Key::Backspace,
        112 => Key::Delete,
        122 => Key::Home,
        123 => Key::End,
        92 => Key::PageUp,
        93 => Key::PageDown,
        // KEYCODE_INSERT: a hardware keyboard's key, not a remote's.
        124 => Key::Insert,
        // KEYCODE_F1 ..= KEYCODE_F12. Settings, devices and full screen are on
        // F1, F2 and F11; the old eframe/winit path mapped the whole range.
        131..=142 => FUNCTION_KEYS[(code - 131) as usize],
        // KEYCODE_NUMPAD_ENTER is a keyboard's Enter too.
        160 => Key::Enter,
        _ => return None,
    })
}

/// The egui events one Android key becomes, or `None` when the interface owns
/// none of them.
///
/// `meta` is `KeyEvent.getMetaState()` and `unicode` is
/// `KeyEvent.getUnicodeChar()`: the character the device's key map made of the
/// key with those modifiers held, so a Shift, a non-Latin layout and a numpad
/// arrive already resolved.
///
/// A physical key is often two things at once: a control (arrows, Enter, the
/// function keys) and, when it prints a character, text. eframe's winit path
/// sent both on Android; the surfaces path used to send only the control,
/// which is why a hardware keyboard could walk the form but never fill in a
/// field. The rules for the text half are egui-winit's own: only on the press,
/// only printable characters, and never for a Ctrl/Command combination -
/// `Ctrl+A` selects the field, it does not type U+0001 into it.
///
/// A key that prints a character but has no name in [`from_android`] - a comma,
/// an @ - is still the interface's: it is text, even though nothing reads it
/// as a control. Both halves count as "taken" on the release too, so the
/// activity answers a key's down and up the same way.
pub fn key_events(
    code: i32,
    pressed: bool,
    repeat: bool,
    meta: i32,
    unicode: i32,
) -> Option<Vec<egui::Event>> {
    // `KeyEvent.META_*_ON`, plus the left/right variants: a device reports
    // either the aggregate bit or the side it was pressed on.
    const SHIFT: i32 = 0x0001 | 0x0040 | 0x0080;
    const ALT: i32 = 0x0002 | 0x0010 | 0x0020;
    const CTRL: i32 = 0x1000 | 0x2000 | 0x4000;

    let printed = printable_char(unicode);
    let key = from_android(code);
    if key.is_none() && printed.is_none() {
        return None;
    }
    let ctrl = meta & CTRL != 0;
    let mut events = Vec::with_capacity(2);
    if let Some(key) = key {
        events.push(egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat,
            modifiers: egui::Modifiers {
                alt: meta & ALT != 0,
                ctrl,
                shift: meta & SHIFT != 0,
                mac_cmd: false,
                command: ctrl,
            },
        });
    }
    if pressed && !ctrl {
        if let Some(printed) = printed {
            events.push(egui::Event::Text(printed.to_string()));
        }
    }
    Some(events)
}

/// The character a key types, if it types one.
///
/// Enter's `\n`, Tab's `\t` and Backspace's `\b` are controls, not text; a
/// field reads those from the `Key` event instead. The private-use ranges are
/// a platform's own keys. Both are the checks egui-winit applies before it
/// makes an `Event::Text`.
fn printable_char(unicode: i32) -> Option<char> {
    let printed = char::from_u32(unicode as u32)?;
    let private_use = ('\u{e000}'..='\u{f8ff}').contains(&printed)
        || ('\u{f0000}'..='\u{ffffd}').contains(&printed)
        || ('\u{100000}'..='\u{10fffd}').contains(&printed);
    (!private_use && !printed.is_ascii_control()).then_some(printed)
}

#[cfg(test)]
mod tests {
    use super::{from_android, key_events, printable_char, DIGITS};

    const CTRL: i32 = 0x1000;
    const SHIFT: i32 = 0x0001;

    /// The named half of a key, which is what the shortcuts read.
    fn keys_of(code: i32, pressed: bool, meta: i32, unicode: i32) -> Vec<egui::Key> {
        key_events(code, pressed, false, meta, unicode)
            .unwrap_or_default()
            .iter()
            .filter_map(|event| match event {
                egui::Event::Key { key, .. } => Some(*key),
                _ => None,
            })
            .collect()
    }

    /// The text half of a key, which is what a field reads.
    fn texts_of(code: i32, pressed: bool, meta: i32, unicode: i32) -> Vec<String> {
        key_events(code, pressed, false, meta, unicode)
            .unwrap_or_default()
            .iter()
            .filter_map(|event| match event {
                egui::Event::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn taken(code: i32, pressed: bool, meta: i32, unicode: i32) -> bool {
        key_events(code, pressed, false, meta, unicode).is_some()
    }

    /// The keys the interface reads, and the ones it must not.
    ///
    /// The second list is the point: the caller answers the key event with
    /// whether the interface took it, and an unclaimed key goes back to the
    /// system. A code mapped here by accident - volume sits one `if` away from
    /// the letter range - would stop working on the remote, with nothing in the
    /// log to say why.
    #[test]
    fn the_keys_that_are_not_the_interface_s_are_left_alone() {
        // The remote, the letters a keyboard types with, and the function keys
        // that open the panels.
        for code in [
            19, 20, 21, 22, 23, 4, 66, 111, 29, 30, 54, 7, 16, 62, 67, 131, 132, 141, 124,
        ] {
            assert!(
                from_android(code).is_some(),
                "keycode {code} is the interface's"
            );
        }
        // Volume, mute, the media keys: the box's, not the wall's.
        for code in [24, 25, 164, 85, 87, 88, 126, 127, 219] {
            assert!(
                from_android(code).is_none(),
                "keycode {code} belongs to the box"
            );
        }
    }

    /// Every key that comes back is a distinct one: two Android codes landing on
    /// the same egui key would make one of them unpressable.
    #[test]
    fn no_two_codes_share_a_key_by_accident() {
        use egui::Key;
        let mut seen: Vec<(Key, i32)> = Vec::new();
        for code in 0..=255 {
            let Some(key) = from_android(code) else {
                continue;
            };
            // Enter and Escape are the two that are meant to be shared: a
            // remote's centre button with a keyboard's enter, Back with escape.
            // The digits have two codes each - the top row and the numpad - but
            // both are meant to be the same control.
            if let Some((_, other)) = seen.iter().find(|(k, _)| *k == key) {
                let shared = matches!(key, Key::Enter | Key::Escape) || DIGITS.contains(&key);
                assert!(
                    shared,
                    "keycode {code} and {other} both became {key:?}"
                );
            } else {
                seen.push((key, code));
            }
        }
    }

    /// The shortcuts the application actually reads, and the numpad beside
    /// them. A missing F1/F2/F11 was a settings panel a keyboard could not open.
    #[test]
    fn a_hardware_keyboard_keeps_its_shortcuts() {
        use egui::Key;
        assert_eq!(from_android(131), Some(Key::F1));
        assert_eq!(from_android(132), Some(Key::F2));
        assert_eq!(from_android(141), Some(Key::F11));
        assert_eq!(from_android(142), Some(Key::F12));
        assert_eq!(from_android(124), Some(Key::Insert));
        // The numpad is the same ten digits, and its Enter is Enter.
        for digit in 0..=9 {
            assert_eq!(from_android(7 + digit), from_android(144 + digit));
        }
        assert_eq!(from_android(160), from_android(66));
    }

    #[test]
    fn a_letter_is_both_a_control_and_text() {
        // KEYCODE_A, as the device's key map spells it.
        assert_eq!(keys_of(29, true, 0, 'a' as i32), vec![egui::Key::A]);
        assert_eq!(texts_of(29, true, 0, 'a' as i32), vec!["a".to_owned()]);
        assert!(taken(29, true, 0, 'a' as i32));
    }

    #[test]
    fn a_release_types_nothing_but_is_still_taken() {
        assert_eq!(keys_of(29, false, 0, 'a' as i32), vec![egui::Key::A]);
        assert!(texts_of(29, false, 0, 'a' as i32).is_empty());
        // Down and up must answer the same, or the system sees half a key.
        assert!(taken(29, false, 0, 'a' as i32));
    }

    #[test]
    fn punctuation_is_text_even_without_a_key_name() {
        // KEYCODE_COMMA has no egui key. An address is made of these.
        assert!(keys_of(55, true, 0, ',' as i32).is_empty());
        assert_eq!(texts_of(55, true, 0, ',' as i32), vec![",".to_owned()]);
        assert!(taken(55, true, 0, ',' as i32));
        // Its release is taken too, even though it sends nothing.
        assert!(taken(55, false, 0, ',' as i32));
    }

    #[test]
    fn enter_tab_and_backspace_are_keys_not_text() {
        for (code, unicode) in [(66, 10), (61, 9), (67, 8), (23, 10)] {
            assert!(taken(code, true, 0, unicode), "keycode {code}");
            assert!(
                texts_of(code, true, 0, unicode).is_empty(),
                "keycode {code} typed a control character"
            );
        }
    }

    #[test]
    fn a_space_is_text_because_a_field_can_hold_one() {
        assert_eq!(keys_of(62, true, 0, 32), vec![egui::Key::Space]);
        assert_eq!(texts_of(62, true, 0, 32), vec![" ".to_owned()]);
    }

    #[test]
    fn ctrl_combinations_do_not_type_control_characters() {
        // Ctrl+A selects the field; it does not put U+0001 in it.
        assert_eq!(keys_of(29, true, CTRL, 1), vec![egui::Key::A]);
        assert!(texts_of(29, true, CTRL, 1).is_empty());
        // A device that reports only the left-hand Ctrl bit means the same.
        assert!(texts_of(29, true, 0x2000, 1).is_empty());
        // Shift is not a command: a shifted A is a capital A.
        assert_eq!(texts_of(29, true, SHIFT, 'A' as i32), vec!["A".to_owned()]);
    }

    #[test]
    fn remote_keys_are_left_to_the_system() {
        // Volume, mute and the media keys: no name, no character.
        for code in [24, 25, 164, 85, 87, 88] {
            assert!(!taken(code, true, 0, 0), "keycode {code}");
        }
        // A key map's private-use, combining-accent and empty values are not
        // text; a character from another script is.
        assert_eq!(printable_char(0), None);
        assert_eq!(printable_char(0xe000), None);
        assert_eq!(printable_char(i32::MIN), None);
        assert_eq!(printable_char('\u{4e2d}' as i32), Some('\u{4e2d}'));
    }

    #[test]
    fn function_keys_and_the_numpad_are_controls() {
        for (code, key) in [
            (131, egui::Key::F1),
            (132, egui::Key::F2),
            (141, egui::Key::F11),
            (142, egui::Key::F12),
            (124, egui::Key::Insert),
            (160, egui::Key::Enter),
        ] {
            assert_eq!(keys_of(code, true, 0, 0), vec![key], "keycode {code}");
            assert!(texts_of(code, true, 0, 0).is_empty(), "keycode {code}");
        }
        // A numpad digit is a control and a character at once.
        assert_eq!(keys_of(144, true, 0, '0' as i32), vec![egui::Key::Num0]);
        assert_eq!(texts_of(144, true, 0, '0' as i32), vec!["0".to_owned()]);
    }
}
