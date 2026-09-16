//! Plain domain types shared by every crate.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Identifies a connected peer. Generated once per client session.
pub type ClientId = Uuid;
/// Identifies a single stroke, globally unique across all peers.
pub type StrokeId = Uuid;
/// Rooms are addressed by a human-readable slug (e.g. `"design-review"`).
pub type RoomId = String;

/// A point in *canvas* space (world coordinates, not screen pixels).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const ZERO: Point = Point { x: 0.0, y: 0.0 };

    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    /// Euclidean distance to another point.
    pub fn distance(self, other: Point) -> f32 {
        ((self.x - other.x).powi(2) + (self.y - other.y).powi(2)).sqrt()
    }
}

impl From<[f32; 2]> for Point {
    fn from([x, y]: [f32; 2]) -> Self {
        Self { x, y }
    }
}

impl From<Point> for [f32; 2] {
    fn from(p: Point) -> Self {
        [p.x, p.y]
    }
}

/// 8-bit-per-channel colour with straight (non-premultiplied) alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const WHITE: Rgba = Rgba::rgb(0xff, 0xff, 0xff);
    pub const BLACK: Rgba = Rgba::rgb(0x00, 0x00, 0x00);
    pub const TRANSPARENT: Rgba = Rgba::new(0, 0, 0, 0);

    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 0xff }
    }

    /// Build from a `0xRRGGBB` literal (fully opaque).
    pub const fn from_hex(hex: u32) -> Self {
        Self::rgb(
            ((hex >> 16) & 0xff) as u8,
            ((hex >> 8) & 0xff) as u8,
            (hex & 0xff) as u8,
        )
    }

    pub const fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }

    pub const fn to_array(self) -> [u8; 4] {
        [self.r, self.g, self.b, self.a]
    }
}

impl Default for Rgba {
    fn default() -> Self {
        Rgba::WHITE
    }
}

impl From<[u8; 4]> for Rgba {
    fn from([r, g, b, a]: [u8; 4]) -> Self {
        Self { r, g, b, a }
    }
}

/// What a stroke's `points` describe.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum StrokeKind {
    /// A freehand polyline through every point.
    #[default]
    Freehand,
    /// A straight segment from `points[0]` to `points[1]`.
    Line,
    /// An axis-aligned rectangle with opposite corners `points[0]`, `points[1]`.
    Rect,
    /// An ellipse inscribed in the rectangle with corners `points[0]`, `points[1]`.
    Ellipse,
    /// Text laid out from the top-left anchor `points[0]`. `size` is the
    /// line height in canvas units; the stroke's `width` is unused.
    Text { content: String, size: f32 },
}

impl StrokeKind {
    /// Closed outlines (rect, ellipse) join their last point back to the first.
    pub fn is_closed(&self) -> bool {
        matches!(self, StrokeKind::Rect | StrokeKind::Ellipse)
    }

    /// Two-point shapes are defined by a start and end corner rather than a
    /// point trail, so a live preview *replaces* its points instead of
    /// appending to them.
    pub fn is_two_point(&self) -> bool {
        matches!(
            self,
            StrokeKind::Line | StrokeKind::Rect | StrokeKind::Ellipse
        )
    }
}

/// Rough per-glyph advance and line height as a fraction of the font size,
/// used to estimate text extents without font metrics (see [`Stroke::bounds`]).
const TEXT_ADVANCE: f32 = 0.55;
const TEXT_LINE_HEIGHT: f32 = 1.25;

/// A completed stroke — freehand, a primitive shape or a text label.
/// Immutable once committed to the CRDT: the same `id` always refers to
/// the same payload, so replication only needs to reason about
/// *membership* (present / tombstoned), not content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stroke {
    pub id: StrokeId,
    /// The peer that drew this stroke.
    pub client_id: ClientId,
    pub color: Rgba,
    /// Brush width in canvas units.
    pub width: f32,
    pub kind: StrokeKind,
    pub points: Vec<Point>,
}

impl Stroke {
    /// A new, empty freehand stroke.
    pub fn new(client_id: ClientId, color: Rgba, width: f32) -> Self {
        Self {
            id: Uuid::new_v4(),
            client_id,
            color,
            width,
            kind: StrokeKind::Freehand,
            points: Vec::new(),
        }
    }

    pub fn with_kind(mut self, kind: StrokeKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn with_points(mut self, points: impl IntoIterator<Item = Point>) -> Self {
        self.points.extend(points);
        self
    }

    /// The label and line height of a text stroke.
    pub fn text(&self) -> Option<(&str, f32)> {
        match &self.kind {
            StrokeKind::Text { content, size } => Some((content, *size)),
            _ => None,
        }
    }

    /// Estimated extent `(width, height)` of a text stroke, in canvas
    /// units. Real metrics need a font, so this is only used for culling
    /// and hit-testing.
    pub fn text_extent(&self) -> Option<(f32, f32)> {
        let (content, size) = self.text()?;
        let lines = content.lines().count().max(1);
        let columns = content
            .lines()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0)
            .max(1);
        Some((
            columns as f32 * size * TEXT_ADVANCE,
            lines as f32 * size * TEXT_LINE_HEIGHT,
        ))
    }

    /// The polyline that traces this stroke: the points themselves for
    /// freehand and lines, the corners for a rectangle, `ellipse_segments`
    /// chords for an ellipse. Closed shapes repeat their first point at the
    /// end. Text has no outline. Shapes given fewer than two points collapse
    /// to whatever points they have.
    pub fn outline(&self, ellipse_segments: usize) -> Cow<'_, [Point]> {
        match (&self.kind, self.points.as_slice()) {
            (StrokeKind::Text { .. }, _) => Cow::Borrowed(&[]),
            (StrokeKind::Freehand, pts) => Cow::Borrowed(pts),
            (StrokeKind::Line, pts) => Cow::Borrowed(&pts[..pts.len().min(2)]),
            (StrokeKind::Rect, &[a, b, ..]) => {
                Cow::Owned(vec![a, Point::new(b.x, a.y), b, Point::new(a.x, b.y), a])
            }
            (StrokeKind::Ellipse, &[a, b, ..]) => {
                let c = Point::new((a.x + b.x) * 0.5, (a.y + b.y) * 0.5);
                let (rx, ry) = ((b.x - a.x).abs() * 0.5, (b.y - a.y).abs() * 0.5);
                let n = ellipse_segments.max(3);
                Cow::Owned(
                    (0..=n)
                        .map(|i| {
                            let t = i as f32 / n as f32 * std::f32::consts::TAU;
                            Point::new(c.x + rx * t.cos(), c.y + ry * t.sin())
                        })
                        .collect(),
                )
            }
            (StrokeKind::Rect | StrokeKind::Ellipse, pts) => Cow::Borrowed(pts),
        }
    }

    /// Axis-aligned bounding box `(min, max)`, padded by half the brush
    /// width (text: the estimated extent instead). Returns `None` for an
    /// empty stroke.
    pub fn bounds(&self) -> Option<(Point, Point)> {
        let first = *self.points.first()?;
        if let Some((w, h)) = self.text_extent() {
            return Some((first, Point::new(first.x + w, first.y + h)));
        }
        let (min, max) = self.points.iter().fold((first, first), |(lo, hi), p| {
            (
                Point::new(lo.x.min(p.x), lo.y.min(p.y)),
                Point::new(hi.x.max(p.x), hi.y.max(p.y)),
            )
        });
        let pad = self.width * 0.5;
        Some((
            Point::new(min.x - pad, min.y - pad),
            Point::new(max.x + pad, max.y + pad),
        ))
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }
}

/// Ephemeral presence ("awareness") information for one peer.
///
/// This is *not* part of the replicated document: it is broadcast at high
/// frequency, never persisted, and the latest value simply wins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CursorState {
    pub client_id: ClientId,
    /// Display name shown next to the remote cursor.
    pub name: String,
    /// Colour used for the cursor glyph and name label.
    pub color: Rgba,
    /// Cursor position in canvas space; `None` while the pointer is off-canvas.
    pub position: Option<Point>,
    /// `true` while the peer is mid-stroke (lets remotes render a "drawing" hint).
    pub drawing: bool,
}

impl CursorState {
    pub fn new(client_id: ClientId, name: impl Into<String>, color: Rgba) -> Self {
        Self {
            client_id,
            name: name.into(),
            color,
            position: None,
            drawing: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(kind: StrokeKind) -> Stroke {
        Stroke::new(Uuid::nil(), Rgba::BLACK, 2.0)
            .with_kind(kind)
            .with_points([Point::new(10.0, 20.0), Point::new(40.0, 60.0)])
    }

    #[test]
    fn rect_outline_is_closed_and_bounds_are_padded() {
        let s = shape(StrokeKind::Rect);
        let o = s.outline(8);
        assert_eq!(o.len(), 5);
        assert_eq!(o[0], o[4]);
        assert_eq!(o[1], Point::new(40.0, 20.0));
        assert_eq!(
            s.bounds(),
            Some((Point::new(9.0, 19.0), Point::new(41.0, 61.0)))
        );
    }

    #[test]
    fn ellipse_outline_stays_inside_its_box() {
        let s = shape(StrokeKind::Ellipse);
        let o = s.outline(32);
        assert_eq!(o.len(), 33);
        assert!(
            o.iter()
                .all(|p| (10.0..=40.0).contains(&p.x) && (20.0..=60.0).contains(&p.y))
        );
        assert!((o[0].x - 40.0).abs() < 1e-4 && (o[0].y - 40.0).abs() < 1e-4);
    }

    #[test]
    fn shapes_with_one_point_do_not_panic() {
        for kind in [StrokeKind::Line, StrokeKind::Rect, StrokeKind::Ellipse] {
            let s = Stroke::new(Uuid::nil(), Rgba::BLACK, 2.0)
                .with_kind(kind)
                .with_points([Point::ZERO]);
            assert_eq!(s.outline(8).len(), 1);
            assert!(s.bounds().is_some());
        }
    }

    #[test]
    fn text_bounds_grow_with_content() {
        let mut s = Stroke::new(Uuid::nil(), Rgba::BLACK, 2.0)
            .with_kind(StrokeKind::Text {
                content: "hi".into(),
                size: 10.0,
            })
            .with_points([Point::ZERO]);
        assert!(s.outline(8).is_empty());
        let (_, small) = s.bounds().unwrap();
        s.kind = StrokeKind::Text {
            content: "hello\nworld".into(),
            size: 10.0,
        };
        let (_, big) = s.bounds().unwrap();
        assert!(big.x > small.x && big.y > small.y);
    }
}
