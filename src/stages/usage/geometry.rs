//! Affine matrices and axis-aligned rectangles in PDF user space.

/// A PDF transformation matrix `[a b c d e f]`: `x' = a x + c y + e`,
/// `y' = b x + d y + f`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Matrix {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl Matrix {
    pub const IDENTITY: Matrix = Matrix::from_array([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    /// From `[a b c d e f]` as written in a PDF.
    pub const fn from_array(m: [f32; 6]) -> Matrix {
        Matrix {
            a: m[0],
            b: m[1],
            c: m[2],
            d: m[3],
            e: m[4],
            f: m[5],
        }
    }

    /// `self` applied first, then `rhs` (the PDF `cm` convention:
    /// `new_ctm = cm_matrix.then(ctm)`).
    pub fn then(self, rhs: Matrix) -> Matrix {
        Matrix {
            a: self.a * rhs.a + self.b * rhs.c,
            b: self.a * rhs.b + self.b * rhs.d,
            c: self.c * rhs.a + self.d * rhs.c,
            d: self.c * rhs.b + self.d * rhs.d,
            e: self.e * rhs.a + self.f * rhs.c + rhs.e,
            f: self.e * rhs.b + self.f * rhs.d + rhs.f,
        }
    }

    pub fn apply(self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    pub fn invert(self) -> Option<Matrix> {
        let det = self.a * self.d - self.b * self.c;
        if det.abs() < 1e-12 || !det.is_finite() {
            return None;
        }
        let (a, b, c, d) = (self.d / det, -self.b / det, -self.c / det, self.a / det);
        Some(Matrix {
            a,
            b,
            c,
            d,
            e: -(self.e * a + self.f * c),
            f: -(self.e * b + self.f * d),
        })
    }

    /// Lengths of the images of the unit x and y vectors: the rendered
    /// width and height of a unit square drawn under this matrix.
    pub fn unit_extent(self) -> (f32, f32) {
        (
            (self.a * self.a + self.b * self.b).sqrt(),
            (self.c * self.c + self.d * self.d).sqrt(),
        )
    }

    /// Bounding box of the unit square under this matrix.
    pub fn unit_square_bbox(self) -> Rect {
        Rect::UNIT.transformed(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Rect {
    pub const UNIT: Rect = Rect::new(0.0, 0.0, 1.0, 1.0);
    pub const EVERYTHING: Rect = Rect::new(f32::MIN, f32::MIN, f32::MAX, f32::MAX);

    pub const fn new(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        Rect { x0, y0, x1, y1 }
    }

    /// Normalized so that `x0 <= x1` and `y0 <= y1`.
    pub fn from_corners(ax: f32, ay: f32, bx: f32, by: f32) -> Rect {
        Rect::new(ax.min(bx), ay.min(by), ax.max(bx), ay.max(by))
    }

    pub fn from_points(points: &[(f32, f32)]) -> Option<Rect> {
        let first = *points.first()?;
        Some(points.iter().fold(
            Rect::new(first.0, first.1, first.0, first.1),
            |r, &(x, y)| Rect::new(r.x0.min(x), r.y0.min(y), r.x1.max(x), r.y1.max(y)),
        ))
    }

    pub fn intersect(self, other: Rect) -> Rect {
        Rect::new(
            self.x0.max(other.x0),
            self.y0.max(other.y0),
            self.x1.min(other.x1),
            self.y1.min(other.y1),
        )
    }

    pub fn union(self, other: Rect) -> Rect {
        Rect::new(
            self.x0.min(other.x0),
            self.y0.min(other.y0),
            self.x1.max(other.x1),
            self.y1.max(other.y1),
        )
    }

    pub fn is_empty(self) -> bool {
        !(self.x1 > self.x0 && self.y1 > self.y0)
    }

    pub fn width(self) -> f32 {
        self.x1 - self.x0
    }

    pub fn height(self) -> f32 {
        self.y1 - self.y0
    }

    /// Bounding box of this rectangle's corners under `m`.
    pub fn transformed(self, m: Matrix) -> Rect {
        let corners = [
            m.apply(self.x0, self.y0),
            m.apply(self.x1, self.y0),
            m.apply(self.x0, self.y1),
            m.apply(self.x1, self.y1),
        ];
        Rect::from_points(&corners).unwrap_or(self)
    }

    /// True when `self` contains `other` up to a small tolerance.
    pub fn covers(self, other: Rect) -> bool {
        const EPS: f32 = 1e-4;
        self.x0 <= other.x0 + EPS
            && self.y0 <= other.y0 + EPS
            && self.x1 >= other.x1 - EPS
            && self.y1 >= other.y1 - EPS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn then_composes_in_pdf_order() {
        let scale = Matrix::from_array([2.0, 0.0, 0.0, 3.0, 0.0, 0.0]);
        let shift = Matrix::from_array([1.0, 0.0, 0.0, 1.0, 10.0, 20.0]);
        // Scale first, then shift.
        let (x, y) = scale.then(shift).apply(1.0, 1.0);
        assert!(close(x, 12.0) && close(y, 23.0));
        // Shift first, then scale.
        let (x, y) = shift.then(scale).apply(1.0, 1.0);
        assert!(close(x, 22.0) && close(y, 63.0));
    }

    #[test]
    fn invert_round_trips() {
        let m = Matrix::from_array([2.0, 1.0, -1.0, 3.0, 5.0, -7.0]);
        let inv = m.invert().unwrap();
        let (x, y) = inv.apply(m.apply(4.0, 9.0).0, m.apply(4.0, 9.0).1);
        assert!(close(x, 4.0) && close(y, 9.0));
        assert!(
            Matrix::from_array([1.0, 2.0, 2.0, 4.0, 0.0, 0.0])
                .invert()
                .is_none()
        );
    }

    #[test]
    fn unit_extent_ignores_rotation() {
        let rot = Matrix::from_array([0.0, 100.0, -50.0, 0.0, 0.0, 0.0]);
        let (w, h) = rot.unit_extent();
        assert!(close(w, 100.0) && close(h, 50.0));
    }

    #[test]
    fn rect_operations() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, -5.0, 20.0, 5.0);
        assert_eq!(a.intersect(b), Rect::new(5.0, 0.0, 10.0, 5.0));
        assert_eq!(a.union(b), Rect::new(0.0, -5.0, 20.0, 10.0));
        assert!(a.intersect(Rect::new(20.0, 20.0, 30.0, 30.0)).is_empty());
        assert!(a.covers(Rect::new(0.0, 0.0, 10.0, 10.0)));
        assert!(!a.covers(b));
    }
}
