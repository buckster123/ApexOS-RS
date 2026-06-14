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
// Slice 2 (emote uniforms): accent / eyes / brows / mouth / gaze come from the
// FaceGl global (mirrored from the 2D FaceView), so APEX's emotes drive it.
// Slice 3 (SDF): the fake hemisphere becomes a real raymarched ellipsoid head
// with a protruding nose; features are painted on the true 3D normal.
//
// Gated by APEX_FACE_GL=1 in main.rs — dormant (zero cost) otherwise.
//
// GLSL ES 1.00 (`#version 100`) for portability: native on the Pi's V3D GLES and
// accepted by desktop GL drivers. No depth, alpha blend, one VAO + fullscreen
// triangle. Follows Slint's official `opengl_underlay` example shape.

use glow::HasContext;
use std::ffi::CStr;

/// One emote frame's worth of expression, mirrored from the `FaceGl` Slint
/// global (which mirrors the 2D FaceView's state→feature derivations). Built by
/// the notifier each frame and pushed to the shader as uniforms.
#[derive(Clone, Copy, Default)]
pub struct FaceExpr {
    pub accent: [f32; 3], // head tint (linear 0..1)
    pub eye_l: f32,       // left-eye  openness 0..1
    pub eye_r: f32,       // right-eye openness 0..1
    pub brow: f32,        // symmetric brow raise (+up / −down)
    pub brow_skew: f32,   // L/R brow asymmetry
    pub brow_angle: f32,  // inner-end tilt (+up = worried, −down = angry)
    pub mouth: f32,       // mouth curve −1..1 (smile ↔ frown)
    pub open: f32,        // open-mouth amount 0..1 (0 = stroked curve)
    pub gaze: [f32; 2],   // gaze offset fraction (x: −left/+right, y: −up/+down)
    pub intensity: f32,   // expression strength 0..1
    pub blush: f32,       // warm-cheek amount 0..1
    pub talk: f32,        // 1 = speaking → animated mouth-flap
    pub head_roll: f32,   // head tilt/cock
    pub head_pitch: f32,  // chin lift (+) / drop (−)
    pub tear: f32,        // 1 = sliding teardrop (sad)
}

pub struct FaceGl {
    gl: glow::Context,
    program: glow::Program,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    u_time: Option<glow::UniformLocation>,
    u_res: Option<glow::UniformLocation>,
    u_origin: Option<glow::UniformLocation>,
    u_accent: Option<glow::UniformLocation>,
    u_eyes: Option<glow::UniformLocation>,
    u_brow: Option<glow::UniformLocation>,
    u_mouth: Option<glow::UniformLocation>,
    u_gaze: Option<glow::UniformLocation>,
    u_intensity: Option<glow::UniformLocation>,
    u_blush: Option<glow::UniformLocation>,
    u_head: Option<glow::UniformLocation>,
    u_anim: Option<glow::UniformLocation>,
}

const VERT: &str = r#"#version 100
attribute vec2 pos;
void main() { gl_Position = vec4(pos, 0.0, 1.0); }
"#;

// Raymarched SDF head — a real 3D ellipsoid head with a protruding nose
// (sphere-traced, normals via gradient, diffuse + rim + spec), accent-tinted,
// with ink eyes / brows / mouth painted on the surface and driven by the same
// expression uniforms as slice 2. Gaze turns the head; an idle bob keeps it
// alive. y is bottom-up (gl_FragCoord, localised to the face rect via u_origin).
//
// Precision: prefer highp for stable marching, but guard it — GLSL ES 1.00 makes
// highp in the fragment stage OPTIONAL, and declaring it where unsupported is a
// compile error. Where it's missing (some V3D/GLES2 drivers) we fall back to
// mediump; the normal-gradient epsilon below is sized to survive that (a too-small
// epsilon underflows at mediump → zero normals → the face collapses to a blob).
const FRAG: &str = r#"#version 100
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
uniform float u_time;
uniform vec2  u_res;       // face-rect size in physical px
uniform vec2  u_origin;    // face-rect bottom-left in physical px (gl_FragCoord frame)
uniform vec3  u_accent;    // head tint
uniform vec2  u_eyes;      // (left, right) eye openness 0..1
uniform vec3  u_brow;      // (raise, skew, inner-tilt)
uniform vec2  u_mouth;     // (curve −1..1, open 0..1)
uniform vec2  u_gaze;      // gaze offset fraction
uniform float u_intensity; // expression strength 0..1
uniform float u_blush;     // warm-cheek amount 0..1
uniform vec2  u_head;      // (roll, pitch) head tilt
uniform vec2  u_anim;      // (talk, tear)

// Inexact-but-bounded ellipsoid SDF (Quílez); fine for sphere tracing a blob.
float sdEllipsoid(vec3 p, vec3 r) {
    float k0 = length(p / r);
    float k1 = length(p / (r * r));
    return k0 * (k0 - 1.0) / k1;
}
// Smooth union — blends the nose into the head with no seam.
float smin(float a, float b, float k) {
    float h = clamp(0.5 + 0.5 * (b - a) / k, 0.0, 1.0);
    return mix(b, a, h) - k * h * (1.0 - h);
}
// Head + nose. Distance only; features are painted in shading, not carved.
float mapHead(vec3 p) {
    float head = sdEllipsoid(p, vec3(0.90, 0.98, 0.82));
    float nose = sdEllipsoid(p - vec3(0.0, -0.06, 0.74), vec3(0.12, 0.16, 0.18));
    return smin(head, nose, 0.10);
}
vec3 calcNormal(vec3 p) {
    // Epsilon kept comfortably above mediump's ~0.001 resolution near r≈0.9 so the
    // central differences don't cancel to zero on a mediump fallback driver.
    vec2 e = vec2(0.005, 0.0);
    return normalize(vec3(
        mapHead(p + e.xyy) - mapHead(p - e.xyy),
        mapHead(p + e.yxy) - mapHead(p - e.yxy),
        mapHead(p + e.yyx) - mapHead(p - e.yyx)));
}
mat3 rotY(float a) { float c = cos(a), s = sin(a); return mat3(c, 0.0, -s, 0.0, 1.0, 0.0, s, 0.0, c); }
mat3 rotX(float a) { float c = cos(a), s = sin(a); return mat3(1.0, 0.0, 0.0, 0.0, c, s, 0.0, -s, c); }
mat3 rotZ(float a) { float c = cos(a), s = sin(a); return mat3(c, s, 0.0, -s, c, 0.0, 0.0, 0.0, 1.0); }
float hash1(float x) { return fract(sin(x * 12.9898) * 43758.5453); }

// Distance from p to segment a—b (2D, for brow bars).
float segd(vec2 p, vec2 a, vec2 b) {
    vec2 pa = p - a, ba = b - a;
    float h = clamp(dot(pa, ba) / dot(ba, ba), 0.0, 1.0);
    return length(pa - ba * h);
}

// A glossy eye: a dark sheened eyeball with two catchlights that shift with
// `look` (gaze + idle drift), and an upper eyelid that descends as `open` drops
// (blink / sleepy / squint) carrying a dark lash line. `head` is the lit head
// colour, used for the eyelid + soft socket so it blends into the face. Returns
// vec4(rgb, mask); mask is the soft eye-opening coverage.
vec4 eyePix(vec2 fc, vec2 c, float open, vec2 look, vec3 head) {
    vec2 p = fc - c;
    float ow = 0.115, oh = 0.155;
    float shape = length(p / vec2(ow, oh));
    if (shape > 1.25) return vec4(0.0);

    // Dark glossy eyeball with a faint downward sheen.
    vec3 col = vec3(0.05, 0.06, 0.09);
    col += vec3(0.05, 0.07, 0.11) * smoothstep(0.0, -oh, p.y);

    // Catchlights — primary up-left, tiny secondary down-right; both ride `look`.
    vec2 pc = look * vec2(0.05, 0.045);
    float cl  = length((p - pc - vec2(-0.030, 0.045)) / vec2(0.020, 0.025));
    col = mix(vec3(1.0), col, smoothstep(0.55, 1.05, cl));
    float cl2 = length((p - pc - vec2(0.022, -0.030)) / vec2(0.011, 0.013));
    col = mix(vec3(0.80, 0.86, 0.96), col, smoothstep(0.55, 1.10, cl2));

    // Upper eyelid: open=1 → lid above the eye; open=0 → covers it entirely.
    float lidY = mix(-oh - 0.03, oh + 0.03, clamp(open, 0.0, 1.0));
    float lid  = smoothstep(lidY - 0.012, lidY + 0.012, p.y);
    col = mix(col, head, lid);
    // Dark lash line riding the lid edge — makes blinks read.
    float lash = smoothstep(0.013, 0.003, abs(p.y - lidY)) * smoothstep(1.25, 1.0, shape);
    col = mix(col, vec3(0.04, 0.05, 0.07), lash);

    return vec4(col, smoothstep(1.25, 1.04, shape));
}

void main() {
    vec2 local = gl_FragCoord.xy - u_origin;
    vec2 uv = (local - 0.5 * u_res) / min(u_res.x, u_res.y);

    // Perspective camera with a subtle idle bob; gaze turns the head.
    float bob = sin(u_time * 1.1) * 0.012;
    vec3 ro = vec3(0.0, bob, 3.05);
    vec3 rd = normalize(vec3(uv * 0.95, -1.75));
    // Head orientation: gaze turn + a per-emote tilt (roll) and chin lift (pitch).
    mat3 look = rotZ(u_head.x * 0.45)
              * rotY(-u_gaze.x * 4.0)
              * rotX(u_gaze.y * 4.0 - u_head.y * 0.4);
    vec3 roH = look * ro;
    vec3 rdH = look * rd;   // march in head space → fixed light + feature frame

    float t = 0.0, hit = -1.0;
    for (int i = 0; i < 72; i++) {
        float d = mapHead(roH + rdH * t);
        if (d < 0.001) { hit = t; break; }
        t += d * 0.9;
        if (t > 5.0) break;
    }
    if (hit < 0.0) { gl_FragColor = vec4(0.0); return; }   // miss → cleared bg

    vec3 p = roH + rdH * hit;
    vec3 n = calcNormal(p);
    vec3 viewDir = -rdH;
    vec3 lightDir = normalize(vec3(0.40, 0.55, 0.85));

    float diff = clamp(dot(n, lightDir), 0.0, 1.0);
    float fill = clamp(dot(n, normalize(vec3(-0.35, -0.25, 0.70))), 0.0, 1.0);
    float rim  = pow(1.0 - clamp(dot(n, viewDir), 0.0, 1.0), 2.5);
    float spec = pow(clamp(dot(reflect(-lightDir, n), viewDir), 0.0, 1.0), 24.0);

    vec3 col = u_accent * (0.30 + 0.60 * diff + 0.16 * fill);
    col += u_accent * rim * 0.38;          // accent rim wrap
    col += vec3(1.0) * spec * 0.20;        // crisp highlight

    vec3  ink   = vec3(0.04, 0.05, 0.07);
    float front = smoothstep(0.05, 0.45, n.z);  // paint features on the face front only
    vec2  fc    = p.xy;

    vec3 headCol = col;   // lit head colour, for eyelids + socket blending

    // Eyes — glossy, gaze-tracking, with descending lids. Pupils follow gaze
    // plus a gentle idle drift so the face never feels frozen.
    // Idle saccades — hold a random target, dart to a new one every ~2.4s;
    // faded out when a real gaze is set so the agent's look wins.
    float st  = u_time * 0.42;
    float seg = floor(st);
    vec2  sA  = vec2(hash1(seg) - 0.5, hash1(seg + 7.3) - 0.5);
    vec2  sB  = vec2(hash1(seg + 1.0) - 0.5, hash1(seg + 8.3) - 0.5);
    vec2  sacc = mix(sA, sB, smoothstep(0.0, 0.10, fract(st))) * 0.5;
    sacc *= 1.0 - clamp(length(u_gaze) * 6.0, 0.0, 1.0);
    vec2  eyeAim = u_gaze * 8.0 + sacc;

    // Crisp auto-blink — a quick lid dip (~0.45s) every ~5s.
    float bph   = fract(u_time * 0.2);
    float blink = 1.0 - 0.92 * (1.0 - smoothstep(0.0, 0.045, abs(bph - 0.03)));
    vec4 le = eyePix(fc, vec2(-0.28, 0.15), u_eyes.x * blink, eyeAim, headCol);
    vec4 re = eyePix(fc, vec2( 0.28, 0.15), u_eyes.y * blink, eyeAim, headCol);
    col = mix(col, le.rgb, le.a * front);
    col = mix(col, re.rgb, re.a * front);

    // Brows — bars with raise + skew (vertical) AND inner-end tilt: angle>0 lifts
    // the inner ends (worried/sad), angle<0 drops them (angry V).
    float lDy = (u_brow.x + u_brow.y) * u_intensity * 0.18;
    float rDy = (u_brow.x - u_brow.y) * u_intensity * 0.18;
    float ta  = u_brow.z * u_intensity * 0.085;
    vec2  lb  = vec2(-0.28, 0.42 + lDy);
    vec2  rb  = vec2( 0.28, 0.42 + rDy);
    float bw  = 0.17;
    float bd  = min(segd(fc, lb + vec2(-bw, -ta), lb + vec2(bw,  ta)),
                    segd(fc, rb + vec2(-bw,  ta), rb + vec2(bw, -ta)));
    col = mix(col, ink, (1.0 - smoothstep(0.045, 0.065, bd)) * front);

    // Mouth — filled maw when open, else a stroked quadratic (smile ↔ frown).
    // When speaking, a fast oscillation drives a lively lip-flap.
    float talkOpen = (0.18 + 0.30 * (0.5 + 0.5 * sin(u_time * 15.0))) * u_anim.x;
    float openAmt  = max(u_mouth.y, talkOpen);
    vec2 mc = vec2(0.0, -0.40);
    if (openAmt > 0.02) {
        float mh = max(0.04, openAmt) * 0.34;
        float mo = length((fc - mc) / vec2(0.30, mh));
        col = mix(col, ink, (1.0 - smoothstep(0.92, 1.08, mo)) * front);
    } else {
        float halfW = 0.40;
        float nx = fc.x / halfW;   // pow(x,2) is undefined for x<0 in GLSL ES — square it
        float yc = mc.y - u_mouth.x * 0.34 * (1.0 - nx * nx);
        float line = smoothstep(0.055, 0.028, abs(fc.y - yc))
                   * step(abs(fc.x - mc.x), halfW) * front;
        col = mix(col, ink, line);
    }

    // Blush — warm cheeks for affection / pride / delight.
    if (u_blush > 0.01) {
        float bl = min(length((fc - vec2(-0.40, -0.07)) / vec2(0.16, 0.115)),
                       length((fc - vec2( 0.40, -0.07)) / vec2(0.16, 0.115)));
        float bm = (1.0 - smoothstep(0.45, 1.0, bl)) * u_blush * 0.45 * front;
        col = mix(col, vec3(1.0, 0.45, 0.55), bm);
    }

    // Tear — wells under the right eye and slides down the cheek, on a loop (sad).
    if (u_anim.y > 0.5) {
        float tph = fract(u_time * 0.33);
        vec2  tc  = vec2(0.255, -0.02 - tph * 0.40);
        float td  = length((fc - tc) / vec2(0.034, 0.052));
        float tm  = (1.0 - smoothstep(0.82, 1.08, td)) * front
                  * smoothstep(0.0, 0.08, tph) * smoothstep(1.0, 0.9, tph);
        col = mix(col, vec3(0.62, 0.82, 1.0), tm * 0.9);
        float tcl = length((fc - tc - vec2(-0.010, 0.014)) / vec2(0.008, 0.010));
        col = mix(col, vec3(1.0), (1.0 - smoothstep(0.6, 1.1, tcl)) * tm);
    }

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
            let u_accent = gl.get_uniform_location(program, "u_accent");
            let u_eyes = gl.get_uniform_location(program, "u_eyes");
            let u_brow = gl.get_uniform_location(program, "u_brow");
            let u_mouth = gl.get_uniform_location(program, "u_mouth");
            let u_gaze = gl.get_uniform_location(program, "u_gaze");
            let u_intensity = gl.get_uniform_location(program, "u_intensity");
            let u_blush = gl.get_uniform_location(program, "u_blush");
            let u_head = gl.get_uniform_location(program, "u_head");
            let u_anim = gl.get_uniform_location(program, "u_anim");

            Ok(Self {
                gl, program, vao, vbo, u_time, u_res, u_origin,
                u_accent, u_eyes, u_brow, u_mouth, u_gaze, u_intensity, u_blush,
                u_head, u_anim,
            })
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
    pub fn draw(
        &self,
        time: f32,
        win_w: f32,
        win_h: f32,
        fx: f32,
        fy: f32,
        fw: f32,
        fh: f32,
        expr: &FaceExpr,
    ) {
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
            // Expression uniforms (mirrored from the 2D FaceView via FaceGl).
            if let Some(u) = self.u_accent.as_ref() {
                self.gl.uniform_3_f32(Some(u), expr.accent[0], expr.accent[1], expr.accent[2]);
            }
            if let Some(u) = self.u_eyes.as_ref() {
                self.gl.uniform_2_f32(Some(u), expr.eye_l, expr.eye_r);
            }
            if let Some(u) = self.u_brow.as_ref() {
                self.gl.uniform_3_f32(Some(u), expr.brow, expr.brow_skew, expr.brow_angle);
            }
            if let Some(u) = self.u_mouth.as_ref() {
                self.gl.uniform_2_f32(Some(u), expr.mouth, expr.open);
            }
            if let Some(u) = self.u_gaze.as_ref() {
                self.gl.uniform_2_f32(Some(u), expr.gaze[0], expr.gaze[1]);
            }
            if let Some(u) = self.u_intensity.as_ref() {
                self.gl.uniform_1_f32(Some(u), expr.intensity);
            }
            if let Some(u) = self.u_blush.as_ref() {
                self.gl.uniform_1_f32(Some(u), expr.blush);
            }
            if let Some(u) = self.u_head.as_ref() {
                self.gl.uniform_2_f32(Some(u), expr.head_roll, expr.head_pitch);
            }
            if let Some(u) = self.u_anim.as_ref() {
                self.gl.uniform_2_f32(Some(u), expr.talk, expr.tear);
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
