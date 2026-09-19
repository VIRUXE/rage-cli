//! Navmesh generation for an interior that has none: rasterise the walkable
//! floor of a collision mesh onto a grid, knock out everything a ped's body
//! would hit, merge what is left into rectangles, and append them to a cell
//! as interior polygons with proper edge adjacency.
//!
//! The polygons are deliberately simple (axis-aligned rectangles with extra
//! collinear vertices where neighbours meet): GTA's navmesh can't express a
//! T-junction — every polygon edge names exactly one neighbour — so shared
//! edges must match vertex for vertex.

use std::collections::HashMap;

use anyhow::{bail, Result};
use rage_formats::{NavEdge, NavPoly, Triangle, Vec3, Ynv, ADJACENT_NONE};

/// What to generate and where.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// World-space XY rectangle to work inside: x0, y0, x1, y1.
    pub clip: [f32; 4],
    /// Expected floor height; triangles far from it are not floor.
    pub floor_z: f32,
    /// Extra XY rectangles to treat as blocked (furniture whose collision
    /// isn't in the file).
    pub blocks: Vec<[f32; 4]>,
    /// Grid step in metres.
    pub grid: f32,
    /// Longest rectangle side in metres.
    pub max_side: f32,
    /// Body slab above the floor that must be clear: low and high edge.
    pub body_low: f32,
    pub body_high: f32,
    /// Floor may be this far below / above `floor_z`.
    pub floor_below: f32,
    pub floor_above: f32,
    /// Minimum `normal.z` for a triangle to count as floor.
    pub walkable_slope: f32,
    /// Vanilla polygons whose centroid is inside the clip box and within
    /// this z range of `floor_z` are sunk and cut off from their neighbours.
    pub sink_below: f32,
    pub sink_above: f32,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            clip: [0.0; 4],
            floor_z: 0.0,
            blocks: Vec::new(),
            grid: 0.25,
            max_side: 3.0,
            body_low: 0.15,
            body_high: 0.45,
            floor_below: 0.3,
            floor_above: 0.6,
            walkable_slope: 0.7,
            sink_below: 1.5,
            sink_above: 2.5,
        }
    }
}

/// Numbers worth printing after a build.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BuildReport {
    pub grid_cells: usize,
    pub floor_cells: usize,
    pub blocked_cells: usize,
    pub free_cells: usize,
    pub rectangles: usize,
    pub new_polys: usize,
    pub sunk_polys: usize,
    pub internal_edges: usize,
}

/// Runs the generator against `cell`, appending the new polygons.
pub fn build(cell: &mut Ynv, triangles: &[Triangle], opts: &BuildOptions) -> Result<BuildReport> {
    let grid = Grid::new(opts)?;
    let mut report = BuildReport { grid_cells: grid.nx * grid.ny, ..Default::default() };

    let mut floor = grid.sample_floor(triangles, opts);
    report.floor_cells = floor.iter().filter(|c| c.is_some()).count();
    let blocked = grid.blocked_cells(&floor, triangles, opts);
    report.blocked_cells = blocked.iter().filter(|&&b| b).count();
    for (f, &b) in floor.iter_mut().zip(&blocked) {
        if b { *f = None; }
    }
    report.free_cells = floor.iter().filter(|c| c.is_some()).count();
    if report.free_cells == 0 {
        bail!("no walkable floor inside the clip box (floor z {} ± {}/{})", opts.floor_z, opts.floor_below, opts.floor_above);
    }

    let rects = grid.rectangles(&floor, opts);
    report.rectangles = rects.len();
    let polys = grid.polygons(&rects, &floor, opts.floor_z);

    report.sunk_polys = sink_vanilla(cell, opts);

    let first = cell.polys.len();
    let mut new_polys: Vec<NavPoly> = polys.into_iter().map(|verts| {
        let mut p = NavPoly::new(verts);
        p.set_interior(true);
        p.set_flat_ground(true);
        p
    }).collect();
    report.internal_edges = link_edges(&mut new_polys, first, cell.area_id);
    report.new_polys = new_polys.len();
    cell.polys.extend(new_polys);
    if cell.polys.len() > 0x3FFF {
        bail!("cell would hold {} polygons; the format allows 16383", cell.polys.len());
    }
    Ok(report)
}

// ─── grid ────────────────────────────────────────────────────────────────────

struct Grid {
    x0: f32,
    y0: f32,
    g: f32,
    nx: usize,
    ny: usize,
}

/// A rectangle of free cells, in cell indices, inclusive-exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    i0: usize,
    j0: usize,
    i1: usize,
    j1: usize,
}

impl Grid {
    fn new(opts: &BuildOptions) -> Result<Self> {
        let [x0, y0, x1, y1] = opts.clip;
        if !(x1 > x0 && y1 > y0) {
            bail!("clip box must be x0,y0,x1,y1 with x1 > x0 and y1 > y0");
        }
        if !(opts.grid > 0.01) {
            bail!("grid step must be positive");
        }
        let nx = ((x1 - x0) / opts.grid).ceil() as usize;
        let ny = ((y1 - y0) / opts.grid).ceil() as usize;
        if nx * ny > 4_000_000 {
            bail!("clip box is {nx}×{ny} cells; use a coarser grid or a smaller box");
        }
        Ok(Self { x0, y0, g: opts.grid, nx, ny })
    }

    fn centre(&self, i: usize, j: usize) -> (f32, f32) {
        (self.x0 + (i as f32 + 0.5) * self.g, self.y0 + (j as f32 + 0.5) * self.g)
    }

    /// Cell index range covering an XY box, clamped to the grid.
    fn cells_in(&self, min: Vec3, max: Vec3) -> Option<(usize, usize, usize, usize)> {
        let i0 = ((min.x - self.x0) / self.g).floor().max(0.0) as usize;
        let j0 = ((min.y - self.y0) / self.g).floor().max(0.0) as usize;
        let i1 = (((max.x - self.x0) / self.g).ceil().max(0.0) as usize).min(self.nx);
        let j1 = (((max.y - self.y0) / self.g).ceil().max(0.0) as usize).min(self.ny);
        (i0 < i1 && j0 < j1).then_some((i0, j0, i1, j1))
    }

    /// Highest floor height under each cell centre, or `None` where no
    /// walkable triangle near `floor_z` covers it.
    fn sample_floor(&self, triangles: &[Triangle], opts: &BuildOptions) -> Vec<Option<f32>> {
        let mut floor = vec![None; self.nx * self.ny];
        let zlo = opts.floor_z - opts.floor_below;
        let zhi = opts.floor_z + opts.floor_above;
        for tri in triangles {
            if tri.normal().z < opts.walkable_slope { continue; }
            let (min, max) = tri_bounds(tri);
            if max.z < zlo || min.z > zhi { continue; }
            let Some((i0, j0, i1, j1)) = self.cells_in(min, max) else { continue };
            for j in j0..j1 {
                for i in i0..i1 {
                    let (cx, cy) = self.centre(i, j);
                    if let Some(z) = height_at(tri, cx, cy) {
                        if z < zlo || z > zhi { continue; }
                        let slot = &mut floor[j * self.nx + i];
                        if slot.is_none_or(|old| z > old) { *slot = Some(z); }
                    }
                }
            }
        }
        floor
    }

    /// Cells whose body slab is intersected by any triangle, or that fall in
    /// a `--block` rectangle, dilated by one cell.
    fn blocked_cells(&self, floor: &[Option<f32>], triangles: &[Triangle], opts: &BuildOptions) -> Vec<bool> {
        let mut blocked = vec![false; self.nx * self.ny];
        let slab_lo = opts.floor_z - opts.floor_below + opts.body_low;
        let slab_hi = opts.floor_z + opts.floor_above + opts.body_high;
        let half = self.g * 0.5;
        for tri in triangles {
            let (min, max) = tri_bounds(tri);
            if max.z < slab_lo || min.z > slab_hi { continue; }
            let Some((i0, j0, i1, j1)) = self.cells_in(min, max) else { continue };
            for j in j0..j1 {
                for i in i0..i1 {
                    let idx = j * self.nx + i;
                    if blocked[idx] { continue; }
                    let Some(f) = floor[idx] else { continue };
                    let (cx, cy) = self.centre(i, j);
                    let centre = Vec3::new(cx, cy, f + (opts.body_low + opts.body_high) * 0.5);
                    let half_size = Vec3::new(half, half, (opts.body_high - opts.body_low) * 0.5);
                    if tri_box_overlap(centre, half_size, &tri.vertices) {
                        blocked[idx] = true;
                    }
                }
            }
        }
        for b in &opts.blocks {
            let Some((i0, j0, i1, j1)) = self.cells_in(Vec3::new(b[0], b[1], 0.0), Vec3::new(b[2], b[3], 0.0)) else { continue };
            for j in j0..j1 {
                for i in i0..i1 {
                    blocked[j * self.nx + i] = true;
                }
            }
        }
        // Dilate by one cell so a polygon edge never touches an obstacle.
        let mut dilated = blocked.clone();
        for j in 0..self.ny {
            for i in 0..self.nx {
                if !blocked[j * self.nx + i] { continue; }
                for dj in -1i64..=1 {
                    for di in -1i64..=1 {
                        let (ii, jj) = (i as i64 + di, j as i64 + dj);
                        if ii >= 0 && jj >= 0 && (ii as usize) < self.nx && (jj as usize) < self.ny {
                            dilated[jj as usize * self.nx + ii as usize] = true;
                        }
                    }
                }
            }
        }
        dilated
    }

    /// Greedy row-major merge of free cells into rectangles.
    fn rectangles(&self, floor: &[Option<f32>], opts: &BuildOptions) -> Vec<Rect> {
        let max_cells = ((opts.max_side / self.g).floor() as usize).max(1);
        let mut taken = vec![false; self.nx * self.ny];
        let free = |i: usize, j: usize, taken: &[bool]| floor[j * self.nx + i].is_some() && !taken[j * self.nx + i];
        let mut rects = Vec::new();
        for j in 0..self.ny {
            for i in 0..self.nx {
                if !free(i, j, &taken) { continue; }
                let mut i1 = i + 1;
                while i1 < self.nx && i1 - i < max_cells && free(i1, j, &taken) { i1 += 1; }
                let mut j1 = j + 1;
                'rows: while j1 < self.ny && j1 - j < max_cells {
                    for ii in i..i1 {
                        if !free(ii, j1, &taken) { break 'rows; }
                    }
                    j1 += 1;
                }
                for jj in j..j1 {
                    for ii in i..i1 {
                        taken[jj * self.nx + ii] = true;
                    }
                }
                rects.push(Rect { i0: i, j0: j, i1, j1 });
            }
        }
        rects
    }

    /// Floor height at a grid point: mean of the free cells around it.
    fn z_at_point(&self, floor: &[Option<f32>], i: usize, j: usize, fallback: f32) -> f32 {
        let mut sum = 0.0;
        let mut n = 0;
        for (di, dj) in [(0i64, 0i64), (-1, 0), (0, -1), (-1, -1)] {
            let (ii, jj) = (i as i64 + di, j as i64 + dj);
            if ii < 0 || jj < 0 || ii as usize >= self.nx || jj as usize >= self.ny { continue; }
            if let Some(z) = floor[jj as usize * self.nx + ii as usize] {
                sum += z;
                n += 1;
            }
        }
        if n == 0 { fallback } else { sum / n as f32 }
    }

    /// Counter-clockwise polygons for the rectangles, with every corner of a
    /// neighbouring rectangle that lies on an edge inserted into that edge.
    fn polygons(&self, rects: &[Rect], floor: &[Option<f32>], fallback_z: f32) -> Vec<Vec<Vec3>> {
        // Corner points per grid line, so edge splitting is exact integer work.
        let mut on_x_line: HashMap<usize, Vec<usize>> = HashMap::new(); // x = i -> js
        let mut on_y_line: HashMap<usize, Vec<usize>> = HashMap::new(); // y = j -> is
        for r in rects {
            for (i, j) in [(r.i0, r.j0), (r.i1, r.j0), (r.i1, r.j1), (r.i0, r.j1)] {
                on_x_line.entry(i).or_default().push(j);
                on_y_line.entry(j).or_default().push(i);
            }
        }
        for v in on_x_line.values_mut() { v.sort_unstable(); v.dedup(); }
        for v in on_y_line.values_mut() { v.sort_unstable(); v.dedup(); }

        let point = |i: usize, j: usize| Vec3::new(
            self.x0 + i as f32 * self.g, self.y0 + j as f32 * self.g, self.z_at_point(floor, i, j, fallback_z),
        );

        rects.iter().map(|r| {
            let mut verts = Vec::new();
            // bottom edge: (i0,j0) -> (i1,j0), increasing i
            verts.push(point(r.i0, r.j0));
            for &i in on_y_line[&r.j0].iter().filter(|&&i| i > r.i0 && i < r.i1) { verts.push(point(i, r.j0)); }
            // right edge: (i1,j0) -> (i1,j1), increasing j
            verts.push(point(r.i1, r.j0));
            for &j in on_x_line[&r.i1].iter().filter(|&&j| j > r.j0 && j < r.j1) { verts.push(point(r.i1, j)); }
            // top edge: (i1,j1) -> (i0,j1), decreasing i
            verts.push(point(r.i1, r.j1));
            for &i in on_y_line[&r.j1].iter().rev().filter(|&&i| i > r.i0 && i < r.i1) { verts.push(point(i, r.j1)); }
            // left edge: (i0,j1) -> (i0,j0), decreasing j
            verts.push(point(r.i0, r.j1));
            for &j in on_x_line[&r.i0].iter().rev().filter(|&&j| j > r.j0 && j < r.j1) { verts.push(point(r.i0, j)); }
            verts
        }).collect()
    }
}

/// Links every shared edge between the new polygons (indices start at
/// `first` in the cell) and returns how many were linked.
fn link_edges(polys: &mut [NavPoly], first: usize, area_id: u32) -> usize {
    let key = |v: Vec3| ((v.x * 1000.0).round() as i64, (v.y * 1000.0).round() as i64);
    let mut by_edge: HashMap<((i64, i64), (i64, i64)), (usize, usize)> = HashMap::new();
    for (pi, p) in polys.iter().enumerate() {
        let n = p.vertices.len();
        for ei in 0..n {
            by_edge.insert((key(p.vertices[ei]), key(p.vertices[(ei + 1) % n])), (pi, ei));
        }
    }
    let mut linked = 0;
    for pi in 0..polys.len() {
        let n = polys[pi].vertices.len();
        for ei in 0..n {
            let a = key(polys[pi].vertices[ei]);
            let b = key(polys[pi].vertices[(ei + 1) % n]);
            if let Some(&(qi, _)) = by_edge.get(&(b, a)) {
                polys[pi].edges[ei] = NavEdge::neighbour(area_id, (first + qi) as u32);
                linked += 1;
            }
        }
    }
    linked
}

/// Sinks vanilla polygons under the interior and cuts every edge to and
/// from them, so the pathfinder can neither reach nor snap to them.
fn sink_vanilla(cell: &mut Ynv, opts: &BuildOptions) -> usize {
    let [x0, y0, x1, y1] = opts.clip;
    let zlo = opts.floor_z - opts.sink_below;
    let zhi = opts.floor_z + opts.sink_above;
    let sunk: Vec<usize> = cell.polys.iter().enumerate()
        .filter(|(_, p)| {
            let c = p.centroid();
            c.x >= x0 && c.x <= x1 && c.y >= y0 && c.y <= y1 && c.z >= zlo && c.z <= zhi
        })
        .map(|(i, _)| i)
        .collect();
    if sunk.is_empty() {
        return 0;
    }
    let is_sunk: std::collections::HashSet<u32> = sunk.iter().map(|&i| i as u32).collect();
    let area = cell.area_id;
    let floor_z = cell.bb_min.z;
    for (i, p) in cell.polys.iter_mut().enumerate() {
        if is_sunk.contains(&(i as u32)) {
            for v in &mut p.vertices { v.z = floor_z; }
            for e in &mut p.edges { *e = NavEdge::NONE; }
            p.portal_links.clear();
        } else {
            for e in &mut p.edges {
                if e.a.area_id == area && is_sunk.contains(&e.a.poly) && e.a.poly != ADJACENT_NONE { e.a = rage_formats::NavEdgeEnd::NONE; }
                if e.b.area_id == area && is_sunk.contains(&e.b.poly) && e.b.poly != ADJACENT_NONE { e.b = rage_formats::NavEdgeEnd::NONE; }
            }
        }
    }
    cell.portals.retain(|p| !is_sunk.contains(&(p.poly_from as u32)) && !is_sunk.contains(&(p.poly_to as u32)));
    sunk.len()
}

// ─── geometry ────────────────────────────────────────────────────────────────

fn tri_bounds(t: &Triangle) -> (Vec3, Vec3) {
    let [a, b, c] = t.vertices;
    (a.min(b).min(c), a.max(b).max(c))
}

/// Height of the triangle's plane at (x, y) if the point is inside it in XY.
fn height_at(t: &Triangle, x: f32, y: f32) -> Option<f32> {
    let [a, b, c] = t.vertices;
    let d = (b.y - c.y) * (a.x - c.x) + (c.x - b.x) * (a.y - c.y);
    if d.abs() < 1e-9 { return None; }
    let l0 = ((b.y - c.y) * (x - c.x) + (c.x - b.x) * (y - c.y)) / d;
    let l1 = ((c.y - a.y) * (x - c.x) + (a.x - c.x) * (y - c.y)) / d;
    let l2 = 1.0 - l0 - l1;
    let eps = -1e-4;
    if l0 < eps || l1 < eps || l2 < eps { return None; }
    Some(l0 * a.z + l1 * b.z + l2 * c.z)
}

/// Akenine-Möller triangle/AABB overlap (separating axis theorem).
fn tri_box_overlap(centre: Vec3, half: Vec3, tri: &[Vec3; 3]) -> bool {
    let v0 = tri[0] - centre;
    let v1 = tri[1] - centre;
    let v2 = tri[2] - centre;
    let e0 = v1 - v0;
    let e1 = v2 - v1;
    let e2 = v0 - v2;

    let axis_test = |axis: Vec3| -> bool {
        let p0 = axis.dot(v0);
        let p1 = axis.dot(v1);
        let p2 = axis.dot(v2);
        let r = half.x * axis.x.abs() + half.y * axis.y.abs() + half.z * axis.z.abs();
        let min = p0.min(p1).min(p2);
        let max = p0.max(p1).max(p2);
        !(min > r || max < -r)
    };

    // 9 cross-product axes
    let box_axes = [Vec3::new(1.0, 0.0, 0.0), Vec3::new(0.0, 1.0, 0.0), Vec3::new(0.0, 0.0, 1.0)];
    for e in [e0, e1, e2] {
        for b in box_axes {
            let axis = e.cross(b);
            if axis.length() < 1e-12 { continue; }
            if !axis_test(axis) { return false; }
        }
    }
    // 3 box face normals
    for b in box_axes {
        if !axis_test(b) { return false; }
    }
    // triangle normal
    let n = e0.cross(e1);
    if n.length() > 1e-12 && !axis_test(n) { return false; }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quad(x0: f32, y0: f32, x1: f32, y1: f32, z: f32, out: &mut Vec<Triangle>) {
        let a = Vec3::new(x0, y0, z);
        let b = Vec3::new(x1, y0, z);
        let c = Vec3::new(x1, y1, z);
        let d = Vec3::new(x0, y1, z);
        out.push(Triangle { vertices: [a, b, c], material: 0 });
        out.push(Triangle { vertices: [a, c, d], material: 0 });
    }

    /// A vertical wall along x from (x0,y) to (x1,y), z0..z1.
    fn wall(x0: f32, x1: f32, y: f32, z0: f32, z1: f32, out: &mut Vec<Triangle>) {
        let a = Vec3::new(x0, y, z0);
        let b = Vec3::new(x1, y, z0);
        let c = Vec3::new(x1, y, z1);
        let d = Vec3::new(x0, y, z1);
        out.push(Triangle { vertices: [a, b, c], material: 0 });
        out.push(Triangle { vertices: [a, c, d], material: 0 });
    }

    fn box_(x0: f32, y0: f32, x1: f32, y1: f32, z0: f32, z1: f32, out: &mut Vec<Triangle>) {
        quad(x0, y0, x1, y1, z1, out);
        wall(x0, x1, y0, z0, z1, out);
        wall(x0, x1, y1, z0, z1, out);
        // side walls along y
        for x in [x0, x1] {
            let a = Vec3::new(x, y0, z0);
            let b = Vec3::new(x, y1, z0);
            let c = Vec3::new(x, y1, z1);
            let d = Vec3::new(x, y0, z1);
            out.push(Triangle { vertices: [a, b, c], material: 0 });
            out.push(Triangle { vertices: [a, c, d], material: 0 });
        }
    }

    fn room() -> (Vec<Triangle>, BuildOptions) {
        let mut tris = Vec::new();
        quad(0.0, 0.0, 10.0, 6.0, 0.0, &mut tris);          // floor
        wall(0.0, 10.0, 0.0, 0.0, 3.0, &mut tris);            // south wall
        wall(0.0, 10.0, 6.0, 0.0, 3.0, &mut tris);            // north wall
        box_(4.0, 2.0, 6.0, 4.0, 0.0, 0.8, &mut tris);        // a table in the middle
        quad(0.0, 0.0, 10.0, 6.0, 3.0, &mut tris);            // ceiling (must not count as floor)
        let opts = BuildOptions { clip: [0.0, 0.0, 10.0, 6.0], floor_z: 0.0, grid: 0.5, max_side: 3.0, ..Default::default() };
        (tris, opts)
    }

    #[test]
    fn tri_box_overlap_detects_walls_and_ignores_far_triangles() {
        let wall = [Vec3::new(0.0, 1.0, 0.0), Vec3::new(2.0, 1.0, 0.0), Vec3::new(2.0, 1.0, 3.0)];
        assert!(tri_box_overlap(Vec3::new(1.0, 1.0, 0.3), Vec3::new(0.25, 0.25, 0.15), &wall));
        assert!(!tri_box_overlap(Vec3::new(1.0, 2.0, 0.3), Vec3::new(0.25, 0.25, 0.15), &wall));
        assert!(!tri_box_overlap(Vec3::new(1.0, 1.0, 5.0), Vec3::new(0.25, 0.25, 0.15), &wall));
    }

    #[test]
    fn height_at_interpolates_inside_and_rejects_outside() {
        let t = Triangle { vertices: [Vec3::new(0.0, 0.0, 0.0), Vec3::new(2.0, 0.0, 2.0), Vec3::new(0.0, 2.0, 0.0)], material: 0 };
        assert!((height_at(&t, 1.0, 0.5).unwrap() - 1.0).abs() < 1e-5);
        assert!(height_at(&t, 3.0, 3.0).is_none());
    }

    #[test]
    fn room_becomes_linked_interior_polygons_around_the_table() {
        let (tris, opts) = room();
        let mut cell = Ynv::new_cell(1, Vec3::new(-10.0, -10.0, -10.0), Vec3::new(140.0, 140.0, 40.0));
        let report = build(&mut cell, &tris, &opts).unwrap();
        assert!(report.new_polys > 4, "{report:?}");
        assert_eq!(report.new_polys, cell.polys.len());
        assert!(cell.polys.iter().all(|p| p.is_interior() && p.is_flat_ground()));
        // Nothing under the table or in the walls.
        for p in &cell.polys {
            let c = p.centroid();
            assert!(!(c.x > 3.5 && c.x < 6.5 && c.y > 1.5 && c.y < 4.5), "polygon under the table at {c:?}");
            assert!(c.y > 0.4 && c.y < 5.6, "polygon in a wall at {c:?}");
            assert!(c.z.abs() < 0.01);
        }
        // Every internal edge is linked both ways.
        for (pi, p) in cell.polys.iter().enumerate() {
            for (ei, e) in p.edges.iter().enumerate() {
                if e.a.is_none() { continue; }
                let q = &cell.polys[e.a.poly as usize];
                assert!(q.edges.iter().any(|f| f.a.poly == pi as u32), "edge {ei} of {pi} not linked back");
            }
        }
        assert!(report.internal_edges > 0);
        // The mesh is connected: flood fill from polygon 0 reaches all.
        let mut seen = vec![false; cell.polys.len()];
        let mut stack = vec![0usize];
        while let Some(i) = stack.pop() {
            if seen[i] { continue; }
            seen[i] = true;
            for e in &cell.polys[i].edges {
                if !e.a.is_none() { stack.push(e.a.poly as usize); }
            }
        }
        assert!(seen.iter().all(|&s| s), "disconnected polygons");
        // And it survives a write/read.
        let back = rage_formats::parse_ynv(&rage_formats::serialize_ynv(&cell).unwrap()).unwrap();
        assert_eq!(back.polys.len(), cell.polys.len());
    }

    #[test]
    fn vanilla_polys_under_the_floor_are_sunk_and_cut_off() {
        let (tris, opts) = room();
        let mut cell = Ynv::new_cell(1, Vec3::new(-10.0, -10.0, -10.0), Vec3::new(140.0, 140.0, 40.0));
        let mut old = NavPoly::new(vec![Vec3::new(1.0, 1.0, -0.5), Vec3::new(2.0, 1.0, -0.5), Vec3::new(2.0, 2.0, -0.5)]);
        old.edges[0] = NavEdge::neighbour(1, 1);
        let mut outside = NavPoly::new(vec![Vec3::new(20.0, 1.0, -0.5), Vec3::new(21.0, 1.0, -0.5), Vec3::new(21.0, 2.0, -0.5)]);
        outside.edges[1] = NavEdge::neighbour(1, 0);
        cell.polys = vec![old, outside];
        let report = build(&mut cell, &tris, &opts).unwrap();
        assert_eq!(report.sunk_polys, 1);
        assert!(cell.polys[0].vertices.iter().all(|v| v.z == -10.0));
        assert!(cell.polys[0].edges.iter().all(|e| e.a.is_none()));
        assert!(cell.polys[1].edges[1].a.is_none(), "neighbour still points at the sunk polygon");
        assert!(cell.polys[1].vertices[0].z == -0.5);
    }

    #[test]
    fn block_rectangles_remove_floor() {
        let (tris, mut opts) = room();
        opts.blocks.push([0.0, 0.0, 10.0, 6.0]);
        let mut cell = Ynv::new_cell(1, Vec3::new(-10.0, -10.0, -10.0), Vec3::new(140.0, 140.0, 40.0));
        assert!(build(&mut cell, &tris, &opts).is_err());
    }
}
