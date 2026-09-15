//! Polyline geometry shared by the client (and anyone else who wants it):
//! spline smoothing for rendering and Ramer–Douglas–Peucker simplification
//! for shrinking strokes before they are committed and shipped.

use crate::types::Point;

/// Resample a polyline through a **centripetal Catmull-Rom** spline. The
/// curve passes through every input point; `subdivide(span_length)` decides
/// how many segments to emit for each span (`0`/`1` keeps it straight).
///
/// Centripetal parameterisation (α = ½) never produces cusps or loops, even
/// when consecutive points are unevenly spaced — which freehand input is.
pub fn catmull_rom(points: &[Point], mut subdivide: impl FnMut(f32) -> usize) -> Vec<Point> {
    let n = points.len();
    if n < 3 {
        return points.to_vec();
    }
    let mut out = Vec::with_capacity(n * 4);
    out.push(points[0]);

    for i in 0..n - 1 {
        // Phantom end control points mirror the endpoints.
        let p0 = if i == 0 { points[0] } else { points[i - 1] };
        let p1 = points[i];
        let p2 = points[i + 1];
        let p3 = if i + 2 < n {
            points[i + 2]
        } else {
            points[n - 1]
        };

        let span = p1.distance(p2);
        let segments = subdivide(span).max(1);
        if segments == 1 || span <= f32::EPSILON {
            out.push(p2);
            continue;
        }

        // Knot sequence with α = 0.5: t_{k+1} = t_k + |p_{k+1} - p_k|^α.
        let t0 = 0.0;
        let t1 = t0 + knot(p0, p1);
        let t2 = t1 + knot(p1, p2);
        let t3 = t2 + knot(p2, p3);

        for s in 1..segments {
            let t = t1 + (t2 - t1) * (s as f32 / segments as f32);
            let a1 = lerp(p0, p1, (t1 - t) / (t1 - t0), (t - t0) / (t1 - t0));
            let a2 = lerp(p1, p2, (t2 - t) / (t2 - t1), (t - t1) / (t2 - t1));
            let a3 = lerp(p2, p3, (t3 - t) / (t3 - t2), (t - t2) / (t3 - t2));
            let b1 = lerp(a1, a2, (t2 - t) / (t2 - t0), (t - t0) / (t2 - t0));
            let b2 = lerp(a2, a3, (t3 - t) / (t3 - t1), (t - t1) / (t3 - t1));
            out.push(lerp(b1, b2, (t2 - t) / (t2 - t1), (t - t1) / (t2 - t1)));
        }
        out.push(p2);
    }
    out
}

/// Knot spacing for centripetal Catmull-Rom; coincident points get a tiny
/// positive spacing so the parameterisation never divides by zero.
fn knot(a: Point, b: Point) -> f32 {
    a.distance(b).sqrt().max(1e-4)
}

fn lerp(a: Point, b: Point, wa: f32, wb: f32) -> Point {
    Point::new(a.x * wa + b.x * wb, a.y * wa + b.y * wb)
}

/// Ramer–Douglas–Peucker: drop points that deviate less than `epsilon`
/// from the straight line between their retained neighbours. Endpoints are
/// always kept. Iterative, so deep recursion on long strokes is not a risk.
pub fn simplify(points: &[Point], epsilon: f32) -> Vec<Point> {
    let n = points.len();
    if n < 3 || epsilon <= 0.0 {
        return points.to_vec();
    }
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[n - 1] = true;

    let mut stack = vec![(0usize, n - 1)];
    while let Some((first, last)) = stack.pop() {
        if last <= first + 1 {
            continue;
        }
        let (a, b) = (points[first], points[last]);
        let (mut best, mut best_dist) = (first, 0.0f32);
        for (i, p) in points.iter().enumerate().take(last).skip(first + 1) {
            let d = perpendicular_distance(*p, a, b);
            if d > best_dist {
                best = i;
                best_dist = d;
            }
        }
        if best_dist > epsilon {
            keep[best] = true;
            stack.push((first, best));
            stack.push((best, last));
        }
    }

    points
        .iter()
        .zip(keep)
        .filter_map(|(p, k)| k.then_some(*p))
        .collect()
}

/// Distance from `p` to the infinite line through `a` and `b` (or to `a`
/// when the two coincide).
fn perpendicular_distance(p: Point, a: Point, b: Point) -> f32 {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let len2 = dx * dx + dy * dy;
    if len2 <= f32::EPSILON {
        return p.distance(a);
    }
    ((p.x - a.x) * dy - (p.y - a.y) * dx).abs() / len2.sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pts(v: &[(f32, f32)]) -> Vec<Point> {
        v.iter().map(|&(x, y)| Point::new(x, y)).collect()
    }

    #[test]
    fn catmull_rom_short_inputs_pass_through() {
        assert!(catmull_rom(&[], |_| 8).is_empty());
        let one = pts(&[(1.0, 2.0)]);
        assert_eq!(catmull_rom(&one, |_| 8), one);
        let two = pts(&[(0.0, 0.0), (10.0, 0.0)]);
        assert_eq!(catmull_rom(&two, |_| 8), two);
    }

    #[test]
    fn catmull_rom_interpolates_control_points() {
        let ctrl = pts(&[(0.0, 0.0), (10.0, 5.0), (20.0, -5.0), (30.0, 0.0)]);
        let curve = catmull_rom(&ctrl, |_| 4);
        // 3 spans × 4 segments + the first point.
        assert_eq!(curve.len(), 13);
        for (i, c) in ctrl.iter().enumerate() {
            assert_eq!(curve[i * 4], *c);
        }
        // Interior samples lie between neighbouring control points in x.
        assert!(curve[1].x > 0.0 && curve[1].x < 10.0);
    }

    #[test]
    fn catmull_rom_no_subdivision_is_identity() {
        let ctrl = pts(&[(0.0, 0.0), (1.0, 1.0), (2.0, 0.0), (3.0, 1.0)]);
        assert_eq!(catmull_rom(&ctrl, |_| 1), ctrl);
    }

    #[test]
    fn catmull_rom_survives_duplicate_points() {
        let ctrl = pts(&[(0.0, 0.0), (0.0, 0.0), (5.0, 5.0), (5.0, 5.0), (10.0, 0.0)]);
        for p in catmull_rom(&ctrl, |_| 6) {
            assert!(p.x.is_finite() && p.y.is_finite());
        }
    }

    #[test]
    fn simplify_drops_collinear_points() {
        let line = pts(&[
            (0.0, 0.0),
            (1.0, 0.01),
            (2.0, -0.01),
            (3.0, 0.0),
            (4.0, 0.0),
        ]);
        let out = simplify(&line, 0.1);
        assert_eq!(out, pts(&[(0.0, 0.0), (4.0, 0.0)]));
    }

    #[test]
    fn simplify_keeps_corners() {
        let corner = pts(&[
            (0.0, 0.0),
            (5.0, 0.0),
            (10.0, 0.0),
            (10.0, 5.0),
            (10.0, 10.0),
        ]);
        assert_eq!(
            simplify(&corner, 0.5),
            pts(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)])
        );
    }

    #[test]
    fn simplify_short_inputs_and_zero_epsilon_pass_through() {
        let two = pts(&[(0.0, 0.0), (1.0, 1.0)]);
        assert_eq!(simplify(&two, 1.0), two);
        let three = pts(&[(0.0, 0.0), (1.0, 0.0), (2.0, 0.0)]);
        assert_eq!(simplify(&three, 0.0), three);
    }

    #[test]
    fn simplify_closed_loop_keeps_shape() {
        // First and last coincide: the "line" degenerates to a point, so the
        // farthest vertex is kept and the loop is preserved.
        let square = pts(&[
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 10.0),
            (0.0, 0.0),
        ]);
        assert_eq!(simplify(&square, 0.5), square);
    }
}
