// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Seven visually distinct procedural 3D presets for Beyrix Background Engine.

use glam::Vec3;

use super::bindings::EvaluatedModifiers;
use super::scene3d::{
    Camera3D, Material3D, Object3D, Particle3D, ProjectionMode, Scene3D, grid_mesh, sphere_mesh,
    torus_mesh,
};
use crate::config::BackgroundPreset;

pub fn build_preset_scene(
    preset: BackgroundPreset,
    t: f32,
    mods: &EvaluatedModifiers,
    mouse_norm: (f32, f32),
    accent: [f32; 3],
) -> Scene3D {
    let mut scene = Scene3D::default();
    let yaw_mouse = (mouse_norm.0 - 0.5) * 0.4;
    let pitch_mouse = (mouse_norm.1 - 0.5) * 0.3;

    match preset {
        BackgroundPreset::Torus => {
            scene.camera = Camera3D {
                position: Vec3::new(yaw_mouse, 0.6 + pitch_mouse, 3.8),
                ..Default::default()
            };
            scene.objects.push(Object3D {
                id: "torus".into(),
                mesh: torus_mesh(1.15, 0.44, 28, 14),
                material: Material3D {
                    color: accent,
                    ..Default::default()
                },
                position: Vec3::ZERO,
                rotation: Vec3::new(
                    t * 0.7 * mods.rotation_speed,
                    t * 1.1 * mods.rotation_speed,
                    0.2,
                ),
                scale: Vec3::splat(mods.scale),
            });
        }
        BackgroundPreset::Sphere => {
            scene.camera = Camera3D {
                position: Vec3::new(yaw_mouse, pitch_mouse, 3.5),
                ..Default::default()
            };
            scene.objects.push(Object3D {
                id: "sphere".into(),
                mesh: sphere_mesh(1.3, 18, 28),
                material: Material3D {
                    color: [0.38, 0.85, 0.76],
                    ..Default::default()
                },
                position: Vec3::ZERO,
                rotation: Vec3::new(0.25, t * 0.6 * mods.rotation_speed, 0.1),
                scale: Vec3::splat(mods.scale),
            });
        }
        BackgroundPreset::Terrain => {
            scene.camera = Camera3D {
                position: Vec3::new(yaw_mouse * 1.5, 2.2 + pitch_mouse, 4.5),
                target: Vec3::new(0.0, 0.2, 0.0),
                ..Default::default()
            };
            let phase = t * 0.9 * mods.rotation_speed;
            let amp = 0.45 * mods.wave_amplitude;
            let mesh = grid_mesh(22, 18, 6.0, |x, z| {
                ((x * 1.3 + phase).sin() * (z * 1.1 - phase * 0.6).cos()) * amp
            });
            scene.objects.push(Object3D {
                id: "terrain".into(),
                mesh,
                material: Material3D {
                    color: [0.52, 0.84, 0.48],
                    ..Default::default()
                },
                position: Vec3::ZERO,
                rotation: Vec3::ZERO,
                scale: Vec3::ONE,
            });
        }
        BackgroundPreset::Ocean => {
            scene.camera = Camera3D {
                position: Vec3::new(0.0, 1.5 + pitch_mouse, 4.2),
                target: Vec3::new(0.0, -0.2, 0.0),
                ..Default::default()
            };
            let p = t * 1.4 * mods.rotation_speed;
            let amp = 0.32 * mods.wave_amplitude;
            let mesh = grid_mesh(24, 18, 7.0, |x, z| {
                ((x * 1.8 + p).sin() + (z * 1.4 - p * 1.2).cos()) * amp
            });
            scene.objects.push(Object3D {
                id: "ocean".into(),
                mesh,
                material: Material3D {
                    color: [0.24, 0.68, 0.98],
                    ..Default::default()
                },
                position: Vec3::ZERO,
                rotation: Vec3::ZERO,
                scale: Vec3::ONE,
            });
        }
        BackgroundPreset::Tunnel => {
            scene.camera = Camera3D {
                position: Vec3::new(yaw_mouse * 0.4, pitch_mouse * 0.4, 0.2),
                target: Vec3::new(0.0, 0.0, -4.0),
                ..Default::default()
            };
            let ring_mesh = torus_mesh(1.3, 0.10, 22, 8);
            for i in 0..6 {
                let z = -((t * 2.2 * mods.rotation_speed + i as f32 * 1.1) % 6.6);
                scene.objects.push(Object3D {
                    id: format!("ring-{i}"),
                    mesh: ring_mesh.clone(),
                    material: Material3D {
                        color: [0.75, 0.48, 0.98],
                        ..Default::default()
                    },
                    position: Vec3::new(0.0, 0.0, z),
                    rotation: Vec3::new(std::f32::consts::FRAC_PI_2, 0.0, t * 0.3 + i as f32 * 0.2),
                    scale: Vec3::splat(mods.scale),
                });
            }
        }
        BackgroundPreset::Starfield => {
            scene.camera = Camera3D {
                position: Vec3::new(yaw_mouse * 0.5, pitch_mouse * 0.5, 2.0),
                target: Vec3::new(0.0, 0.0, -5.0),
                ..Default::default()
            };
            for i in 0..90 {
                let fi = i as f32;
                let angle = fi * 2.39996;
                let radius = 0.6 + (fi % 11.0) * 0.28;
                let z = -((fi * 0.37 + t * 2.8 * mods.rotation_speed) % 9.0);
                scene.particles.push(Particle3D {
                    pos: Vec3::new(angle.cos() * radius, angle.sin() * radius, z),
                    vel: Vec3::ZERO,
                    color: [0.85, 0.92, 1.0],
                    ch: if z > -2.5 {
                        '*'
                    } else if z > -5.5 {
                        '+'
                    } else {
                        '.'
                    },
                });
            }
        }
        BackgroundPreset::AudioReactive => {
            scene.camera = Camera3D {
                position: Vec3::new(yaw_mouse, 0.9 + pitch_mouse, 4.2),
                projection: ProjectionMode::Perspective,
                ..Default::default()
            };
            scene.objects.push(Object3D {
                id: "core".into(),
                mesh: sphere_mesh(0.85, 14, 22),
                material: Material3D {
                    color: [0.96, 0.44, 0.62],
                    ..Default::default()
                },
                position: Vec3::ZERO,
                rotation: Vec3::new(t * 0.5, t * 0.9 * mods.rotation_speed, 0.0),
                scale: Vec3::splat(mods.scale),
            });
            let ring_mesh = torus_mesh(1.55, 0.14, 24, 10);
            scene.objects.push(Object3D {
                id: "halo".into(),
                mesh: ring_mesh,
                material: Material3D {
                    color: accent,
                    ..Default::default()
                },
                position: Vec3::ZERO,
                rotation: Vec3::new(t * 1.2, -t * 0.7, 0.4),
                scale: Vec3::splat(0.9 + mods.wave_amplitude * 0.25),
            });
            for i in 0..24 {
                let a = (i as f32 / 24.0) * std::f32::consts::TAU + t;
                let r = 1.9 + mods.particle_impulse * 0.8;
                scene.particles.push(Particle3D {
                    pos: Vec3::new(a.cos() * r, (t * 2.0 + i as f32).sin() * 0.4, a.sin() * r),
                    vel: Vec3::ZERO,
                    color: [0.4, 0.9, 1.0],
                    ch: '*',
                });
            }
        }
    }
    scene.light.intensity = mods.light_intensity;
    scene
}
