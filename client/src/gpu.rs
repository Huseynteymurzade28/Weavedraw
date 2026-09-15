//! GPU-resident stroke rendering through an `egui` paint callback.
//!
//! Committed strokes are grouped, in draw order, into chunks of
//! [`CHUNK_SIZE`]. Each chunk owns a vertex/index buffer holding the
//! tessellated meshes from [`TessCache`] and is identified by a hash of the
//! stroke ids it contains, so a frame in which nothing changed uploads
//! nothing and costs one `glDrawElements` per visible chunk. Adding a
//! stroke touches only the last chunk; erasing one re-uploads the chunks
//! after it (their membership shifts) but never re-tessellates.
//!
//! The vertex format is `epaint::Vertex` verbatim (position, unused uv,
//! premultiplied sRGB colour) and the shader mirrors `egui_glow`'s, so the
//! result is pixel-identical to letting egui draw the same meshes.

use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use common::{Point, Stroke, StrokeId, StrokeSet};
use eframe::egui_glow::{CallbackFn, ShaderVersion};
use eframe::glow::{self, HasContext};
use egui::epaint::{Mesh, Vertex};
use egui::{PaintCallback, Rect, Shape};
use tracing::{debug, warn};

use crate::canvas::{Camera, TessCache, overlaps};

pub const CHUNK_SIZE: usize = 64;

/// One GPU buffer pair plus what is needed to place and cull it.
struct Chunk {
    key: u64,
    /// World point at the buffer's local origin.
    origin: Point,
    bounds: (Point, Point),
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    ebo: glow::Buffer,
    index_count: i32,
}

/// Everything the paint callback needs; shared with it behind a mutex
/// because `CallbackFn` must be `Send + Sync + 'static`.
struct Gpu {
    gl: Arc<glow::Context>,
    program: glow::Program,
    u_screen_size: glow::UniformLocation,
    u_offset: glow::UniformLocation,
    u_scale: glow::UniformLocation,
    chunks: Vec<Chunk>,
}

pub struct StrokeRenderer {
    tess: TessCache,
    gpu: Arc<Mutex<Gpu>>,
    scratch: Mesh,
    /// Document generation the chunks were last synced against.
    synced: Option<u64>,
}

const VERT_SRC: &str = r#"
uniform vec2 u_screen_size;
uniform vec2 u_offset;
uniform float u_scale;
I vec2 a_pos;
I vec4 a_srgba;
O vec4 v_rgba_in_gamma;
void main() {
    vec2 p = u_offset + a_pos * u_scale;
    gl_Position = vec4(2.0 * p.x / u_screen_size.x - 1.0,
                       1.0 - 2.0 * p.y / u_screen_size.y, 0.0, 1.0);
    v_rgba_in_gamma = a_srgba / 255.0;
}
"#;

const FRAG_SRC: &str = r#"
#if NEW_SHADER_INTERFACE
    in vec4 v_rgba_in_gamma;
    out vec4 f_color;
    #define gl_FragColor f_color
#else
    varying vec4 v_rgba_in_gamma;
#endif
void main() {
    // Premultiplied sRGB straight out, exactly like egui's own meshes.
    gl_FragColor = v_rgba_in_gamma;
}
"#;

fn shader_header(version: ShaderVersion) -> String {
    let mut h = version.version_declaration().to_owned();
    let new = version.is_new_shader_interface();
    h.push_str(&format!("#define NEW_SHADER_INTERFACE {}\n", u8::from(new)));
    if new {
        h.push_str("#define I in\n#define O out\n");
    } else {
        h.push_str("#define I attribute\n#define O varying\n");
    }
    if version.is_embedded() {
        h.push_str("precision highp float;\n");
    }
    h
}

impl StrokeRenderer {
    pub fn new(gl: Arc<glow::Context>) -> Result<Self, String> {
        let version = ShaderVersion::get(&gl);
        let header = shader_header(version);
        // SAFETY: plain GL object creation on the context eframe gave us,
        // called from the UI thread that owns it.
        let program = unsafe {
            let program = gl.create_program()?;
            let mut shaders = Vec::new();
            for (kind, src) in [
                (glow::VERTEX_SHADER, VERT_SRC),
                (glow::FRAGMENT_SHADER, FRAG_SRC),
            ] {
                let shader = gl.create_shader(kind)?;
                gl.shader_source(shader, &format!("{header}{src}"));
                gl.compile_shader(shader);
                if !gl.get_shader_compile_status(shader) {
                    let log = gl.get_shader_info_log(shader);
                    gl.delete_shader(shader);
                    gl.delete_program(program);
                    return Err(format!("shader compile failed: {log}"));
                }
                gl.attach_shader(program, shader);
                shaders.push(shader);
            }
            gl.link_program(program);
            for shader in shaders {
                gl.detach_shader(program, shader);
                gl.delete_shader(shader);
            }
            if !gl.get_program_link_status(program) {
                let log = gl.get_program_info_log(program);
                gl.delete_program(program);
                return Err(format!("program link failed: {log}"));
            }
            program
        };
        let uniform = |name: &str| unsafe {
            gl.get_uniform_location(program, name)
                .ok_or_else(|| format!("missing uniform {name}"))
        };
        let gpu = Gpu {
            u_screen_size: uniform("u_screen_size")?,
            u_offset: uniform("u_offset")?,
            u_scale: uniform("u_scale")?,
            gl,
            program,
            chunks: Vec::new(),
        };
        debug!(?version, "stroke renderer ready");
        Ok(Self {
            tess: TessCache::default(),
            gpu: Arc::new(Mutex::new(gpu)),
            scratch: Mesh::default(),
            synced: None,
        })
    }

    /// Bring the GPU buffers in line with the document. Call once per frame
    /// before [`Self::paint`]. `generation` must change whenever the visible
    /// set of strokes does; while it (and the zoom bucket) stay the same the
    /// call returns without touching a single stroke.
    pub fn sync(&mut self, ctx: &egui::Context, doc: &StrokeSet, zoom: f32, generation: u64) {
        let mut gpu = self.gpu.lock().unwrap_or_else(|e| e.into_inner());
        let invalidated = self.tess.begin_frame(ctx, zoom);
        if invalidated {
            gpu.drop_chunks();
        } else if self.synced == Some(generation) {
            return;
        }
        self.synced = Some(generation);
        let zoom = self.tess.zoom();

        let strokes: Vec<&Stroke> = doc.visible().collect();
        let mut chunk_index = 0;
        for group in strokes.chunks(CHUNK_SIZE) {
            let key = chunk_key(group.iter().map(|s| s.id));
            if gpu.chunks.get(chunk_index).is_none_or(|c| c.key != key) {
                self.scratch.clear();
                let mut origin = None;
                let mut bounds: Option<(Point, Point)> = None;
                for stroke in group {
                    let Some(cached) = self.tess.get(stroke) else {
                        continue;
                    };
                    let origin = *origin.get_or_insert(cached.bounds.0);
                    bounds = Some(match bounds {
                        None => cached.bounds,
                        Some((lo, hi)) => (
                            Point::new(lo.x.min(cached.bounds.0.x), lo.y.min(cached.bounds.0.y)),
                            Point::new(hi.x.max(cached.bounds.1.x), hi.y.max(cached.bounds.1.y)),
                        ),
                    });
                    let shift = egui::vec2(
                        (cached.bounds.0.x - origin.x) * zoom,
                        (cached.bounds.0.y - origin.y) * zoom,
                    );
                    let base = self.scratch.vertices.len() as u32;
                    self.scratch
                        .vertices
                        .extend(cached.mesh.vertices.iter().map(|v| Vertex {
                            pos: v.pos + shift,
                            ..*v
                        }));
                    self.scratch
                        .indices
                        .extend(cached.mesh.indices.iter().map(|i| i + base));
                }
                let (origin, bounds) = match (origin, bounds) {
                    (Some(o), Some(b)) => (o, b),
                    _ => (Point::ZERO, (Point::ZERO, Point::ZERO)),
                };
                gpu.upload(chunk_index, key, origin, bounds, &self.scratch);
            } else {
                // Unchanged chunk: still mark its meshes live so they survive
                // pruning and are ready if a later chunk shifts.
                for stroke in group {
                    self.tess.get(stroke);
                }
            }
            chunk_index += 1;
        }
        gpu.truncate(chunk_index);
        self.tess.end_frame();
    }

    /// The shape that draws every visible chunk into `rect`.
    pub fn paint(&self, rect: Rect, cam: Camera) -> Shape {
        let gpu = self.gpu.clone();
        let cache_zoom = self.tess.zoom();
        let view = cam.world_bounds(rect);
        Shape::Callback(PaintCallback {
            rect,
            callback: Arc::new(CallbackFn::new(move |info, _painter| {
                let gpu = gpu.lock().unwrap_or_else(|e| e.into_inner());
                // SAFETY: egui_glow invokes us on the GL thread with the
                // viewport set to `rect`; we touch only our own objects and
                // egui restores its state afterwards.
                unsafe {
                    let gl = &gpu.gl;
                    gl.use_program(Some(gpu.program));
                    gl.disable(glow::CULL_FACE);
                    gl.disable(glow::DEPTH_TEST);
                    gl.enable(glow::BLEND);
                    gl.blend_equation_separate(glow::FUNC_ADD, glow::FUNC_ADD);
                    gl.blend_func_separate(
                        glow::ONE,
                        glow::ONE_MINUS_SRC_ALPHA,
                        glow::ONE_MINUS_DST_ALPHA,
                        glow::ONE,
                    );
                    let size = info.viewport.size();
                    gl.uniform_2_f32(Some(&gpu.u_screen_size), size.x, size.y);
                    gl.uniform_1_f32(Some(&gpu.u_scale), cam.zoom / cache_zoom);
                    for chunk in &gpu.chunks {
                        if chunk.index_count == 0 || !overlaps(chunk.bounds, view) {
                            continue;
                        }
                        let at = cam.to_screen(rect, chunk.origin) - rect.min;
                        gl.uniform_2_f32(Some(&gpu.u_offset), at.x, at.y);
                        gl.bind_vertex_array(Some(chunk.vao));
                        gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(chunk.ebo));
                        gl.draw_elements(glow::TRIANGLES, chunk.index_count, glow::UNSIGNED_INT, 0);
                    }
                    gl.bind_vertex_array(None);
                    gl.bind_buffer(glow::ARRAY_BUFFER, None);
                    gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, None);
                }
            })),
        })
    }

    pub fn chunk_count(&self) -> usize {
        self.gpu.lock().map(|g| g.chunks.len()).unwrap_or(0)
    }

    pub fn cached_meshes(&self) -> usize {
        self.tess.len()
    }

    pub fn built_this_frame(&self) -> usize {
        self.tess.built_this_frame()
    }

    /// Release GL objects. Call from `App::on_exit` while the context lives.
    pub fn destroy(&mut self) {
        let mut gpu = self.gpu.lock().unwrap_or_else(|e| e.into_inner());
        gpu.drop_chunks();
        unsafe { gpu.gl.delete_program(gpu.program) };
    }
}

fn chunk_key(ids: impl Iterator<Item = StrokeId>) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    for id in ids {
        id.hash(&mut h);
    }
    h.finish()
}

impl Gpu {
    /// Create or replace chunk `index` with `mesh`.
    fn upload(
        &mut self,
        index: usize,
        key: u64,
        origin: Point,
        bounds: (Point, Point),
        mesh: &Mesh,
    ) {
        let gl = &self.gl;
        // SAFETY: buffer uploads on our own objects, on the GL thread.
        unsafe {
            let chunk = if let Some(existing) = self.chunks.get_mut(index) {
                existing
            } else {
                debug_assert_eq!(index, self.chunks.len());
                let (vao, vbo, ebo) = match (
                    gl.create_vertex_array(),
                    gl.create_buffer(),
                    gl.create_buffer(),
                ) {
                    (Ok(vao), Ok(vbo), Ok(ebo)) => (vao, vbo, ebo),
                    (vao, vbo, ebo) => {
                        warn!("failed to allocate GL buffers: {vao:?} {vbo:?} {ebo:?}");
                        return;
                    }
                };
                gl.bind_vertex_array(Some(vao));
                gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
                let stride = std::mem::size_of::<Vertex>() as i32;
                gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, stride, 0);
                gl.enable_vertex_attrib_array(0);
                gl.vertex_attrib_pointer_f32(
                    1,
                    4,
                    glow::UNSIGNED_BYTE,
                    false,
                    stride,
                    std::mem::offset_of!(Vertex, color) as i32,
                );
                gl.enable_vertex_attrib_array(1);
                gl.bind_vertex_array(None);
                self.chunks.push(Chunk {
                    key,
                    origin,
                    bounds,
                    vao,
                    vbo,
                    ebo,
                    index_count: 0,
                });
                self.chunks.last_mut().expect("just pushed")
            };
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(chunk.vbo));
            gl.buffer_data_u8_slice(
                glow::ARRAY_BUFFER,
                bytemuck::cast_slice(&mesh.vertices),
                glow::STATIC_DRAW,
            );
            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(chunk.ebo));
            gl.buffer_data_u8_slice(
                glow::ELEMENT_ARRAY_BUFFER,
                bytemuck::cast_slice(&mesh.indices),
                glow::STATIC_DRAW,
            );
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, None);
            chunk.key = key;
            chunk.origin = origin;
            chunk.bounds = bounds;
            chunk.index_count = mesh.indices.len() as i32;
        }
    }

    fn truncate(&mut self, len: usize) {
        while self.chunks.len() > len {
            let chunk = self.chunks.pop().expect("len checked");
            self.delete(chunk);
        }
    }

    fn drop_chunks(&mut self) {
        for chunk in std::mem::take(&mut self.chunks) {
            self.delete(chunk);
        }
    }

    fn delete(&self, chunk: Chunk) {
        // SAFETY: deleting our own objects on the GL thread.
        unsafe {
            self.gl.delete_vertex_array(chunk.vao);
            self.gl.delete_buffer(chunk.vbo);
            self.gl.delete_buffer(chunk.ebo);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn chunk_key_is_order_sensitive_and_stable() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert_eq!(chunk_key([a, b].into_iter()), chunk_key([a, b].into_iter()));
        assert_ne!(chunk_key([a, b].into_iter()), chunk_key([b, a].into_iter()));
        assert_ne!(chunk_key([a].into_iter()), chunk_key([a, b].into_iter()));
    }

    #[test]
    fn vertex_layout_matches_shader_attributes() {
        // The VAO hard-codes these; epaint's layout is stable but explicit.
        assert_eq!(std::mem::offset_of!(Vertex, pos), 0);
        assert_eq!(std::mem::offset_of!(Vertex, color), 16);
        assert_eq!(std::mem::size_of::<Vertex>(), 20);
    }
}
