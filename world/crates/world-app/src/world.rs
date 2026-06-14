//! Bevy 3D scene scaffolding — the hub Atrium (DESIGN.md §2, doc 03 §5).
//!
//! ============================================================================
//! BUILD STATUS: this whole module is behind `#[cfg(feature = "viz")]` and is OFF
//! by default. Bevy is a DEFERRED migration (DESIGN.md D2/D3, doc 06 §4): no
//! released Bevy shares Slint 1.16's wgpu-29, so the shared-device handoff cannot
//! typecheck/link today (Spike 0 records the clash). This file is therefore
//! UNCOMPILED in the default build and is NOT yet validated against a Bevy version.
//! The API shapes below target Bevy 0.16 (required-components style) and may need
//! adjustment when the version is pinned in Spike 0. // TODO(Mn): Spike 0 → pin Bevy.
//! ============================================================================
//!
//! What this scaffolds (per doc 03 §5/§6):
//!   - a headless `App` (NO `WindowPlugin` primary window, NEVER `App::run` — it is
//!     stepped from the Slint frame timer via `step()`);
//!   - the hub scene: ground plane, key light, one camera;
//!   - placeholder STATION entities (one per object-console kind) tagged `StationEntity`;
//!   - one placeholder AVATAR entity tagged `AvatarEntity`;
//!   - a picking/raycast STUB system that, on "activate", emits an `ActivateRequest`.

#![cfg(feature = "viz")]

use bevy::prelude::*;
use std::sync::mpsc::Sender;

// ── Scene marker components (doc 03 §5) ──────────────────────────────────────

/// A placed station console. `id` matches `stations::Station::id` and the integer
/// the Slint `activate(int)` callback carries.
#[derive(Component)]
pub struct StationEntity {
    pub id: i32,
}

/// An embodied agent avatar = one agentd `SessionId` (stored as the bare u64 to keep
/// this module free of a `world-protocol` import; the bridge maps it back).
#[derive(Component)]
pub struct AvatarEntity {
    pub session: u64,
}

/// The free-fly / walk camera.
#[derive(Component)]
pub struct WorldCamera;

/// What the picker found under the reticle this frame (broad-phase result). `None`
/// when nothing is targeted. Drives the FOCUSED-state outline in a later milestone.
#[derive(Resource, Default)]
pub struct Focused(pub Option<i32>);

/// Emitted out of the Bevy world when the user (or an agent) activates a target.
/// The bridge forwards this to the Slint `activate(int)` callback (Mode II takeover).
/// Carrying it on a std mpsc keeps the Bevy↔host channel framework-agnostic.
#[derive(Resource)]
pub struct ActivateOut(pub Sender<i32>);

// ── App construction ─────────────────────────────────────────────────────────

/// Build the headless Bevy app. The Slint rendering notifier injects the shared
/// wgpu device/queue (doc 03 §2.2) — that wiring is the make-or-break integration and
/// is NOT done here yet. // TODO(Mn): Spike 1 — inject Slint's RenderDevice/RenderQueue
/// and target a persistent `world_texture` instead of a primary window.
pub fn build_world_app(activate_tx: Sender<i32>) -> App {
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: None, // headless — Slint owns the window (Pattern A)
        ..default()
    }))
    .init_resource::<Focused>()
    .insert_resource(ActivateOut(activate_tx))
    .add_systems(Startup, setup_hub)
    .add_systems(Update, (camera_controller, pick_system));
    app
}

/// Step the world exactly one frame. Called from the Slint `Timer` (~16 ms) in
/// `main.rs`; NEVER `App::run` (doc 06 Risks: two event loops). After this returns,
/// the host hands the freshest `world_texture` to the Slint `Image`.
pub fn step(app: &mut App) {
    app.update();
}

// ── Scene setup ────────────────────────────────────────────────────────────────

fn setup_hub(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // Key light.
    commands.spawn((
        DirectionalLight { illuminance: 8000.0, ..default() },
        Transform::from_xyz(4.0, 8.0, 4.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));

    // Camera — orbit/walk controller drives this Transform (doc 03 §6).
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(0.0, 1.7, 8.0).looking_at(Vec3::new(0.0, 1.0, 0.0), Vec3::Y),
        WorldCamera,
    ));

    // Ground plane — the Atrium floor (doubles as ambient telemetry surface later).
    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(24.0, 24.0))),
        MeshMaterial3d(materials.add(Color::srgb(0.12, 0.13, 0.18))),
        Transform::from_xyz(0.0, 0.0, 0.0),
    ));

    // Placeholder STATION consoles around the perimeter (doc 05 §6 object consoles).
    // ids 1..=5 match `StationRegistry::launch_layout` (0 is the Chat avatar below).
    let station_color = materials.add(Color::srgb(0.20, 0.45, 0.70));
    let station_mesh = meshes.add(Cuboid::new(1.0, 2.0, 0.3));
    let perimeter = [
        (1, Vec3::new(-6.0, 1.0, -4.0)), // System
        (2, Vec3::new(0.0, 1.0, -7.0)),  // Sensors
        (3, Vec3::new(6.0, 1.0, -4.0)),  // Council
        (4, Vec3::new(6.0, 1.0, 4.0)),   // Terminal
        (5, Vec3::new(-6.0, 1.0, 4.0)),  // Memory
    ];
    for (id, pos) in perimeter {
        commands.spawn((
            Mesh3d(station_mesh.clone()),
            MeshMaterial3d(station_color.clone()),
            Transform::from_translation(pos),
            StationEntity { id },
        ));
    }

    // Placeholder AVATAR — the root APEX agent, near center (DESIGN.md §3). v0 body
    // is a capsule; glTF rig is v1. // TODO(Mn): bind to the real root SessionId from
    // the first `session_init`; pulse `agent_text` -> emissive.
    commands.spawn((
        Mesh3d(meshes.add(Capsule3d::new(0.4, 1.2))),
        MeshMaterial3d(materials.add(Color::srgb(0.0, 1.0, 0.62))),
        Transform::from_xyz(0.0, 1.0, 0.0),
        AvatarEntity { session: 0 }, // placeholder; replaced on session bind
    ));

    info!("apexos-world hub scene ready (5 station consoles + 1 avatar placeholder)");
}

// ── Systems ────────────────────────────────────────────────────────────────────

/// Camera controller STUB. Real WASD + mouse-look (Walk) and orbit/fly modes land in
/// M0/M1 (doc 03 §6). // TODO(Mn): read input, drive the WorldCamera Transform.
fn camera_controller(_camera: Query<&mut Transform, With<WorldCamera>>) {
    // no-op stub
}

/// Picking/raycast STUB (doc 03 §6). The real path casts a ray from the camera
/// through the reticle/cursor, broad-phases against each `StationEntity`/`AvatarEntity`
/// AABB, narrows to the few in-range, sets `Focused`, and on the activate input emits
/// the focused id via `ActivateOut`.
///
/// Here it is wired but inert: it demonstrates the activate channel without real input
/// handling. // TODO(Mn): M1 implement the ray test + the E-key/click activation edge.
fn pick_system(
    focused: Res<Focused>,
    activate_out: Res<ActivateOut>,
    _stations: Query<(&StationEntity, &Transform)>,
) {
    // Real impl: ray-test _stations, set Focused, and on the activate input edge:
    if false {
        if let Some(id) = focused.0 {
            let _ = activate_out.0.send(id);
        }
    }
}
