//! Plain domain types shared by every crate.

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

/// A completed freehand stroke. Immutable once committed to the CRDT: the
/// same `id` always refers to the same payload, so replication only needs
/// to reason about *membership* (present / tombstoned), not content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stroke {
    pub id: StrokeId,
    /// The peer that drew this stroke.
    pub client_id: ClientId,
    pub color: Rgba,
    /// Brush width in canvas units.
    pub width: f32,
    pub points: Vec<Point>,
}

impl Stroke {
    pub fn new(client_id: ClientId, color: Rgba, width: f32) -> Self {
        Self {
            id: Uuid::new_v4(),
            client_id,
            color,
            width,
            points: Vec::new(),
        }
    }

    pub fn with_points(mut self, points: impl IntoIterator<Item = Point>) -> Self {
        self.points.extend(points);
        self
    }

    /// Axis-aligned bounding box `(min, max)`, padded by half the brush width.
    /// Returns `None` for an empty stroke.
    pub fn bounds(&self) -> Option<(Point, Point)> {
        let first = *self.points.first()?;
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
