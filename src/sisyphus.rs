//! Pure geometry of the animated Sisyphus menu-bar icon.
//!
//! Frames are returned as drawing primitives in points, origin bottom-left,
//! inside a `ICON_W` x `ICON_H` box, so they can be unit-tested and drawn by
//! any backend (AppKit for the menu bar, PNG for previews).
//!
//! Scenes:
//! * `Push`      – pushing the boulder uphill (loops); with music he sings
//!   (notes float up from his head).
//! * `RestIntro` – sits down while the boulder rolls away and vanishes
//!   (plays once); stars fade in.
//! * `RestSky`   – resting under a twinkling night sky with the occasional
//!   shooting star (loops); with music he wears headphones.

use std::f64::consts::{PI, TAU};

pub const ICON_W: f64 = 24.0;
pub const ICON_H: f64 = 22.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scene {
    Push,
    RestIntro,
    RestSky,
}

impl Scene {
    /// Seconds per pass (a loop for `Push`/`RestSky`, once for the intro).
    pub fn duration(self) -> f64 {
        match self {
            Scene::Push => 2.0,
            Scene::RestIntro => 3.5,
            Scene::RestSky => 12.0,
        }
    }
}

type Pt = (f64, f64);

/// One drawing primitive; `alpha` multiplies the ink colour.
#[derive(Debug, Clone, PartialEq)]
pub enum Prim {
    /// Open polyline with round caps and joins.
    Line { pts: Vec<Pt>, width: f64, alpha: f64 },
    /// Circle, stroked (`width > 0`) or filled (`width == 0`).
    Circle { c: Pt, r: f64, width: f64, alpha: f64 },
}

// Slope geometry: a straight hill rising to the right.
const ORIGIN: Pt = (1.0, 3.0);
const SLOPE_DEG: f64 = 22.0;
const SLOPE_LEN: f64 = 23.0;
const BOULDER_R: f64 = 4.6;
const STROKE: f64 = 1.25;
const THIGH: f64 = 3.3;
const SHIN: f64 = 3.3;
const HEAD_R: f64 = 1.5;

/// Unit vectors along (`u`) and normal to (`n`) the slope.
fn axes() -> (Pt, Pt) {
    let t = SLOPE_DEG.to_radians();
    ((t.cos(), t.sin()), (-t.sin(), t.cos()))
}

/// Point at distance `s` along the slope and height `h` above it.
fn on_slope(s: f64, h: f64) -> Pt {
    let (u, n) = axes();
    (ORIGIN.0 + s * u.0 + h * n.0, ORIGIN.1 + s * u.1 + h * n.1)
}

fn add(a: Pt, b: Pt) -> Pt {
    (a.0 + b.0, a.1 + b.1)
}

fn sub(a: Pt, b: Pt) -> Pt {
    (a.0 - b.0, a.1 - b.1)
}

fn scale(a: Pt, k: f64) -> Pt {
    (a.0 * k, a.1 * k)
}

fn len(a: Pt) -> f64 {
    a.0.hypot(a.1)
}

fn lerp(a: Pt, b: Pt, t: f64) -> Pt {
    add(a, scale(sub(b, a), t))
}

/// Smooth 0→1 ramp for `x` in [0, 1] (clamped outside).
fn smoothstep(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// Two-bone inverse kinematics: knee position for a leg from `hip` to
/// `foot`, bending towards `+x` (knees point forward/uphill).
fn knee(hip: Pt, foot: Pt) -> Pt {
    let d = sub(foot, hip);
    let dist = len(d).min(THIGH + SHIN - 1e-6);
    // Distance from hip to the knee's projection on the hip-foot line.
    let a = (THIGH * THIGH - SHIN * SHIN + dist * dist) / (2.0 * dist);
    let h = (THIGH * THIGH - a * a).max(0.0).sqrt();
    let dir = scale(d, 1.0 / len(d));
    let base = add(hip, scale(dir, a));
    // Pick the perpendicular whose x component is positive.
    let perp = if -dir.1 >= 0.0 { (-dir.1, dir.0) } else { (dir.1, -dir.0) };
    add(base, scale(perp, h))
}

fn line(pts: Vec<Pt>) -> Prim {
    Prim::Line { pts, width: STROKE, alpha: 1.0 }
}

fn dot(c: Pt, r: f64, alpha: f64) -> Prim {
    Prim::Circle { c, r, width: 0.0, alpha }
}

fn ground() -> Prim {
    Prim::Line { pts: vec![on_slope(0.0, 0.0), on_slope(SLOPE_LEN, 0.0)], width: 1.1, alpha: 1.0 }
}

/// Boulder with a mark that shows rotation (`angle` in radians, CCW).
fn boulder(c: Pt, angle: f64, alpha: f64) -> Vec<Prim> {
    let mark = add(c, scale((angle.cos(), angle.sin()), BOULDER_R * 0.5));
    vec![Prim::Circle { c, r: BOULDER_R, width: STROKE, alpha }, dot(mark, 0.95, alpha)]
}

/// An eighth note whose head sits at `p`.
fn note(p: Pt, alpha: f64) -> Vec<Prim> {
    let stem_top = add(p, (0.5, 1.7));
    vec![
        dot(p, 0.6, alpha),
        Prim::Line { pts: vec![add(p, (0.5, 0.1)), stem_top], width: 0.45, alpha },
        Prim::Line { pts: vec![stem_top, add(stem_top, (0.75, -0.55))], width: 0.45, alpha },
    ]
}

/// Headband arc plus ear cups around a head at `c`.
fn headphones(c: Pt) -> Vec<Prim> {
    let band: Vec<Pt> = (0..=8)
        .map(|i| {
            let a = (20.0 + 140.0 * f64::from(i) / 8.0).to_radians();
            add(c, (2.05 * a.cos(), 2.05 * a.sin()))
        })
        .collect();
    vec![
        Prim::Line { pts: band, width: 0.6, alpha: 1.0 },
        dot(add(c, (-1.9, 0.0)), 0.75, 1.0),
        dot(add(c, (1.9, 0.0)), 0.75, 1.0),
    ]
}

/// Frame for `scene` at phase in `[0, 1)` of its duration.
pub fn frame(scene: Scene, phase: f64, music: bool) -> Vec<Prim> {
    match scene {
        Scene::Push => push_frame(phase.rem_euclid(1.0), music),
        // The intro ends exactly where the sky loop's t = 0 begins.
        Scene::RestIntro => {
            let p = phase.clamp(0.0, 1.0);
            rest_frame((p - 1.0) * Scene::RestIntro.duration(), Some(p), music)
        }
        Scene::RestSky => rest_frame(phase.rem_euclid(1.0) * Scene::RestSky.duration(), None, music),
    }
}

fn push_frame(phase: f64, music: bool) -> Vec<Prim> {
    let (_, n) = axes();
    // Two strides per 2-second loop.
    let w = 2.0 * TAU * phase;
    // Slight bob twice per stride.
    let bob = scale(n, 0.25 * (2.0 * w).sin().abs());

    let c = on_slope(17.0, BOULDER_R);
    let hip = add(on_slope(6.6, 5.0), bob);
    let shoulder = add(on_slope(9.8, 7.4), bob);
    let torso = sub(shoulder, hip);
    let head = add(shoulder, scale(torso, 1.75 / len(torso)));

    // Hands touch the boulder on the side facing the shoulder.
    let to_shoulder = sub(add(shoulder, (0.0, 0.6)), c);
    let hand = add(c, scale(to_shoulder, BOULDER_R / len(to_shoulder)));
    let elbow = add(scale(add(shoulder, hand), 0.5), (0.0, -0.7));

    // Alternating stride: each foot slides along the slope, lifted on the
    // forward swing.
    let mut prims = vec![ground()];
    for sign in [1.0, -1.0] {
        let sw = sign * w.sin();
        let lift = if sign * w.cos() > 0.0 { 0.7 * (sign * w.cos()) } else { 0.0 };
        let foot = on_slope(4.6 + 1.4 * sw, lift);
        prims.push(line(vec![hip, knee(hip, foot), foot]));
    }
    prims.push(line(vec![hip, shoulder]));
    prims.push(line(vec![shoulder, elbow, hand]));
    prims.push(dot(head, 1.45, 1.0));
    // Rolling uphill to the right = clockwise rotation.
    prims.extend(boulder(c, PI / 2.0 - w / 2.0, 1.0));

    if music {
        // Singing: two notes float up and back from his head, staggered by
        // half a loop, fading in and out.
        for offset in [0.0, 0.5] {
            let t = (phase + offset).rem_euclid(1.0);
            let alpha = smoothstep(t / 0.15) * smoothstep((1.0 - t) / 0.3);
            let base = lerp((9.4, 17.0), (6.6, 19.2), t);
            let p = add(base, (0.35 * (TAU * t * 1.5).sin(), 0.0));
            prims.extend(note(p, alpha));
        }
    }
    prims
}

/// Fixed star field: (position, radius, twinkles per sky loop, phase).
/// Placed in the sky above the slope, clear of the seated figure.
const STARS: [(Pt, f64, f64, f64); 9] = [
    ((2.5, 19.5), 0.55, 1.0, 0.0),
    ((5.5, 16.0), 0.45, 2.0, 1.3),
    ((8.0, 20.6), 0.5, 3.0, 2.1),
    ((11.0, 17.5), 0.6, 1.0, 4.0),
    ((13.6, 21.0), 0.45, 2.0, 0.7),
    ((14.8, 18.4), 0.4, 3.0, 5.2),
    ((4.0, 12.6), 0.4, 2.0, 3.3),
    ((7.6, 13.4), 0.35, 1.0, 1.9),
    ((22.8, 21.2), 0.4, 3.0, 2.8),
];

/// Shooting stars in the 12 s sky loop: (start s, length s, from, to, alpha).
const METEORS: [(f64, f64, Pt, Pt, f64); 2] = [
    (2.0, 0.8, (17.0, 21.0), (6.0, 16.2), 1.0),
    (7.6, 0.6, (11.5, 21.1), (3.0, 17.8), 0.7),
];

/// Seated figure at sky time `t` (seconds; negative during the intro).
/// `intro` is the intro progress in [0, 1], or `None` in the sky loop.
fn rest_frame(t: f64, intro: Option<f64>, music: bool) -> Vec<Prim> {
    let sky_period = Scene::RestSky.duration();
    // Breathing: 4 breaths per sky loop, continuous from the intro.
    let breath = (0.0, 0.22 * (TAU * t / (sky_period / 4.0)).sin());

    let seat = on_slope(19.0, 0.9);
    let shoulder = add(add(seat, (0.6, 4.4)), breath);
    let head = add(shoulder, (0.35, 2.0));
    let knee_pt = add(seat, (-2.8, 2.6));
    let foot = on_slope(15.0, 0.3);
    let hand_back = on_slope(21.8, 0.35);

    let mut prims = vec![
        ground(),
        line(vec![seat, knee_pt, foot]),
        line(vec![seat, shoulder]),
        // One forearm resting on the knee, the other propping him up.
        line(vec![shoulder, add(knee_pt, (0.3, 0.3))]),
        line(vec![shoulder, hand_back]),
        dot(head, HEAD_R, 1.0),
    ];
    if music {
        prims.extend(headphones(head));
    }

    // Stars twinkle with integer cycles per loop (seamless); during the
    // intro they fade in over its second half.
    let fade_in = intro.map_or(1.0, |p| smoothstep((p - 0.4) / 0.6));
    for &(c, r, cycles, ph) in &STARS {
        let tw = 0.5 + 0.5 * (TAU * cycles * t / sky_period + ph).sin();
        let alpha = (0.35 + 0.65 * tw) * fade_in;
        if alpha > 0.01 {
            prims.push(dot(c, r, alpha));
        }
    }

    match intro {
        // The boulder rolls away from his feet, accelerating, and fades out
        // as it leaves the hill. It does not come back.
        Some(p) => {
            let (s0, s1) = (11.0, -4.0);
            let s = s0 + (s1 - s0) * p * p;
            let alpha = smoothstep((1.0 - p) / 0.3);
            // Rolling downhill to the left = counter-clockwise.
            let angle = PI / 2.0 + (s0 - s) / BOULDER_R;
            if alpha > 0.01 {
                prims.extend(boulder(on_slope(s, BOULDER_R), angle, alpha));
            }
        }
        None => {
            for &(start, dur, from, to, peak) in &METEORS {
                let k = (t - start) / dur;
                if (0.0..=1.0).contains(&k) {
                    // Bright head, short tail, fading in and out.
                    let alpha = peak * smoothstep(k / 0.2) * smoothstep((1.0 - k) / 0.35);
                    let head_pt = lerp(from, to, k);
                    let tail = lerp(from, to, (k - 0.3).max(0.0));
                    prims.push(Prim::Line { pts: vec![tail, head_pt], width: 0.6, alpha: alpha * 0.8 });
                    prims.push(dot(head_pt, 0.45, alpha));
                }
            }
        }
    }
    prims
}

#[cfg(test)]
mod tests {
    use super::*;

    fn points(p: &Prim) -> Vec<(Pt, f64)> {
        match p {
            Prim::Line { pts, width, .. } => pts.iter().map(|&q| (q, width / 2.0)).collect(),
            Prim::Circle { c, r, width, .. } => vec![(*c, r + width / 2.0)],
        }
    }

    fn boulders(prims: &[Prim]) -> Vec<Pt> {
        prims
            .iter()
            .filter_map(|p| match p {
                Prim::Circle { c, r, .. } if *r == BOULDER_R => Some(*c),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn everything_fits_the_icon_box() {
        // The boulder may roll off the left edge in the intro; everything
        // else must stay inside.
        for scene in [Scene::Push, Scene::RestIntro, Scene::RestSky] {
            for music in [false, true] {
                for i in 0..=48 {
                    for prim in frame(scene, f64::from(i) / 48.0, music) {
                        let rolling_off = scene == Scene::RestIntro
                            && matches!(prim, Prim::Circle { r, .. } if r == BOULDER_R || r == 0.95);
                        for ((x, y), r) in points(&prim) {
                            assert!(y + r <= ICON_H + 0.01 && y - r >= -0.01, "{scene:?} y={y}");
                            assert!(x + r <= ICON_W + 0.01, "{scene:?} x={x}");
                            if !rolling_off {
                                assert!(x - r >= -0.01, "{scene:?} x={x}");
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn knee_has_valid_bone_lengths() {
        let (hip, foot) = ((5.0, 9.0), (4.0, 4.5));
        let k = knee(hip, foot);
        assert!((len(sub(k, hip)) - THIGH).abs() < 1e-6);
        assert!((len(sub(foot, k)) - SHIN).abs() < 1e-6);
        assert!(k.0 > hip.0.min(foot.0), "knee bends forward");
    }

    #[test]
    fn boulder_rolls_away_and_never_returns() {
        let x = |p: f64| boulders(&frame(Scene::RestIntro, p, false))[0].0;
        assert!(x(0.2) > x(0.5) && x(0.5) > x(0.8));
        assert!(boulders(&frame(Scene::RestIntro, 1.0, false)).is_empty(), "gone at the end");
        for i in 0..24 {
            assert!(boulders(&frame(Scene::RestSky, f64::from(i) / 24.0, false)).is_empty());
        }
    }

    #[test]
    fn music_adds_notes_or_headphones() {
        let more = |scene, p| frame(scene, p, true).len() > frame(scene, p, false).len();
        assert!(more(Scene::Push, 0.3), "singing notes");
        assert!(more(Scene::RestSky, 0.3), "headphones");
    }

    #[test]
    fn shooting_stars_are_rare() {
        // Count frames (12 fps) with a meteor streak: ~1.4 s of 12 s.
        let streak = |p: f64| {
            frame(Scene::RestSky, p, false)
                .iter()
                .any(|x| matches!(x, Prim::Line { width, .. } if *width == 0.6))
        };
        let n = (0..144).filter(|&i| streak(f64::from(i) / 144.0)).count();
        assert!((10..=24).contains(&n), "{n} meteor frames");
    }

    #[test]
    fn intro_end_matches_sky_start() {
        assert_eq!(frame(Scene::RestIntro, 1.0, true), frame(Scene::RestSky, 0.0, true));
    }
}
