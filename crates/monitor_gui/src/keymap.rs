//! Android's key codes, translated once.
//!
//! A remote and a keyboard both arrive as `KeyEvent` codes, and the interface
//! wants egui's keys. The mapping lives here rather than beside the loop that
//! uses it because it is the one part of that loop with no device in it: it is
//! a table, and a table can be tested on any machine.

/// Android's `KEYCODE_*` to egui's key, for the ones a remote or a keyboard
/// sends. `None` for everything else - media keys, volume, the language switch.
///
/// `None` is not the same as "ignore": the caller hands an unclaimed key back to
/// the system, which is what keeps the volume buttons working on a remote.
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
        // KEYCODE_0 ..= KEYCODE_9
        7..=16 => match code - 7 {
            0 => Key::Num0,
            1 => Key::Num1,
            2 => Key::Num2,
            3 => Key::Num3,
            4 => Key::Num4,
            5 => Key::Num5,
            6 => Key::Num6,
            7 => Key::Num7,
            8 => Key::Num8,
            _ => Key::Num9,
        },
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
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::from_android;

    /// The keys the interface reads, and the ones it must not.
    ///
    /// The second list is the point: the caller answers the key event with
    /// whether the interface took it, and an unclaimed key goes back to the
    /// system. A code mapped here by accident - volume sits one `if` away from
    /// the letter range - would stop working on the remote, with nothing in the
    /// log to say why.
    #[test]
    fn the_keys_that_are_not_the_interface_s_are_left_alone() {
        // The remote, and the letters a keyboard types with.
        for code in [19, 20, 21, 22, 23, 4, 66, 111, 29, 30, 54, 7, 16, 62, 67] {
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
            if let Some((_, other)) = seen.iter().find(|(k, _)| *k == key) {
                assert!(
                    matches!(key, Key::Enter | Key::Escape),
                    "keycode {code} and {other} both became {key:?}"
                );
            } else {
                seen.push((key, code));
            }
        }
    }
}
