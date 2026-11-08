//! Icon references: the text an icon slot's field holds (see
//! [`SlotKind::Icon`](crate::row::SlotKind::Icon)).
//!
//! | Reference | Meaning |
//! |---|---|
//! | `glyph:TEXT` or `glyph:TEXT@COLOR` | a glyph (such as a Nerd Font symbol) drawn with the icon font |
//! | a value of at most two characters with no `:` | a glyph in the default color |
//! | `SCHEME:REST` | anything else; only resolvers the host installs can draw it |
//! | empty | no icon |
//!
//! A color is `#rrggbb`; a glyph without one uses the theme's text color.
//!
//! Glyphs need no resolver and touch nothing outside the picker. A
//! host decides which other schemes exist by installing resolvers, so data can
//! never reach a resolver the host did not choose.

use std::fmt;

/// An icon color, `#rrggbb` in references.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct IconColor(pub [u8; 3]);

impl IconColor {
    pub fn parse(text: &str) -> Option<Self> {
        let hex = text.trim().strip_prefix('#')?;
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        Some(Self([channel(0)?, channel(2)?, channel(4)?]))
    }
}

impl fmt::Display for IconColor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [r, g, b] = self.0;
        write!(f, "#{r:02x}{g:02x}{b:02x}")
    }
}

/// A parsed icon reference.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IconRef<'a> {
    None,
    Glyph {
        text: &'a str,
        color: Option<IconColor>,
    },
    /// A scheme for a host resolver, such as `app:running`.
    Other(&'a str),
}

impl<'a> IconRef<'a> {
    pub fn parse(reference: &'a str) -> Self {
        let reference = reference.trim();
        if reference.is_empty() {
            return Self::None;
        }
        if let Some(rest) = reference.strip_prefix("glyph:") {
            // The color follows the last `@`, so a glyph may itself be `@`.
            let (text, color) = match rest.rsplit_once('@') {
                Some((text, color)) if !text.is_empty() => match IconColor::parse(color) {
                    Some(color) => (text, Some(color)),
                    None => (rest, None),
                },
                _ => (rest, None),
            };
            return if text.is_empty() {
                Self::None
            } else {
                Self::Glyph { text, color }
            };
        }
        if !reference.contains(':') && reference.chars().count() <= 2 {
            return Self::Glyph {
                text: reference,
                color: None,
            };
        }
        Self::Other(reference)
    }

    /// The reference for a glyph, for sources that build references.
    pub fn glyph(text: &str, color: Option<IconColor>) -> String {
        match color {
            Some(color) => format!("glyph:{text}@{color}"),
            None => format!("glyph:{text}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_glyphs_and_host_schemes() {
        assert_eq!(IconRef::parse(""), IconRef::None);
        assert_eq!(
            IconRef::parse("glyph:\u{e68b}@#dea584"),
            IconRef::Glyph {
                text: "\u{e68b}",
                color: Some(IconColor([0xde, 0xa5, 0x84]))
            }
        );
        assert_eq!(
            IconRef::parse("glyph:@"),
            IconRef::Glyph {
                text: "@",
                color: None
            }
        );
        assert_eq!(
            IconRef::parse("glyph:x@nope"),
            IconRef::Glyph {
                text: "x@nope",
                color: None
            }
        );
        // Color names are not colors: the whole text is the glyph.
        assert_eq!(
            IconRef::parse("glyph:x@success"),
            IconRef::Glyph {
                text: "x@success",
                color: None
            }
        );
        assert_eq!(
            IconRef::parse("★"),
            IconRef::Glyph {
                text: "★",
                color: None
            }
        );
        assert_eq!(IconRef::parse("app:running"), IconRef::Other("app:running"));
        // Longer bare text is not a glyph: it is left for host resolvers.
        assert_eq!(IconRef::parse("running"), IconRef::Other("running"));
    }

    #[test]
    fn builders_round_trip() {
        let color = IconColor([1, 2, 255]);
        let glyph = IconRef::glyph("x", Some(color));
        assert_eq!(glyph, "glyph:x@#0102ff");
        assert_eq!(
            IconRef::parse(&glyph),
            IconRef::Glyph {
                text: "x",
                color: Some(color)
            }
        );
    }
}
