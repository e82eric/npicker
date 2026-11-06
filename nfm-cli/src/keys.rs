//! Key chords such as `ctrl-o`, `alt+enter` or `ctrl-shift-k`.
use nfm_egui::egui::{self, Key, Modifiers};

#[derive(Clone, Debug)]
pub struct Chord {
    pub modifiers: Modifiers,
    pub key: Key,
    /// Canonical text, printed by `--expect`.
    pub name: String,
}
impl PartialEq for Chord {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}
impl Chord {
    fn modifier_count(&self) -> usize {
        [self.modifiers.ctrl, self.modifiers.alt, self.modifiers.shift]
            .into_iter()
            .filter(|&m| m)
            .count()
    }
    /// Consume the chord from this frame's input. Extra modifiers do not match,
    /// so `ctrl-k` and `ctrl-shift-k` are distinct.
    pub fn consume(&self, ctx: &egui::Context) -> bool {
        ctx.input_mut(|input| {
            let position = input.events.iter().position(|event| {
                matches!(
                    event,
                    egui::Event::Key { key, pressed: true, modifiers, .. }
                        if *key == self.key
                            && modifiers.ctrl == self.modifiers.ctrl
                            && modifiers.alt == self.modifiers.alt
                            && modifiers.shift == self.modifiers.shift
                )
            });
            let Some(position) = position else {
                return false;
            };
            input.events.remove(position);
            // A printable key is followed by its Text event (egui-winit queues
            // Text right after Key); drop it so the bound key is not typed into
            // the query. Ctrl and Alt chords produce no text.
            if !self.modifiers.ctrl
                && !self.modifiers.alt
                && matches!(input.events.get(position), Some(egui::Event::Text(_)))
            {
                input.events.remove(position);
            }
            true
        })
    }
    /// Help-overlay text, e.g. `Ctrl+Shift+K`.
    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        if self.modifiers.ctrl {
            parts.push("Ctrl".to_owned());
        }
        if self.modifiers.alt {
            parts.push("Alt".to_owned());
        }
        if self.modifiers.shift {
            parts.push("Shift".to_owned());
        }
        parts.push(self.key.name().to_owned());
        parts.join("+")
    }
}

/// Most specific chords first, so a binding with more modifiers wins.
pub fn sort_specific_first<T>(chords: &mut [(Chord, T)]) {
    chords.sort_by_key(|(chord, _)| std::cmp::Reverse(chord.modifier_count()));
}

pub fn parse(text: &str) -> Result<Chord, String> {
    if text.is_empty() {
        return Err("empty key".into());
    }
    // `page-up` and `page-down` contain a separator; join them first so they
    // are not read as a `page` modifier.
    let original = text;
    let lower = text.to_ascii_lowercase();
    let joined = ["page-up", "page-down"]
        .into_iter()
        .find(|alias| lower.ends_with(alias))
        .map(|alias| {
            let start = text.len() - alias.len();
            format!("{}{}", &text[..start], alias.replace('-', ""))
        });
    let text = joined.as_deref().unwrap_or(text);
    // The key is after the last separator, unless the key itself is `-` or `+`.
    let split = text
        .char_indices()
        .rev()
        .skip(1)
        .find(|&(_, c)| c == '-' || c == '+')
        .map(|(i, _)| i);
    let (modifier_text, key_text) = match split {
        Some(i) => (&text[..i], &text[i + 1..]),
        None => ("", text),
    };
    let mut modifiers = Modifiers::NONE;
    for part in modifier_text
        .split(['-', '+'])
        .filter(|part| !part.is_empty())
    {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "c" => modifiers.ctrl = true,
            "alt" | "a" | "m" | "meta" => modifiers.alt = true,
            "shift" | "s" => modifiers.shift = true,
            other => return Err(format!("unknown modifier '{other}' in '{original}'")),
        }
    }
    modifiers.command = modifiers.ctrl;
    let key = parse_key(key_text).ok_or_else(|| format!("unknown key '{key_text}' in '{original}'"))?;
    let mut name = Vec::new();
    if modifiers.ctrl {
        name.push("ctrl".to_owned());
    }
    if modifiers.alt {
        name.push("alt".to_owned());
    }
    if modifiers.shift {
        name.push("shift".to_owned());
    }
    name.push(key.name().to_ascii_lowercase());
    Ok(Chord {
        modifiers,
        key,
        name: name.join("-"),
    })
}

fn parse_key(text: &str) -> Option<Key> {
    let lower = text.to_ascii_lowercase();
    let alias = match lower.as_str() {
        "enter" | "return" | "cr" => "Enter",
        "esc" | "escape" => "Escape",
        "tab" => "Tab",
        "space" => "Space",
        "bs" | "backspace" | "bspace" => "Backspace",
        "del" | "delete" => "Delete",
        "ins" | "insert" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pgup" | "pageup" | "page-up" => "PageUp",
        "pgdn" | "pagedown" | "page-down" => "PageDown",
        "up" => "ArrowUp",
        "down" => "ArrowDown",
        "left" => "ArrowLeft",
        "right" => "ArrowRight",
        _ => "",
    };
    if !alias.is_empty() {
        return Key::from_name(alias);
    }
    if let Some(number) = lower.strip_prefix('f')
        && number.parse::<u8>().is_ok()
    {
        return Key::from_name(&format!("F{number}"));
    }
    if text.chars().count() == 1 {
        return Key::from_name(&lower);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chords() {
        let chord = parse("ctrl-o").unwrap();
        assert!(chord.modifiers.ctrl && !chord.modifiers.shift);
        assert_eq!(chord.key, Key::O);
        assert_eq!(chord.name, "ctrl-o");
        assert_eq!(parse("Ctrl+Shift+K").unwrap().name, "ctrl-shift-k");
        assert_eq!(parse("alt-enter").unwrap().key, Key::Enter);
        assert_eq!(parse("f5").unwrap().key, Key::F5);
        assert_eq!(parse("ctrl--").unwrap().key, Key::Minus);
        assert_eq!(parse("ctrl-/").unwrap().key, Key::Slash);
        assert_eq!(parse("enter").unwrap().name, "enter");
        assert_eq!(parse("pgdn").unwrap().key, Key::PageDown);
        assert_eq!(parse("page-down").unwrap().key, Key::PageDown);
        assert_eq!(parse("Page-Up").unwrap().key, Key::PageUp);
        let chord = parse("ctrl-page-up").unwrap();
        assert!(chord.modifiers.ctrl);
        assert_eq!(chord.key, Key::PageUp);
        assert!(parse("hyper-x").is_err());
        assert!(parse("ctrl-nope").is_err());
        assert!(parse("").is_err());
        assert_eq!(parse("ctrl-o").unwrap(), parse("control+O").unwrap());
    }

    fn press(key: Key, modifiers: Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    fn consume(chord: &str, events: Vec<egui::Event>) -> (bool, Vec<egui::Event>) {
        let ctx = egui::Context::default();
        ctx.begin_pass(egui::RawInput {
            events,
            ..Default::default()
        });
        let consumed = parse(chord).unwrap().consume(&ctx);
        let remaining = ctx.input(|input| input.events.clone());
        // The font atlas upload is never applied in a test; clear it.
        ctx.end_pass().textures_delta.clear();
        (consumed, remaining)
    }

    #[test]
    fn consuming_a_printable_key_drops_its_text() {
        let (consumed, remaining) = consume(
            "space",
            vec![press(Key::Space, Modifiers::NONE), egui::Event::Text(" ".into())],
        );
        assert!(consumed);
        assert!(remaining.is_empty());
        // Other typed text in the same frame is kept.
        let (_, remaining) = consume(
            "space",
            vec![
                egui::Event::Text("a".into()),
                press(Key::A, Modifiers::NONE),
                press(Key::Space, Modifiers::NONE),
                egui::Event::Text(" ".into()),
            ],
        );
        assert_eq!(remaining.len(), 2);
        assert!(matches!(&remaining[0], egui::Event::Text(t) if t == "a"));
    }

    #[test]
    fn specific_chords_sort_first() {
        let mut chords = vec![(parse("ctrl-k").unwrap(), 1), (parse("ctrl-shift-k").unwrap(), 2)];
        sort_specific_first(&mut chords);
        assert_eq!(chords[0].1, 2);
    }
}
