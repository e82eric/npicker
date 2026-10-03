use egui::{Pos2, Rect, Vec2};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Position {
    Top,
    #[default]
    Center,
    Bottom,
    /// Leave a margin below the anchor when the picker fits; otherwise anchor
    /// to the bottom of the viewport.
    Cursor,
}

/// All coordinates are logical egui points in the host viewport.
#[derive(Clone, Copy, Debug)]
pub struct Placement {
    pub bounds: Rect,
    pub position: Position,
    pub anchor: Option<Pos2>,
    pub width: f32,
    pub margin: f32,
}
impl Placement {
    pub fn new(bounds: Rect) -> Self {
        Self {
            bounds,
            position: Position::Center,
            anchor: None,
            width: f32::INFINITY,
            margin: 10.0,
        }
    }
    pub(crate) fn rect(self, requested_height: f32) -> Rect {
        let margin = self
            .margin
            .max(0.0)
            .min(self.bounds.width().min(self.bounds.height()).max(0.0) / 2.0);
        let bounds = self.bounds.shrink(margin);
        let size = Vec2::new(
            self.width.max(1.0).min(bounds.width().max(0.0)),
            requested_height.max(0.0).min(bounds.height().max(0.0)),
        );
        let x = bounds.center().x - size.x / 2.0;
        let y = match self.position {
            Position::Top => bounds.top(),
            Position::Center => bounds.center().y - size.y / 2.0,
            Position::Bottom => bounds.bottom() - size.y,
            Position::Cursor => self
                .anchor
                .map(|anchor| anchor.y + margin)
                .filter(|&below| below + size.y <= bounds.bottom())
                .unwrap_or(bounds.bottom() - size.y),
        }
        .clamp(bounds.top(), (bounds.bottom() - size.y).max(bounds.top()));
        Rect::from_min_size(Pos2::new(x, y), size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cursor_placement_leaves_gap_or_anchors_to_viewport_bottom() {
        let bounds = Rect::from_min_size(Pos2::new(100.0, 200.0), Vec2::new(400.0, 300.0));
        let mut placement = Placement::new(bounds);
        placement.position = Position::Cursor;
        placement.anchor = Some(Pos2::new(200.0, 250.0));
        assert_eq!(placement.rect(150.0).top(), 260.0);
        // There is space below, but not enough for the complete picker plus gap.
        placement.anchor = Some(Pos2::new(200.0, 350.0));
        let rect = placement.rect(150.0);
        assert_eq!(rect.bottom(), 490.0);
        assert!(bounds.contains_rect(rect));
        placement.anchor = Some(Pos2::new(200.0, 490.0));
        assert_eq!(placement.rect(150.0).bottom(), 490.0);
        assert!(bounds.contains_rect(placement.rect(1000.0)));
        placement.anchor = None;
        assert_eq!(placement.rect(150.0).bottom(), 490.0);
    }
}
