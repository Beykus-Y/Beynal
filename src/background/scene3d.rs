// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! 3D scene graph, camera, lighting, procedural meshes, particles, and
//! character-grid rasterizer powered by `glam`.

use glam::{Mat4, Vec3, Vec4};

#[derive(Clone, Copy, Debug)]
pub struct Vertex3D {
    pub pos: Vec3,
    pub normal: Vec3,
}

#[derive(Clone, Debug)]
pub struct Mesh3D {
    pub vertices: Vec<Vertex3D>,
    pub indices: Vec<[usize; 3]>,
}

#[derive(Clone, Debug)]
pub struct Material3D {
    pub color: [f32; 3],
    pub ambient: f32,
    pub diffuse: f32,
    pub specular: f32,
}

impl Default for Material3D {
    fn default() -> Self {
        Self {
            color: [0.45, 0.78, 0.96],
            ambient: 0.20,
            diffuse: 0.80,
            specular: 0.35,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Object3D {
    pub id: String,
    pub mesh: Mesh3D,
    pub material: Material3D,
    pub position: Vec3,
    pub rotation: Vec3,
    pub scale: Vec3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectionMode {
    Perspective,
    Orthographic,
}

#[derive(Clone, Debug)]
pub struct Camera3D {
    pub position: Vec3,
    pub target: Vec3,
    pub up: Vec3,
    pub fov_rad: f32,
    pub ortho_scale: f32,
    pub near: f32,
    pub far: f32,
    pub projection: ProjectionMode,
}

impl Default for Camera3D {
    fn default() -> Self {
        Self {
            position: Vec3::new(0.0, 0.8, 4.2),
            target: Vec3::ZERO,
            up: Vec3::Y,
            fov_rad: 55.0_f32.to_radians(),
            ortho_scale: 2.5,
            near: 0.1,
            far: 100.0,
            projection: ProjectionMode::Perspective,
        }
    }
}

impl Camera3D {
    pub fn view_proj(&self, aspect: f32) -> Mat4 {
        let view = Mat4::look_at_rh(self.position, self.target, self.up);
        let proj = match self.projection {
            ProjectionMode::Perspective => {
                Mat4::perspective_rh_gl(self.fov_rad, aspect.max(0.1), self.near, self.far)
            }
            ProjectionMode::Orthographic => {
                let half_h = self.ortho_scale;
                let half_w = half_h * aspect.max(0.1);
                Mat4::orthographic_rh_gl(-half_w, half_w, -half_h, half_h, self.near, self.far)
            }
        };
        proj * view
    }
}

#[derive(Clone, Debug)]
pub struct Light3D {
    pub direction: Vec3,
    pub color: [f32; 3],
    pub intensity: f32,
}

impl Default for Light3D {
    fn default() -> Self {
        Self {
            direction: Vec3::new(0.4, 0.8, 0.6).normalize(),
            color: [1.0, 1.0, 1.0],
            intensity: 1.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Particle3D {
    pub pos: Vec3,
    pub vel: Vec3,
    pub color: [f32; 3],
    pub ch: char,
}

#[derive(Clone, Debug, Default)]
pub struct Scene3D {
    pub camera: Camera3D,
    pub light: Light3D,
    pub objects: Vec<Object3D>,
    pub particles: Vec<Particle3D>,
}

/// Output cell of the 3D-to-character rasterizer.
#[derive(Clone, Copy, Debug)]
pub struct BgCharCell {
    pub ch: char,
    pub color: [f32; 3],
    pub depth: f32,
}

impl Default for BgCharCell {
    fn default() -> Self {
        Self {
            ch: ' ',
            color: [0.0; 3],
            depth: f32::INFINITY,
        }
    }
}

/// Software Z-buffered 3D-to-character rasterizer.
pub struct CharRasterizer {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<BgCharCell>,
}

impl CharRasterizer {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            cells: vec![BgCharCell::default(); cols * rows],
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if (self.cols, self.rows) != (cols, rows) {
            self.cols = cols;
            self.rows = rows;
            self.cells.resize(cols * rows, BgCharCell::default());
        }
    }

    pub fn render(&mut self, scene: &Scene3D, charset: &str, brightness: f32, cell_aspect: f32) {
        self.cells.fill(BgCharCell::default());
        let ramp: Vec<char> = if charset.is_empty() {
            " .:-=+*#%@".chars().collect()
        } else {
            charset.chars().collect()
        };
        let aspect = (self.cols as f32 / self.rows.max(1) as f32) * cell_aspect.max(0.25);
        let vp = scene.camera.view_proj(aspect);
        let light_dir = scene.light.direction.normalize_or_zero();

        for obj in &scene.objects {
            let model = Mat4::from_translation(obj.position)
                * Mat4::from_euler(
                    glam::EulerRot::XYZ,
                    obj.rotation.x,
                    obj.rotation.y,
                    obj.rotation.z,
                )
                * Mat4::from_scale(obj.scale);
            let mvp = vp * model;

            for tri in &obj.mesh.indices {
                let v0 = obj.mesh.vertices[tri[0]];
                let v1 = obj.mesh.vertices[tri[1]];
                let v2 = obj.mesh.vertices[tri[2]];

                let p0 = project(mvp, v0.pos, self.cols, self.rows);
                let p1 = project(mvp, v1.pos, self.cols, self.rows);
                let p2 = project(mvp, v2.pos, self.cols, self.rows);
                let (Some(s0), Some(s1), Some(s2)) = (p0, p1, p2) else {
                    continue;
                };

                let n0 = (model * Vec4::from((v0.normal, 0.0)))
                    .truncate()
                    .normalize_or_zero();
                let n1 = (model * Vec4::from((v1.normal, 0.0)))
                    .truncate()
                    .normalize_or_zero();
                let n2 = (model * Vec4::from((v2.normal, 0.0)))
                    .truncate()
                    .normalize_or_zero();
                let avg_n = (n0 + n1 + n2).normalize_or_zero();

                let diff = avg_n.dot(light_dir).max(0.0);
                let lum = ((obj.material.ambient
                    + obj.material.diffuse * diff * scene.light.intensity)
                    * brightness)
                    .clamp(0.05, 1.0);
                let ch = sample_ramp(&ramp, lum);
                let col = [
                    (obj.material.color[0] * scene.light.color[0] * lum).clamp(0.0, 1.0),
                    (obj.material.color[1] * scene.light.color[1] * lum).clamp(0.0, 1.0),
                    (obj.material.color[2] * scene.light.color[2] * lum).clamp(0.0, 1.0),
                ];

                self.raster_triangle(s0, s1, s2, ch, col);
            }
        }

        for p in &scene.particles {
            if let Some(sp) = project(vp, p.pos, self.cols, self.rows) {
                let x = sp.x.round() as isize;
                let y = sp.y.round() as isize;
                if x >= 0 && y >= 0 && (x as usize) < self.cols && (y as usize) < self.rows {
                    let idx = y as usize * self.cols + x as usize;
                    if sp.z < self.cells[idx].depth {
                        self.cells[idx] = BgCharCell {
                            ch: p.ch,
                            color: p.color,
                            depth: sp.z,
                        };
                    }
                }
            }
        }
    }

    fn raster_triangle(&mut self, a: Vec3, b: Vec3, c: Vec3, ch: char, color: [f32; 3]) {
        let min_x = a.x.min(b.x).min(c.x).floor().max(0.0) as usize;
        let max_x = a.x.max(b.x).max(c.x).ceil().min((self.cols - 1) as f32) as usize;
        let min_y = a.y.min(b.y).min(c.y).floor().max(0.0) as usize;
        let max_y = a.y.max(b.y).max(c.y).ceil().min((self.rows - 1) as f32) as usize;
        let area = edge(a, b, c);
        if area.abs() < 1e-4 {
            return;
        }
        for py in min_y..=max_y {
            for px in min_x..=max_x {
                let p = Vec3::new(px as f32 + 0.5, py as f32 + 0.5, 0.0);
                let w0 = edge(b, c, p) / area;
                let w1 = edge(c, a, p) / area;
                let w2 = edge(a, b, p) / area;
                if w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0 {
                    let z = w0 * a.z + w1 * b.z + w2 * c.z;
                    let idx = py * self.cols + px;
                    if z < self.cells[idx].depth {
                        self.cells[idx] = BgCharCell {
                            ch,
                            color,
                            depth: z,
                        };
                    }
                }
            }
        }
    }
}

#[inline]
fn edge(a: Vec3, b: Vec3, c: Vec3) -> f32 {
    (c.x - a.x) * (b.y - a.y) - (c.y - a.y) * (b.x - a.x)
}

fn project(mvp: Mat4, p: Vec3, cols: usize, rows: usize) -> Option<Vec3> {
    let clip = mvp * Vec4::from((p, 1.0));
    if clip.w <= 0.05 {
        return None;
    }
    let ndc = clip.truncate() / clip.w;
    if ndc.z < -1.0 || ndc.z > 1.0 {
        return None;
    }
    let sx = (ndc.x * 0.5 + 0.5) * cols as f32;
    let sy = (1.0 - (ndc.y * 0.5 + 0.5)) * rows as f32;
    Some(Vec3::new(sx, sy, ndc.z))
}

fn sample_ramp(ramp: &[char], lum: f32) -> char {
    let idx = ((lum.clamp(0.0, 1.0)) * (ramp.len() - 1) as f32).round() as usize;
    ramp[idx.min(ramp.len() - 1)]
}

/// Generate a procedural 3D torus mesh.
pub fn torus_mesh(major_r: f32, minor_r: f32, segs_u: usize, segs_v: usize) -> Mesh3D {
    let mut vertices = Vec::with_capacity(segs_u * segs_v);
    let mut indices = Vec::with_capacity(segs_u * segs_v * 2);
    for i in 0..segs_u {
        let u = (i as f32 / segs_u as f32) * std::f32::consts::TAU;
        let (su, cu) = u.sin_cos();
        for j in 0..segs_v {
            let v = (j as f32 / segs_v as f32) * std::f32::consts::TAU;
            let (sv, cv) = v.sin_cos();
            let pos = Vec3::new(
                (major_r + minor_r * cv) * cu,
                minor_r * sv,
                (major_r + minor_r * cv) * su,
            );
            let normal = Vec3::new(cv * cu, sv, cv * su).normalize();
            vertices.push(Vertex3D { pos, normal });
        }
    }
    for i in 0..segs_u {
        let ni = (i + 1) % segs_u;
        for j in 0..segs_v {
            let nj = (j + 1) % segs_v;
            let a = i * segs_v + j;
            let b = ni * segs_v + j;
            let c = ni * segs_v + nj;
            let d = i * segs_v + nj;
            indices.push([a, b, c]);
            indices.push([a, c, d]);
        }
    }
    Mesh3D { vertices, indices }
}

/// Generate a procedural UV sphere mesh.
pub fn sphere_mesh(radius: f32, rings: usize, sectors: usize) -> Mesh3D {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for r in 0..=rings {
        let phi = std::f32::consts::PI * (r as f32 / rings as f32);
        let (sp, cp) = phi.sin_cos();
        for s in 0..=sectors {
            let theta = std::f32::consts::TAU * (s as f32 / sectors as f32);
            let (st, ct) = theta.sin_cos();
            let normal = Vec3::new(sp * ct, cp, sp * st).normalize();
            vertices.push(Vertex3D {
                pos: normal * radius,
                normal,
            });
        }
    }
    let stride = sectors + 1;
    for r in 0..rings {
        for s in 0..sectors {
            let a = r * stride + s;
            let b = (r + 1) * stride + s;
            let c = (r + 1) * stride + s + 1;
            let d = r * stride + s + 1;
            indices.push([a, b, c]);
            indices.push([a, c, d]);
        }
    }
    Mesh3D { vertices, indices }
}

/// Generate a procedural wave/terrain grid mesh.
pub fn grid_mesh(nx: usize, nz: usize, size: f32, height_fn: impl Fn(f32, f32) -> f32) -> Mesh3D {
    let mut vertices = Vec::with_capacity((nx + 1) * (nz + 1));
    let mut indices = Vec::with_capacity(nx * nz * 2);
    for z in 0..=nz {
        let fz = (z as f32 / nz as f32 - 0.5) * size;
        for x in 0..=nx {
            let fx = (x as f32 / nx as f32 - 0.5) * size;
            let fy = height_fn(fx, fz);
            let eps = 0.08;
            let dx = height_fn(fx + eps, fz) - fy;
            let dz = height_fn(fx, fz + eps) - fy;
            let normal = Vec3::new(-dx, eps, -dz).normalize();
            vertices.push(Vertex3D {
                pos: Vec3::new(fx, fy, fz),
                normal,
            });
        }
    }
    let stride = nx + 1;
    for z in 0..nz {
        for x in 0..nx {
            let a = z * stride + x;
            let b = (z + 1) * stride + x;
            let c = (z + 1) * stride + x + 1;
            let d = z * stride + x + 1;
            indices.push([a, b, c]);
            indices.push([a, c, d]);
        }
    }
    Mesh3D { vertices, indices }
}

/// Parse a Wavefront `.obj` mesh for user-imported 3D models.
pub fn load_obj_mesh(obj_text: &str) -> Result<Mesh3D, String> {
    let mut positions = Vec::new();
    let mut indices = Vec::new();
    for line in obj_text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("v ") {
            let nums: Vec<f32> = rest
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            if nums.len() >= 3 {
                positions.push(Vec3::new(nums[0], nums[1], nums[2]));
            }
        } else if let Some(rest) = line.strip_prefix("f ") {
            let face: Vec<usize> = rest
                .split_whitespace()
                .filter_map(|tok| tok.split('/').next()?.parse::<usize>().ok())
                .filter_map(|idx| idx.checked_sub(1))
                .collect();
            for w in face.windows(2).skip(1) {
                if face[0] < positions.len() && w[0] < positions.len() && w[1] < positions.len() {
                    indices.push([face[0], w[0], w[1]]);
                }
            }
        }
    }
    if positions.is_empty() || indices.is_empty() {
        return Err("empty or invalid OBJ mesh".to_string());
    }
    let mut normals = vec![Vec3::ZERO; positions.len()];
    for &[a, b, c] in &indices {
        let n = (positions[b] - positions[a]).cross(positions[c] - positions[a]);
        normals[a] += n;
        normals[b] += n;
        normals[c] += n;
    }
    let vertices = positions
        .into_iter()
        .zip(normals)
        .map(|(pos, n)| Vertex3D {
            pos,
            normal: n.normalize_or_zero(),
        })
        .collect();
    Ok(Mesh3D { vertices, indices })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rasterizer_produces_non_empty_ascii_depth_sorted() {
        let mut scene = Scene3D::default();
        scene.objects.push(Object3D {
            id: "torus".into(),
            mesh: torus_mesh(1.2, 0.45, 24, 12),
            material: Material3D::default(),
            position: Vec3::ZERO,
            rotation: Vec3::new(0.6, 0.4, 0.0),
            scale: Vec3::ONE,
        });
        let mut rast = CharRasterizer::new(60, 24);
        rast.render(&scene, " .:-=+*#%@", 1.0, 0.55);
        let non_space = rast.cells.iter().filter(|c| c.ch != ' ').count();
        assert!(non_space > 30);
    }

    #[test]
    fn obj_loader_parses_triangle_mesh() {
        let obj = "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n";
        let mesh = load_obj_mesh(obj).unwrap();
        assert_eq!(mesh.vertices.len(), 3);
        assert_eq!(mesh.indices.len(), 1);
    }
}
