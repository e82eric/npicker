//! Multi-line result rows ("cards").
//!
//! A source that returns a [`RowTemplate`] from
//! [`ItemsSource::row_template`](crate::store::ItemsSource::row_template) has
//! its results painted as cards: every row has the same lines, each line holds
//! slots that show one field, and an optional leading icon sits at the left.
//! Icon slots hold icon references (see [`crate::icon`]). Field values come from
//! [`ItemsSource::row_fields`](crate::store::ItemsSource::row_fields), indexed
//! by the slot's `field`, already formatted for display.
//!
//! Pickers only paint text. How a value becomes that text (a date, a duration,
//! an icon for a path) is up to the source: a slot may carry a [`RowSlot::format`]
//! spec, which the core does not interpret.
//!
//! The template is fixed for a source, so every card has the same height.

use std::fmt;

/// How a slot's text is painted.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RowStyle {
    /// Normal text.
    #[default]
    Normal,
    /// Larger, prominent text, such as a title.
    Title,
    /// Dimmed text, such as a subtitle, project or time.
    Muted,
    /// The accent color.
    Accent,
}

/// Where a slot sits on its line.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RowAlign {
    /// Left slots share the space the right slots leave, in order.
    #[default]
    Left,
    /// Right slots are measured first and keep their full width when they can.
    Right,
}

/// A source-defined slot format, such as `devicon` or `date=%Y-%m-%d`: a name
/// and an optional argument. The core keeps it for the source; it does not
/// interpret it, and a source may accept only some formats on some slots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RowFormat {
    pub name: String,
    pub argument: Option<String>,
}

impl RowFormat {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            argument: None,
        }
    }
    pub fn with_argument(name: impl Into<String>, argument: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            argument: Some(argument.into()),
        }
    }
}

impl fmt::Display for RowFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.argument {
            Some(argument) => write!(f, "{}={argument}", self.name),
            None => f.write_str(&self.name),
        }
    }
}

/// What a slot shows.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SlotKind {
    /// The field's text.
    #[default]
    Text,
    /// An icon: the field holds an icon reference (see [`crate::icon`]).
    Icon,
}

/// One field on a card line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RowSlot {
    /// Index into [`RowFields::values`].
    pub field: usize,
    pub kind: SlotKind,
    pub style: RowStyle,
    pub align: RowAlign,
    /// Whether query matches are highlighted in this slot.
    pub highlight: bool,
    /// How the source formats the value, if it is not shown as it is.
    pub format: Option<RowFormat>,
}

impl RowSlot {
    pub fn new(field: usize) -> Self {
        Self {
            field,
            kind: SlotKind::Text,
            style: RowStyle::Normal,
            align: RowAlign::Left,
            highlight: true,
            format: None,
        }
    }
    /// An icon slot is never highlighted.
    pub fn icon(mut self) -> Self {
        self.kind = SlotKind::Icon;
        self.highlight = false;
        self
    }
    pub fn style(mut self, style: RowStyle) -> Self {
        self.style = style;
        self
    }
    pub fn right(mut self) -> Self {
        self.align = RowAlign::Right;
        self
    }
    pub fn format(mut self, format: RowFormat) -> Self {
        self.format = Some(format);
        self
    }
    pub fn highlight(mut self, highlight: bool) -> Self {
        self.highlight = highlight && self.kind == SlotKind::Text;
        self
    }
}

/// One line of a card.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RowLine {
    pub slots: Vec<RowSlot>,
}

/// The layout of every card in a source.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RowTemplate {
    pub lines: Vec<RowLine>,
    /// An icon in a column at the card's left, beside its title line (see
    /// [`RowTemplate::title_line`]). Every card keeps the column, so text
    /// stays aligned when a card has no icon.
    pub leading: Option<RowSlot>,
}

impl RowTemplate {
    /// Every slot, line by line, then the leading icon.
    pub fn slots(&self) -> impl Iterator<Item = &RowSlot> {
        self.lines
            .iter()
            .flat_map(|line| &line.slots)
            .chain(&self.leading)
    }

    pub fn slots_mut(&mut self) -> impl Iterator<Item = &mut RowSlot> {
        self.lines
            .iter_mut()
            .flat_map(|line| &mut line.slots)
            .chain(&mut self.leading)
    }

    /// The highest field index the template reads.
    pub fn max_field(&self) -> Option<usize> {
        self.slots().map(|slot| slot.field).max()
    }

    /// The line the leading icon sits beside: the first line with a `Title`
    /// slot, else the first line.
    pub fn title_line(&self) -> usize {
        self.lines
            .iter()
            .position(|line| line.slots.iter().any(|slot| slot.style == RowStyle::Title))
            .unwrap_or(0)
    }

    /// Parse a compact template over named fields.
    ///
    /// Lines are separated by `;` and slots by `|`. A slot is a field name
    /// with optional `:` modifiers: `title`, `muted`, `accent`, `normal`,
    /// `right`, `left`, `nohl` (no match highlighting) and `icon` (the field
    /// holds an icon reference, see [`crate::icon`]). Any other modifier is
    /// the slot's [`RowFormat`], for the source to interpret: `name` or
    /// `name=argument`. An argument runs to the end of the slot, so it may
    /// contain `:` (`updated:muted:date=%H:%M`), and only one format is
    /// allowed per slot.
    ///
    /// `leading` is one slot in the same syntax for the leading icon, such as
    /// `path:devicon`; it is always an icon.
    pub fn parse(
        spec: &str,
        leading: Option<&str>,
        mut field_index: impl FnMut(&str) -> Option<usize>,
    ) -> Result<Self, RowTemplateError> {
        let mut lines = Vec::new();
        for line in spec.split(';') {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let slots = line
                .split('|')
                .map(|text| parse_slot(text, &mut field_index))
                .collect::<Result<_, _>>()?;
            lines.push(RowLine { slots });
        }
        if lines.is_empty() {
            return Err(RowTemplateError("row template has no lines".into()));
        }
        let leading = leading
            .map(|text| parse_slot(text, &mut field_index).map(RowSlot::icon))
            .transpose()?;
        Ok(Self { lines, leading })
    }
}

fn parse_slot(
    text: &str,
    field_index: &mut impl FnMut(&str) -> Option<usize>,
) -> Result<RowSlot, RowTemplateError> {
    let (name, mut rest) = text.split_once(':').unwrap_or((text, ""));
    let name = name.trim();
    if name.is_empty() {
        return Err(RowTemplateError(format!("empty slot '{}'", text.trim())));
    }
    let field =
        field_index(name).ok_or_else(|| RowTemplateError(format!("unknown field '{name}'")))?;
    let mut slot = RowSlot::new(field);
    while !rest.is_empty() {
        let (modifier, next) = match rest.split_once(':') {
            // `name=argument` takes the rest of the slot.
            Some((modifier, _)) if modifier.contains('=') => (rest, ""),
            Some((modifier, next)) => (modifier, next),
            None => (rest, ""),
        };
        rest = next;
        let modifier = modifier.trim();
        slot = match modifier.to_ascii_lowercase().as_str() {
            "" => slot,
            "title" => slot.style(RowStyle::Title),
            "muted" => slot.style(RowStyle::Muted),
            "accent" => slot.style(RowStyle::Accent),
            "normal" => slot.style(RowStyle::Normal),
            "right" => slot.right(),
            "left" => {
                slot.align = RowAlign::Left;
                slot
            }
            "nohl" => slot.highlight(false),
            "icon" => slot.icon(),
            _ => {
                if let Some(existing) = &slot.format {
                    return Err(RowTemplateError(format!(
                        "field '{name}' has two formats: '{existing}' and '{modifier}'"
                    )));
                }
                let format = match modifier.split_once('=') {
                    Some((format, argument)) => {
                        RowFormat::with_argument(format.trim().to_ascii_lowercase(), argument)
                    }
                    None => RowFormat::new(modifier.to_ascii_lowercase()),
                };
                slot.format(format)
            }
        };
    }
    Ok(slot)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RowTemplateError(pub String);

impl fmt::Display for RowTemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RowTemplateError {}

/// Field values for one card, indexed by [`RowSlot::field`]. Values are
/// already formatted for display; icon slots hold icon references.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RowFields {
    pub values: Vec<String>,
}

impl RowFields {
    pub fn get(&self, field: usize) -> &str {
        self.values.get(field).map(String::as_str).unwrap_or("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_compact_templates() {
        let names = ["project", "updated", "title", "summary", "path"];
        let index = |name: &str| names.iter().position(|n| n.eq_ignore_ascii_case(name));
        let template = RowTemplate::parse(
            "project:muted|updated:date:right; title:title ;summary:muted",
            Some("path:devicon"),
            index,
        )
        .unwrap();
        assert_eq!(template.lines.len(), 3);
        let leading = template.leading.as_ref().unwrap();
        assert_eq!(leading.field, 4);
        assert_eq!(leading.kind, SlotKind::Icon);
        assert_eq!(leading.format, Some(RowFormat::new("devicon")));
        assert_eq!(template.title_line(), 1);
        let updated = &template.lines[0].slots[1];
        assert_eq!(updated.field, 1);
        assert_eq!(updated.align, RowAlign::Right);
        assert_eq!(updated.format, Some(RowFormat::new("date")));
        // Highlighting is the source's call, not the format's.
        assert!(updated.highlight);
        assert_eq!(template.lines[1].slots[0].style, RowStyle::Title);
        assert!(template.lines[1].slots[0].highlight);
        assert_eq!(template.max_field(), Some(4));
    }

    #[test]
    fn format_arguments_run_to_the_end_of_the_slot() {
        let index = |name: &str| ["a", "b"].iter().position(|n| *n == name);
        let template =
            RowTemplate::parse("a:muted:date=%H:%M:%S|b:number=2:right", None, index).unwrap();
        let [a, b] = &template.lines[0].slots[..] else {
            panic!("two slots");
        };
        assert_eq!(a.style, RowStyle::Muted);
        assert_eq!(a.format, Some(RowFormat::with_argument("date", "%H:%M:%S")));
        // `right` follows the argument, so it is part of it.
        assert_eq!(
            b.format,
            Some(RowFormat::with_argument("number", "2:right"))
        );
        assert_eq!(b.align, RowAlign::Left);
    }

    #[test]
    fn rejects_unknown_fields_and_second_formats() {
        let index = |name: &str| (name == "a").then_some(0);
        assert!(RowTemplate::parse("b", None, index).is_err());
        assert!(RowTemplate::parse("a:bytes:date", None, index).is_err());
        assert!(RowTemplate::parse("a", Some("b"), index).is_err());
        assert!(RowTemplate::parse(" ; ", None, index).is_err());
    }

    #[test]
    fn icon_slots_are_never_highlighted() {
        let index = |name: &str| ["a", "b"].iter().position(|n| *n == name);
        let template = RowTemplate::parse("a:icon|b", Some("b:devicon"), index).unwrap();
        let icon = &template.lines[0].slots[0];
        assert_eq!(icon.kind, SlotKind::Icon);
        assert!(!icon.highlight);
        assert!(!icon.clone().highlight(true).highlight);
        assert!(template.lines[0].slots[1].highlight);
        assert_eq!(template.slots().count(), 3);
        assert_eq!(template.max_field(), Some(1));
    }
}
