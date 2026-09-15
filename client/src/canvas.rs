//! Canvas geometry: the world ↔ screen camera, stroke rasterisation through
//! the `egui` painter, and hit-testing for the eraser.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use common::{Point, Rgba, Stroke, StrokeId, geom};
use egui::epaint::{Mesh, PathStroke, Tessellator};
use egui::{Color32, Painter, Pos2, Rect, Shape, Vec2};

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
fn in_view(stroke: &Stroke, view: (Point, Point)) -> bool {
    stroke.bounds().is_some_and(|b| overlaps(b, view))
}

/// Target spacing between spline samples, in screen pixels.
const SMOOTH_STEP_PX: f32 = 3.0;
const MAX_SUBDIVISIONS: usize = 12;

/// Resample a stroke's control points for rendering at `zoom`.
fn smoothed(points: &[Point], zoom: f32) -> Vec<Point> {
    geom::catmull_rom(points, |span| {
        ((span * zoom / SMOOTH_STEP_PX).ceil() as usize).clamp(1, MAX_SUBDIVISIONS)
    })
}

/// Build the shapes for a stroke with `map` taking world points to the
/// target space and `scale` the world→target factor (for the brush width).
fn stroke_shape(stroke: &Stroke, scale: f32, alpha: f32, map: impl Fn(Point) -> Pos2) -> Shape {
    let color = to_color32(stroke.color).gamma_multiply(alpha);
    let width = (stroke.width * scale).max(0.75);
    match stroke.points.as_slice() {
        [] => Shape::Noop,
        [p] => Shape::circle_filled(map(*p), width * 0.5, color),
        pts => {
            let path: Vec<Pos2> = smoothed(pts, scale).into_iter().map(map).collect();
            // Round caps: egui paths have butt ends, so cap them manually.
            Shape::Vec(vec![
                Shape::circle_filled(path[0], width * 0.5, color),
                Shape::circle_filled(path[path.len() - 1], width * 0.5, color),
                Shape::line(path, PathStroke::new(width, color)),
            ])
        }
    }
}

/// Draw one stroke straight through the painter (no caching). Used for
/// strokes whose points change every frame: live previews and the one
/// being drawn locally. `alpha` scales the colour so previews can be faded.
pub fn paint_stroke(painter: &Painter, cam: &Camera, rect: Rect, stroke: &Stroke, alpha: f32) {
    if !in_view(stroke, cam.world_bounds(rect)) {
        return;
    }
    painter.add(stroke_shape(stroke, cam.zoom, alpha, |p| {
        cam.to_screen(rect, p)
    }));
}

/// Tessellated meshes for committed strokes, keyed by id. This is the CPU
/// half of the renderer: [`crate::gpu::StrokeRenderer`] uploads these into
/// vertex buffers and the tessellation is never repeated while the entry
/// lives.
///
/// Meshes are built at a *quantised* zoom (powers of 2^(1/4)) in
/// coordinates local to the stroke's bounding-box corner; at draw time they
/// are scaled by the small residual factor and translated into place. The
/// residual is at most ~9% either way, so anti-aliasing feathering (which
/// is baked into the mesh) stays visually correct. Crossing a zoom bucket
/// or changing DPI invalidates everything.
pub struct TessCache {
    zoom: f32,
    pixels_per_point: f32,
    tessellator: Option<Tessellator>,
    meshes: HashMap<StrokeId, Cached>,
    /// Ids requested since the last [`Self::end_frame`]; the rest are pruned there.
    touched: HashSet<StrokeId>,
    built_this_frame: usize,
}

pub struct Cached {
    /// World-space bounds, so view culling never re-walks the points.
    pub bounds: (Point, Point),
    /// Vertices at `(world - bounds.0) * zoom`.
    pub mesh: Mesh,
}

impl Default for TessCache {
    fn default() -> Self {
        Self {
            zoom: 0.0,
            pixels_per_point: 0.0,
            tessellator: None,
            meshes: HashMap::new(),
            touched: HashSet::new(),
            built_this_frame: 0,
        }
    }
}

/// Snap a zoom level to the nearest 2^(k/4) bucket.
pub fn quantize_zoom(zoom: f32) -> f32 {
    2f32.powf((zoom.log2() * 4.0).round() / 4.0)
}

impl TessCache {
    /// Call once per frame before requesting meshes. Returns `true` when
    /// the zoom bucket or DPI changed and every mesh was dropped, so GPU-side
    /// copies must be rebuilt too.
    pub fn begin_frame(&mut self, ctx: &egui::Context, zoom: f32) -> bool {
        let zoom = quantize_zoom(zoom);
        let ppp = ctx.pixels_per_point();
        self.built_this_frame = 0;
        if self.tessellator.is_some() && zoom == self.zoom && ppp == self.pixels_per_point {
            return false;
        }
        self.touched.clear();
        let mut options = ctx.tessellation_options(|o| *o);
        options.coarse_tessellation_culling = false;
        // The pre-rasterised discs live in the font atlas, which we do not
        // sample from, so end caps are tessellated geometrically.
        options.prerasterized_discs = false;
        let font_tex_size = ctx.fonts(|f| f.font_image_size());
        self.tessellator = Some(Tessellator::new(ppp, options, font_tex_size, Vec::new()));
        self.meshes.clear();
        self.zoom = zoom;
        self.pixels_per_point = ppp;
        true
    }

    /// The quantised zoom the current meshes were built at.
    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// Fetch (building on first use) the mesh for a stroke. `None` for an
    /// empty stroke.
    pub fn get(&mut self, stroke: &Stroke) -> Option<&Cached> {
        let cache_zoom = self.zoom;
        let tess = self.tessellator.as_mut().expect("begin_frame before get");
        self.touched.insert(stroke.id);
        let entry = match self.meshes.entry(stroke.id) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(v) => {
                let bounds = stroke.bounds()?;
                self.built_this_frame += 1;
                let min = bounds.0;
                let shape = stroke_shape(stroke, cache_zoom, 1.0, |p| {
                    Pos2::new((p.x - min.x) * cache_zoom, (p.y - min.y) * cache_zoom)
                });
                let mut mesh = Mesh::default();
                tess.tessellate_shape(shape, &mut mesh);
                v.insert(Cached { bounds, mesh })
            }
        };
        Some(entry)
    }

    /// Forget meshes for strokes that were not requested since the previous
    /// call (i.e. during this sync pass).
    pub fn end_frame(&mut self) {
        let touched = &self.touched;
        self.meshes.retain(|id, _| touched.contains(id));
        self.touched.clear();
    }

    pub fn len(&self) -> usize {
        self.meshes.len()
    }

    /// Meshes tessellated during the current frame (for the debug readout).
    pub fn built_this_frame(&self) -> usize {
        self.built_this_frame
    }
}

/// `true` if two world-space rects `(min, max)` overlap.
pub fn overlaps((min, max): (Point, Point), (lo, hi): (Point, Point)) -> bool {
    max.x >= lo.x && min.x <= hi.x && max.y >= lo.y && min.y <= hi.y
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
    fn zoom_quantisation_is_idempotent_and_close() {
        for z in [0.1, 0.37, 1.0, 1.05, 2.9, 20.0] {
            let q = quantize_zoom(z);
            assert_eq!(quantize_zoom(q), q);
            let ratio = z / q;
            assert!((0.9..=1.1).contains(&ratio), "zoom {z} → {q}");
        }
        assert_eq!(quantize_zoom(1.0), 1.0);
        assert_eq!(quantize_zoom(2.0), 2.0);
    }

    #[test]
    fn smoothing_keeps_endpoints_and_densifies() {
        let pts = [
            Point::new(0.0, 0.0),
            Point::new(10.0, 10.0),
            Point::new(20.0, 0.0),
        ];
        let out = smoothed(&pts, 1.0);
        assert!(out.len() > pts.len());
        assert_eq!(out[0], pts[0]);
        assert_eq!(*out.last().unwrap(), pts[2]);
    }

    #[test]
    fn opaque_color_round_trip() {
        // `Color32` is premultiplied, so only opaque colours survive exactly;
        // the brush picker only produces opaque colours.
        let c = Rgba::from_hex(0x0ac81e);
        assert_eq!(from_color32(to_color32(c)), c);
    }
}
