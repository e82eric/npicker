#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct KeyModifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum KeyName {
    Character(char),
    Enter,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Backspace,
    Delete,
    PageUp,
    PageDown,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct KeyChord {
    pub key: KeyName,
    pub modifiers: KeyModifiers,
}

pub fn parse_key_chord(value: &str) -> Option<KeyChord> {
    let mut modifiers = KeyModifiers::default();
    let mut key = None;
    for part in value
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers.ctrl = true,
            "alt" => modifiers.alt = true,
            "shift" => modifiers.shift = true,
            value if key.is_none() => key = parse_key_name(value),
            _ => return None,
        }
    }
    Some(KeyChord {
        key: key?,
        modifiers,
    })
}

fn parse_key_name(value: &str) -> Option<KeyName> {
    match value {
        "arrowup" | "up" => Some(KeyName::Up),
        "arrowdown" | "down" => Some(KeyName::Down),
        "arrowleft" | "left" => Some(KeyName::Left),
        "arrowright" | "right" => Some(KeyName::Right),
        "pgup" | "pageup" => Some(KeyName::PageUp),
        "pgdown" | "pagedown" => Some(KeyName::PageDown),
        "esc" | "escape" => Some(KeyName::Escape),
        "return" | "enter" => Some(KeyName::Enter),
        "backspace" => Some(KeyName::Backspace),
        "delete" => Some(KeyName::Delete),
        "home" => Some(KeyName::Home),
        "end" => Some(KeyName::End),
        value if value.chars().count() == 1 => Some(KeyName::Character(
            value.chars().next()?.to_ascii_lowercase(),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifier_order_independently() {
        assert_eq!(
            parse_key_chord("shift+ctrl+P"),
            Some(KeyChord {
                key: KeyName::Character('p'),
                modifiers: KeyModifiers {
                    ctrl: true,
                    shift: true,
                    alt: false,
                },
            })
        );
    }

    #[test]
    fn normalizes_named_key_aliases() {
        assert_eq!(
            parse_key_chord("Alt+ArrowUp"),
            Some(KeyChord {
                key: KeyName::Up,
                modifiers: KeyModifiers {
                    alt: true,
                    ..KeyModifiers::default()
                },
            })
        );
        assert_eq!(parse_key_chord("ctrl+unknown"), None);
    }
}
