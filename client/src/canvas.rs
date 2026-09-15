//! Canvas geometry: the world ↔ screen camera, stroke rasterisation through
//! the `egui` painter, and hit-testing for the eraser.

use common::{Point, Rgba, Stroke};
use egui::{Color32, Painter, Pos2, Rect, Shape, Vec2, epaint::PathStroke};

pub const MIN_ZOOM: f32 = 0.1;
pub const MAX_ZOOM: f32 = 20.0;

/// Maps canvas (world) coordinates to the screen rect the canvas occupies.
#[derive(Debug, Clone, Copy)]
pub struct Camera {
    /// World point shown at the centre of the viewport.
    pub center: Point,
    /// Screen pixels per world unit.
    pub zoom: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            center: Point::ZERO,
            zoom: 1.0,
        }
    }
}

impl Camera {
    pub fn to_screen(self, rect: Rect, p: Point) -> Pos2 {
        rect.center() + Vec2::new(p.x - self.center.x, p.y - self.center.y) * self.zoom
    }

    pub fn to_world(self, rect: Rect, s: Pos2) -> Point {
        let d = (s - rect.center()) / self.zoom;
        Point::new(self.center.x + d.x, self.center.y + d.y)
    }

    /// Visible world-space rectangle `(min, max)`.
    pub fn world_bounds(self, rect: Rect) -> (Point, Point) {
        (self.to_world(rect, rect.min), self.to_world(rect, rect.max))
    }

    /// Move the view by a screen-space delta (drag panning).
    pub fn pan(&mut self, delta: Vec2) {
        self.center.x -= delta.x / self.zoom;
        self.center.y -= delta.y / self.zoom;
    }

    /// Scale around a screen point so the world under the pointer stays put.
    pub fn zoom_at(&mut self, rect: Rect, anchor: Pos2, factor: f32) {
        let before = self.to_world(rect, anchor);
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let after = self.to_world(rect, anchor);
        self.center.x += before.x - after.x;
        self.center.y += before.y - after.y;
    }
}

pub fn to_color32(c: Rgba) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a)
}

pub fn from_color32(c: Color32) -> Rgba {
    Rgba::from(c.to_srgba_unmultiplied())
}

/// `true` if the stroke's bounding box overlaps the visible world rect.
fn in_view(stroke: &Stroke, (lo, hi): (Point, Point)) -> bool {
    stroke
        .bounds()
        .is_some_and(|(min, max)| max.x >= lo.x && min.x <= hi.x && max.y >= lo.y && min.y <= hi.y)
}

/// Draw one stroke. `alpha` scales the stroke colour (previews are faded).
pub fn paint_stroke(painter: &Painter, cam: &Camera, rect: Rect, stroke: &Stroke, alpha: f32) {
    if !in_view(stroke, cam.world_bounds(rect)) {
        return;
    }
    let color = to_color32(stroke.color).gamma_multiply(alpha);
    let width = (stroke.width * cam.zoom).max(0.75);
    match stroke.points.as_slice() {
        [] => {}
        [p] => {
            painter.circle_filled(cam.to_screen(rect, *p), width * 0.5, color);
        }
        pts => {
            let screen: Vec<Pos2> = pts.iter().map(|p| cam.to_screen(rect, *p)).collect();
            // Round caps: egui paths have butt ends, so cap them manually.
            painter.circle_filled(screen[0], width * 0.5, color);
            painter.circle_filled(screen[screen.len() - 1], width * 0.5, color);
            painter.add(Shape::line(screen, PathStroke::new(width, color)));
        }
    }
}

/// Distance from `p` to the segment `a`–`b`.
fn segment_distance(p: Point, a: Point, b: Point) -> f32 {
    let ab = Point::new(b.x - a.x, b.y - a.y);
    let len2 = ab.x * ab.x + ab.y * ab.y;
    if len2 <= f32::EPSILON {
        return p.distance(a);
    }
    let t = (((p.x - a.x) * ab.x + (p.y - a.y) * ab.y) / len2).clamp(0.0, 1.0);
    p.distance(Point::new(a.x + ab.x * t, a.y + ab.y * t))
}

/// `true` if `p` lies within `tolerance` of the stroke's painted body.
pub fn hit_test(stroke: &Stroke, p: Point, tolerance: f32) -> bool {
    let reach = stroke.width * 0.5 + tolerance;
    let Some((min, max)) = stroke.bounds() else {
        return false;
    };
    if p.x < min.x - tolerance
        || p.x > max.x + tolerance
        || p.y < min.y - tolerance
        || p.y > max.y + tolerance
    {
        return false;
    }
    match stroke.points.as_slice() {
        [] => false,
        [only] => only.distance(p) <= reach,
        pts => pts
            .windows(2)
            .any(|w| segment_distance(p, w[0], w[1]) <= reach),
    }
}

/// Subtle dot grid so panning and zooming have a visible reference.
pub fn paint_grid(painter: &Painter, cam: &Camera, rect: Rect, color: Color32) {
    // Pick a spacing that stays between ~24 and ~96 screen px.
    let mut spacing = 32.0_f32;
    while spacing * cam.zoom < 24.0 {
        spacing *= 2.0;
    }
    while spacing * cam.zoom > 96.0 {
        spacing /= 2.0;
    }
    let (lo, hi) = cam.world_bounds(rect);
    let x0 = (lo.x / spacing).floor() as i64;
    let x1 = (hi.x / spacing).ceil() as i64;
    let y0 = (lo.y / spacing).floor() as i64;
    let y1 = (hi.y / spacing).ceil() as i64;
    if (x1 - x0) * (y1 - y0) > 40_000 {
        return; // absurdly zoomed out; the dots would be noise anyway
    }
    let radius = (cam.zoom * 0.75).clamp(0.75, 1.5);
    for gx in x0..=x1 {
        for gy in y0..=y1 {
            let p = cam.to_screen(rect, Point::new(gx as f32 * spacing, gy as f32 * spacing));
            painter.circle_filled(p, radius, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn rect() -> Rect {
        Rect::from_min_size(Pos2::new(100.0, 50.0), Vec2::new(800.0, 600.0))
    }

    #[test]
    fn screen_world_round_trip() {
        let cam = Camera {
            center: Point::new(12.5, -7.0),
            zoom: 2.5,
        };
        let s = Pos2::new(333.0, 222.0);
        let back = cam.to_screen(rect(), cam.to_world(rect(), s));
        assert!((back - s).length() < 1e-3);
    }

    #[test]
    fn zoom_keeps_anchor_fixed() {
        let mut cam = Camera::default();
        let anchor = Pos2::new(700.0, 400.0);
        let before = cam.to_world(rect(), anchor);
        cam.zoom_at(rect(), anchor, 3.0);
        let after = cam.to_world(rect(), anchor);
        assert!(before.distance(after) < 1e-3);
        assert!((cam.zoom - 3.0).abs() < 1e-6);
    }

    #[test]
    fn zoom_is_clamped() {
        let mut cam = Camera::default();
        cam.zoom_at(rect(), rect().center(), 1000.0);
        assert_eq!(cam.zoom, MAX_ZOOM);
        cam.zoom_at(rect(), rect().center(), 1e-6);
        assert_eq!(cam.zoom, MIN_ZOOM);
    }

    #[test]
    fn hit_test_respects_width_and_tolerance() {
        let s = Stroke::new(Uuid::nil(), Rgba::BLACK, 4.0)
            .with_points([Point::new(0.0, 0.0), Point::new(10.0, 0.0)]);
        assert!(hit_test(&s, Point::new(5.0, 1.9), 0.0));
        assert!(!hit_test(&s, Point::new(5.0, 2.5), 0.0));
        assert!(hit_test(&s, Point::new(5.0, 2.5), 1.0));
        assert!(!hit_test(&s, Point::new(15.0, 0.0), 1.0));
        assert!(hit_test(&s, Point::new(12.0, 0.0), 1.0));
    }

    #[test]
    fn hit_test_single_point_stroke() {
        let s = Stroke::new(Uuid::nil(), Rgba::BLACK, 6.0).with_points([Point::new(3.0, 3.0)]);
        assert!(hit_test(&s, Point::new(5.0, 3.0), 0.0));
        assert!(!hit_test(&s, Point::new(7.0, 3.0), 0.0));
    }

    #[test]
    fn opaque_color_round_trip() {
        // `Color32` is premultiplied, so only opaque colours survive exactly;
        // the brush picker only produces opaque colours.
        let c = Rgba::from_hex(0x0ac81e);
        assert_eq!(from_color32(to_color32(c)), c);
    }
}
