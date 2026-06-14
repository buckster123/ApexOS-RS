// ── Phase-2 face — raw GL under Slint's rendering notifier ───────────────────
//
// Renders a custom GLSL face inside the existing ui-slint window via
// `Window::set_rendering_notifier` + femtovg's `GraphicsAPI::NativeOpenGL`,
// sharing the same GL context femtovg draws with. This is Path A (in-Slint GL
// underlay) toward the real raymarched, emote-driven face.
//
// Slice 1 (scissor-to-window): the GL pass is confined to the FaceView's live
// on-window rect (glViewport + glScissor), so the face renders *inside the face
// window* and tracks it as it moves/resizes — not floating over the whole UI.
// It clears its rect to the face bg first, hiding the 2D fallback face beneath.
// Still a blinking lit cyan sphere; emote uniforms + SDF sculpting come next.
//
// Gated by APEX_FACE_GL=1 in main.rs — dormant (zero cost) otherwise.
//
// GLSL ES 1.00 (`#version 100`) for portability: native on the Pi's V3D GLES and
// accepted by desktop GL drivers. No depth, alpha blend, one VAO + fullscreen
// triangle. Follows Slint's official `opengl_underlay` example shape.

use glow::HasContext;
use std::ffi::CStr;

pub struct FaceGl {
    gl: glow::Context,
    program: glow::Program,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    u_time: Option<glow::UniformLocation>,
    u_res: Option<glow::UniformLocation>,
    u_origin: Option<glow::UniformLocation>,
}

const VERT: &str = r#"#version 100
attribute vec2 pos;
void main() { gl_Position = vec4(pos, 0.0, 1.0); }
"#;

// Lit sphere + blinking eyes + a small smile. Cyan APEX accent. Alpha 0 outside
// the head so it floats over the UI. y is bottom-up (gl_FragCoord).
const FRAG: &str = r#"#version 100
precision mediump float;
uniform float u_time;
uniform vec2  u_res;     // face-rect size in physical px
uniform vec2  u_origin;  // face-rect bottom-left in physical px (gl_FragCoord frame)
void main() {
    // gl_FragCoord is window-absolute even with a viewport set, so localise it to
    // the face rect before normalising — keeps the face centred in its window.
    vec2 local = gl_FragCoord.xy - u_origin;
    vec2 uv = (local - 0.5 * u_res) / min(u_res.x, u_res.y);
    float r = 0.32;
    float d = length(uv);
    if (d > r) { gl_FragColor = vec4(0.0); return; }

    // Fake hemisphere normal for simple diffuse + rim lighting.
    float z = sqrt(max(0.0, r * r - d * d));
    vec3 n = normalize(vec3(uv, z));
    vec3 lightDir = normalize(vec3(0.4, 0.55, 0.8));
    float diff = clamp(dot(n, lightDir), 0.0, 1.0);
    vec3 base = vec3(0.0, 0.83, 1.0);
    vec3 col = base * (0.22 + 0.78 * diff);
    col += pow(1.0 - z / r, 3.0) * 0.35;          // rim glow

    vec3 ink = vec3(0.02, 0.05, 0.08);

    // Eyes — squash vertically on a blink every ~3s.
    float blink = step(2.75, mod(u_time, 3.0));
    float eyeH = mix(1.0, 0.12, blink);
    float le = length(vec2((uv.x + 0.11),        (uv.y - 0.06) / eyeH));
    float re = length(vec2((uv.x - 0.11),        (uv.y - 0.06) / eyeH));
    float eye = min(le, re);
    col = mix(ink, col, smoothstep(0.040, 0.052, eye));

    // Smile — a shallow upward parabola.
    float curve = uv.y + 0.12 - 0.45 * uv.x * uv.x;
    float mouth = smoothstep(0.016, 0.0, abs(curve)) * step(abs(uv.x), 0.14);
    col = mix(col, ink, mouth);

    gl_FragColor = vec4(col, 1.0);
}
"#;

impl FaceGl {
    /// Build the program + geometry from the live GL context. `get_proc_address`
    /// is only valid for the duration of this call (the rendering-setup callback);
    /// glow eagerly resolves every entry point, so we don't retain it.
    pub fn new(
        get_proc_address: &dyn Fn(&CStr) -> *const std::ffi::c_void,
    ) -> Result<Self, String> {
        unsafe {
            let gl = glow::Context::from_loader_function_cstr(|s| get_proc_address(s));

            let program = gl.create_program().map_err(|e| format!("create_program: {e}"))?;
            for (kind, src) in [(glow::VERTEX_SHADER, VERT), (glow::FRAGMENT_SHADER, FRAG)] {
                let sh = gl.create_shader(kind).map_err(|e| format!("create_shader: {e}"))?;
                gl.shader_source(sh, src);
                gl.compile_shader(sh);
                if !gl.get_shader_compile_status(sh) {
                    return Err(format!("shader compile: {}", gl.get_shader_info_log(sh)));
                }
                gl.attach_shader(program, sh);
                gl.delete_shader(sh);
            }
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                return Err(format!("program link: {}", gl.get_program_info_log(program)));
            }

            // Fullscreen triangle.
            let verts: [f32; 6] = [-1.0, -1.0, 3.0, -1.0, -1.0, 3.0];
            let bytes = core::slice::from_raw_parts(
                verts.as_ptr() as *const u8,
                verts.len() * core::mem::size_of::<f32>(),
            );
            let vao = gl.create_vertex_array().map_err(|e| format!("create_vao: {e}"))?;
            let vbo = gl.create_buffer().map_err(|e| format!("create_vbo: {e}"))?;
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STATIC_DRAW);
            let loc = gl.get_attrib_location(program, "pos").ok_or("attrib 'pos' missing")?;
            gl.enable_vertex_attrib_array(loc);
            gl.vertex_attrib_pointer_f32(loc, 2, glow::FLOAT, false, 0, 0);
            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);

            let u_time = gl.get_uniform_location(program, "u_time");
            let u_res = gl.get_uniform_location(program, "u_res");
            let u_origin = gl.get_uniform_location(program, "u_origin");

            Ok(Self { gl, program, vao, vbo, u_time, u_res, u_origin })
        }
    }

    /// Draw the face confined to the face window's rect. Called from the
    /// AfterRendering notifier (femtovg has already drawn this frame), so we
    /// scissor to the face rect, clear it, and blend the face on top — then
    /// restore GL state so femtovg's next frame is unaffected.
    ///
    /// All args are **physical** px. `win_w/win_h` = full framebuffer; `fx/fy`
    /// = face-rect top-left in window coords (Y-down, as Slint reports); `fw/fh`
    /// = face-rect size. We flip Y here for GL's bottom-left origin.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(&self, time: f32, win_w: f32, win_h: f32, fx: f32, fy: f32, fw: f32, fh: f32) {
        if fw <= 0.0 || fh <= 0.0 {
            return;
        }
        // GL's origin is bottom-left; Slint's fy is top-down.
        let sy = win_h - (fy + fh);
        let (ix, iy, iw, ih) = (fx as i32, sy as i32, fw as i32, fh as i32);
        unsafe {
            // Confine everything below to the face rect.
            self.gl.viewport(ix, iy, iw, ih);
            self.gl.enable(glow::SCISSOR_TEST);
            self.gl.scissor(ix, iy, iw, ih);

            // Opaque face background — hides the 2D fallback face femtovg drew
            // underneath, so we see one face, not two overlaid. (~Palette.bg.)
            self.gl.clear_color(0.051, 0.059, 0.094, 1.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);

            self.gl.disable(glow::DEPTH_TEST);
            self.gl.enable(glow::BLEND);
            self.gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
            self.gl.use_program(Some(self.program));
            if let Some(u) = self.u_time.as_ref() {
                self.gl.uniform_1_f32(Some(u), time);
            }
            if let Some(u) = self.u_res.as_ref() {
                self.gl.uniform_2_f32(Some(u), fw, fh);
            }
            if let Some(u) = self.u_origin.as_ref() {
                self.gl.uniform_2_f32(Some(u), fx, sy);
            }
            self.gl.bind_vertex_array(Some(self.vao));
            self.gl.draw_arrays(glow::TRIANGLES, 0, 3);
            self.gl.bind_vertex_array(None);
            self.gl.use_program(None);

            // Restore full-frame state so the next femtovg frame isn't clipped.
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.viewport(0, 0, win_w as i32, win_h as i32);
        }
    }
}

impl Drop for FaceGl {
    fn drop(&mut self) {
        unsafe {
            self.gl.delete_program(self.program);
            self.gl.delete_buffer(self.vbo);
            self.gl.delete_vertex_array(self.vao);
        }
    }
}
