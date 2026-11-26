//! Small, dependency-free animation primitives.
//!
//! Springs and easings are evaluated from an absolute virtual clock, so a late
//! frame does not accumulate error. Springs can be underdamped (`damping-ratio
//! < 1`), and easing curves such as `ease-out-back` or `cubic-bezier` also
//! support overshoot.

use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Curve {
    Linear,
    EaseInOut,
    EaseOutQuad,
    EaseOutCubic,
    EaseOutExpo,
    EaseOutBack,
    EaseInOutBack,
    CubicBezier { x1: f64, y1: f64, x2: f64, y2: f64 },
}

impl Curve {
    pub fn y(self, x: f64) -> f64 {
        let x = x.clamp(0.0, 1.0);
        match self {
            Curve::Linear => x,
            Curve::EaseInOut => {
                if x < 0.5 {
                    2.0 * x * x
                } else {
                    1.0 - (-2.0 * x + 2.0).powi(2) / 2.0
                }
            }
            Curve::EaseOutQuad => 1.0 - (1.0 - x) * (1.0 - x),
            Curve::EaseOutCubic => 1.0 - (1.0 - x).powi(3),
            Curve::EaseOutExpo => {
                if x >= 1.0 {
                    1.0
                } else {
                    1.0 - 2.0_f64.powf(-10.0 * x)
                }
            }
            Curve::EaseOutBack => {
                let c1 = 1.70158;
                let c3 = c1 + 1.0;
                1.0 + c3 * (x - 1.0).powi(3) + c1 * (x - 1.0).powi(2)
            }
            Curve::EaseInOutBack => {
                let c1 = 1.70158;
                let c2 = c1 * 1.525;
                if x < 0.5 {
                    ((2.0 * x).powi(2) * ((c2 + 1.0) * 2.0 * x - c2)) / 2.0
                } else {
                    ((2.0 * x - 2.0).powi(2) * ((c2 + 1.0) * (x * 2.0 - 2.0) + c2) + 2.0) / 2.0
                }
            }
            Curve::CubicBezier { x1, y1, x2, y2 } => cubic_bezier_y(x, x1, y1, x2, y2),
        }
    }
}

fn cubic_bezier_y(x: f64, x1: f64, y1: f64, x2: f64, y2: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }

    let sample = |t: f64, a: f64, b: f64| {
        let mt = 1.0 - t;
        3.0 * mt * mt * t * a + 3.0 * mt * t * t * b + t * t * t
    };

    // Bisection is plenty for a UI curve and has no startup cost.
    let mut lo = 0.0;
    let mut hi = 1.0;
    let mut t = x;
    for _ in 0..32 {
        t = (lo + hi) / 2.0;
        let guess = sample(t, x1, x2);
        if guess < x {
            lo = t;
        } else {
            hi = t;
        }
    }
    sample(t, y1, y2)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Spring {
        damping_ratio: f64,
        stiffness: f64,
        epsilon: f64,
    },
    Easing {
        duration_ms: u64,
        curve: Curve,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnimationConfig {
    pub off: bool,
    pub kind: Kind,
}

impl AnimationConfig {
    pub const fn spring(damping_ratio: f64, stiffness: f64, epsilon: f64) -> Self {
        Self {
            off: false,
            kind: Kind::Spring {
                damping_ratio,
                stiffness,
                epsilon,
            },
        }
    }

    pub const fn easing(duration_ms: u64, curve: Curve) -> Self {
        Self {
            off: false,
            kind: Kind::Easing { duration_ms, curve },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OpenAnimationConfig {
    pub animation: AnimationConfig,
    pub scale_from: f64,
    pub opacity_from: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnimationsConfig {
    pub off: bool,
    pub slowdown: f64,
    pub view_movement: AnimationConfig,
    pub window_open: OpenAnimationConfig,
    pub window_close: AnimationConfig,
    /// Target scale at the end of the close animation.
    pub window_close_scale: f64,
    pub window_resize: AnimationConfig,
    pub window_movement: AnimationConfig,
    pub overview: AnimationConfig,
}

impl Default for AnimationsConfig {
    fn default() -> Self {
        Self {
            off: false,
            slowdown: 1.0,
            view_movement: AnimationConfig::spring(0.9, 900.0, 0.0007),
            window_open: OpenAnimationConfig {
                animation: AnimationConfig::spring(0.72, 800.0, 0.001),
                scale_from: 0.9,
                opacity_from: 0.0,
            },
            // Close uses a short ease-out; no bounce is wanted.
            window_close: AnimationConfig::easing(120, Curve::EaseOutQuad),
            window_close_scale: 0.92,
            window_resize: AnimationConfig::spring(1.0, 800.0, 0.0005),
            // Window/column movement, stack reorder, and drop transitions.
            window_movement: AnimationConfig::spring(0.82, 900.0, 0.0007),
            overview: AnimationConfig::spring(0.9, 900.0, 0.0007),
        }
    }
}

/// Absolute virtual time. `slowdown` > 1 makes animations slower.
#[derive(Debug, Clone)]
pub struct Clock {
    base_instant: Instant,
    base_virtual: f64,
    slowdown: f64,
}

impl Clock {
    pub fn new(slowdown: f64) -> Self {
        let slowdown = if slowdown.is_finite() {
            slowdown.max(0.001)
        } else {
            1.0
        };
        Self {
            base_instant: Instant::now(),
            base_virtual: 0.0,
            slowdown,
        }
    }

    pub fn now(&self) -> f64 {
        self.base_virtual + self.base_instant.elapsed().as_secs_f64() / self.slowdown
    }

    /// Change the global slowdown without making running animations jump or
    /// ever moving virtual time backward.
    pub fn set_slowdown(&mut self, slowdown: f64) {
        if !slowdown.is_finite() {
            return;
        }
        let slowdown = slowdown.max(0.001);
        if (slowdown - self.slowdown).abs() < f64::EPSILON {
            return;
        }

        let now = Instant::now();
        self.base_virtual += now
            .saturating_duration_since(self.base_instant)
            .as_secs_f64()
            / self.slowdown;
        self.base_instant = now;
        self.slowdown = slowdown;
    }
}

#[derive(Debug, Clone)]
pub struct Animation {
    from: f64,
    to: f64,
    start: f64,
    initial_velocity: f64,
    kind: Kind,
    off: bool,
}

impl Animation {
    pub fn new(config: AnimationConfig, from: f64, to: f64, now: f64) -> Self {
        Self {
            from,
            to,
            start: now,
            initial_velocity: 0.0,
            kind: config.kind,
            off: config.off,
        }
    }

    pub fn value(&self, now: f64) -> f64 {
        if self.off {
            return self.to;
        }
        match self.kind {
            Kind::Easing { duration_ms, curve } => {
                if duration_ms == 0 {
                    return self.to;
                }
                let t = ((now - self.start) / (duration_ms as f64 / 1000.0)).clamp(0.0, 1.0);
                self.from + (self.to - self.from) * curve.y(t)
            }
            Kind::Spring {
                damping_ratio,
                stiffness,
                ..
            } => self.spring_value(now, damping_ratio, stiffness),
        }
    }

    pub fn velocity(&self, now: f64) -> f64 {
        if self.off {
            return 0.0;
        }
        const H: f64 = 1.0 / 1000.0;
        let t0 = (now - H).max(self.start);
        let t1 = now + H;
        (self.value(t1) - self.value(t0)) / (t1 - t0)
    }

    /// Monotonic 0..1 progress for properties that must not follow an
    /// overshoot, such as opacity.
    ///
    /// Assumes a `0.0 -> 1.0` animation with zero initial velocity.
    ///
    /// `value()` deliberately overshoots for a bouncy spring: it goes above
    /// the target, back below it, and then settles. Driving opacity from that
    /// value makes a fully opaque window turn slightly translucent on the
    /// rebound. Opacity instead rises with the spring until it first reaches
    /// the target, then holds at 1. Easing uses elapsed time, so overshooting
    /// curves such as `ease-out-back` cannot make opacity dip either.
    pub fn monotonic_progress(&self, now: f64) -> f64 {
        if self.off {
            return 1.0;
        }
        let elapsed = (now - self.start).max(0.0);
        match self.kind {
            Kind::Easing { duration_ms, .. } => {
                if duration_ms == 0 {
                    1.0
                } else {
                    (elapsed / (duration_ms as f64 / 1000.0)).clamp(0.0, 1.0)
                }
            }
            Kind::Spring {
                damping_ratio,
                stiffness,
                ..
            } => {
                let stiffness = stiffness.max(0.0001);
                let beta = damping_ratio.max(0.0) * stiffness.sqrt();
                let omega0 = stiffness.sqrt();
                if beta < omega0 {
                    let omega1 = (omega0 * omega0 - beta * beta).sqrt();
                    // A 0 -> 1 spring with zero initial velocity first reaches
                    // the target when the decaying bracket crosses zero:
                    // omega1 * t = pi/2 + atan(beta / omega1).
                    let first_target =
                        (std::f64::consts::FRAC_PI_2 + (beta / omega1).atan()) / omega1;
                    if elapsed >= first_target {
                        1.0
                    } else {
                        self.value(now).clamp(0.0, 1.0)
                    }
                } else {
                    // Critically/overdamped springs approach the target from
                    // below; their value is already monotonic.
                    self.value(now).clamp(0.0, 1.0)
                }
            }
        }
    }

    pub fn is_done(&self, now: f64) -> bool {
        if self.off {
            return true;
        }
        match self.kind {
            Kind::Easing { duration_ms, .. } => now - self.start >= duration_ms as f64 / 1000.0,
            Kind::Spring {
                damping_ratio,
                stiffness,
                epsilon,
            } => {
                let elapsed = (now - self.start).max(0.0);
                if elapsed > 20.0 {
                    return true;
                }

                let stiffness = stiffness.max(0.0001);
                let beta = damping_ratio.max(0.0) * stiffness.sqrt();
                let omega0 = stiffness.sqrt();

                if beta > 0.0 && omega0 - beta > f32::EPSILON as f64 {
                    // Underdamped: wait for the true oscillation envelope
                    // (including initial velocity on retargeted springs) to
                    // decay below epsilon.
                    let omega1 = (omega0 * omega0 - beta * beta).sqrt();
                    let x0 = self.from - self.to;
                    let envelope = x0.hypot((beta * x0 + self.initial_velocity) / omega1);
                    !envelope.is_finite() || envelope * (-beta * elapsed).exp() <= epsilon
                } else {
                    let value = self.value(now);
                    if !value.is_finite() {
                        return true;
                    }
                    let velocity = self.velocity(now);
                    (value - self.to).abs() <= epsilon
                        && (!velocity.is_finite() || velocity.abs() <= epsilon * omega0.max(1.0))
                }
            }
        }
    }

    pub fn retarget(&mut self, now: f64, to: f64) {
        let value = self.value(now);
        let velocity = self.velocity(now);
        self.from = value;
        self.to = to;
        self.start = now;
        self.initial_velocity = if matches!(self.kind, Kind::Spring { .. }) {
            velocity
        } else {
            0.0
        };
    }

    // Analytical damped harmonic oscillator adapted from niri's `Spring::oscillate`
    // (derived from libadwaita LGPL-2.1-or-later and RBBAnimation MIT).
    fn spring_value(&self, now: f64, damping_ratio: f64, stiffness: f64) -> f64 {
        let t = (now - self.start).max(0.0);
        let stiffness = stiffness.max(0.0001);
        let omega0 = stiffness.sqrt();
        let beta = damping_ratio.max(0.0) * omega0;
        let x0 = self.from - self.to;
        let v0 = self.initial_velocity;
        let envelope = (-beta * t).exp();

        if (beta - omega0).abs() <= f32::EPSILON as f64 {
            self.to + envelope * (x0 + (beta * x0 + v0) * t)
        } else if beta < omega0 {
            let omega1 = (omega0 * omega0 - beta * beta).sqrt();
            self.to
                + envelope
                    * (x0 * (omega1 * t).cos() + ((beta * x0 + v0) / omega1) * (omega1 * t).sin())
        } else {
            // Use the real-exponential form instead of exp(-beta*t) * cosh/sinh.
            // At large t the envelope underflows to zero while cosh/sinh
            // overflow, producing NaN for an otherwise valid overdamped spring.
            let omega2 = (beta * beta - omega0 * omega0).sqrt();
            let c = (beta * x0 + v0) / omega2;
            let e1 = (-(beta - omega2) * t).exp();
            let e2 = (-(beta + omega2) * t).exp();
            self.to + 0.5 * (x0 + c) * e1 + 0.5 * (x0 - c) * e2
        }
    }
}

/// A scalar with a target and an optional running animation.
#[derive(Debug, Clone)]
pub struct AnimatedValue {
    pub value: f64,
    pub target: f64,
    animation: Option<Animation>,
}

impl AnimatedValue {
    pub fn new(value: f64) -> Self {
        Self {
            value,
            target: value,
            animation: None,
        }
    }

    pub fn set_target(&mut self, target: f64, now: f64, config: AnimationConfig) {
        if config.off || (self.value - target).abs() < f64::EPSILON {
            self.value = target;
            self.target = target;
            self.animation = None;
            return;
        }

        // Repeated layout passes with the same target must not restart an
        // in-flight spring.
        if self.animation.is_some() && (self.target - target).abs() < f64::EPSILON {
            return;
        }

        self.target = target;
        if let Some(animation) = &mut self.animation {
            tracing::trace!(now, target, "retarget animation");
            animation.retarget(now, target);
        } else {
            tracing::trace!(now, target, from = self.value, "start animation");
            self.animation = Some(Animation::new(config, self.value, target, now));
        }
    }

    pub fn snap(&mut self, value: f64) {
        self.value = value;
        self.target = value;
        self.animation = None;
    }

    pub fn tick(&mut self, now: f64) -> bool {
        let Some(animation) = &self.animation else {
            return false;
        };
        tracing::trace!(
            now,
            value = animation.value(now),
            done = animation.is_done(now),
            "animation tick value"
        );
        self.value = animation.value(now);
        if animation.is_done(now) {
            self.value = self.target;
            self.animation = None;
            // Render one final frame at the snapped value, then stop.
        }
        true
    }

    pub fn is_animating(&self) -> bool {
        self.animation.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn easing_endpoints() {
        for curve in [
            Curve::Linear,
            Curve::EaseInOut,
            Curve::EaseOutBack,
            Curve::EaseInOutBack,
            Curve::CubicBezier {
                x1: 0.34,
                y1: 1.56,
                x2: 0.64,
                y2: 1.0,
            },
        ] {
            assert!((curve.y(0.0) - 0.0).abs() < 0.001);
            assert!((curve.y(1.0) - 1.0).abs() < 0.001);
        }
    }

    #[test]
    fn back_curve_overshoots() {
        let curve = Curve::EaseOutBack;
        assert!(curve.y(0.7) > 1.0);
    }

    #[test]
    fn underdamped_spring_overshoots() {
        let config = AnimationConfig::spring(0.35, 900.0, 0.0001);
        let animation = Animation::new(config, 0.0, 1.0, 0.0);
        assert!(animation.value(0.12) > 1.0);
        assert!(!animation.is_done(0.1));
        assert!(animation.is_done(5.0));
    }

    #[test]
    fn monotonic_progress_does_not_follow_the_spring_rebound() {
        // An underdamped spring overshoots, then undershoots.
        let animation = Animation::new(AnimationConfig::spring(0.58, 600.0, 0.0015), 0.0, 1.0, 0.0);
        assert!(animation.value(0.2) > 1.0);
        assert!(animation.value(0.315) < 1.0);
        // Opacity is complete when the window first reaches normal size, and
        // the later undershoot cannot reduce it.
        assert!((animation.monotonic_progress(0.11) - 1.0).abs() < 1e-9);
        assert_eq!(animation.monotonic_progress(0.2), 1.0);
        assert_eq!(animation.monotonic_progress(0.315), 1.0);

        let mut last = 0.0;
        for step in 0..1000 {
            let t = step as f64 * 0.005;
            let progress = animation.monotonic_progress(t);
            assert!((0.0..=1.0).contains(&progress));
            assert!(
                progress + 1e-9 >= last,
                "opacity progress fell from {last} to {progress} at {t}s"
            );
            last = progress;
        }
        assert!(last > 0.99);
    }

    #[test]
    fn monotonic_progress_uses_time_for_overshooting_easings() {
        let animation = Animation::new(
            AnimationConfig::easing(100, Curve::EaseOutBack),
            0.0,
            1.0,
            0.0,
        );
        assert!(animation.value(0.07) > 1.0);

        let mut last = 0.0;
        for step in 0..=100 {
            let t = step as f64 / 1000.0;
            let progress = animation.monotonic_progress(t);
            assert!(progress + 1e-9 >= last);
            last = progress;
        }
        assert!((last - 1.0).abs() < 1e-9);
    }

    #[test]
    fn eased_animation_completes() {
        let config = AnimationConfig::easing(100, Curve::Linear);
        let animation = Animation::new(config, 0.0, 1.0, 0.0);
        assert_eq!(animation.value(0.05), 0.5);
        assert!(animation.is_done(0.1));
    }

    #[test]
    fn retarget_keeps_current_value() {
        let config = AnimationConfig::easing(100, Curve::Linear);
        let mut animation = Animation::new(config, 0.0, 1.0, 0.0);
        let before = animation.value(0.05);
        animation.retarget(0.05, 2.0);
        assert!((animation.value(0.05) - before).abs() < 0.001);
    }

    #[test]
    fn camera_spring_settles_quickly() {
        let animation =
            Animation::new(AnimationConfig::spring(0.9, 900.0, 0.0007), 0.0, 956.0, 0.0);
        assert!(!animation.is_done(0.1));
        assert!(
            animation.is_done(0.6),
            "value={} vel={}",
            animation.value(0.6),
            animation.velocity(0.6)
        );
    }

    #[test]
    fn animated_value_renders_a_final_frame_then_stops() {
        let mut value = AnimatedValue::new(0.0);
        value.set_target(1.0, 0.0, AnimationConfig::easing(100, Curve::Linear));
        assert!(value.tick(0.05));
        assert!(value.is_animating());
        assert!(value.tick(0.2));
        assert!(!value.is_animating());
        assert_eq!(value.value, 1.0);
        assert!(!value.tick(0.3));
    }

    #[test]
    fn animated_value_snaps_when_off() {
        let mut value = AnimatedValue::new(3.0);
        let config = AnimationConfig {
            off: true,
            ..AnimationConfig::spring(1.0, 900.0, 0.001)
        };
        value.set_target(7.0, 0.0, config);
        assert_eq!(value.value, 7.0);
        assert!(!value.tick(1.0));
    }

    #[test]
    fn clock_slowdown_change_never_moves_backward() {
        let mut clock = Clock::new(0.01);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let before = clock.now();
        clock.set_slowdown(1000.0);
        let after = clock.now();
        assert!(
            after >= before,
            "virtual time jumped backward: {before} -> {after}"
        );
        clock.set_slowdown(f64::NAN);
        clock.set_slowdown(f64::INFINITY);
        assert!(clock.now() >= after);
    }

    #[test]
    fn retargeted_spring_preserves_momentum_near_target() {
        let mut anim = Animation::new(AnimationConfig::spring(0.9, 900.0, 0.001), 0.0, 500.0, 0.0);
        let mid = anim.value(0.04);
        anim.retarget(0.04, mid);
        assert!(
            !anim.is_done(0.04),
            "retargeted spring with high velocity should not finish immediately"
        );
        assert!(anim.is_done(1.0));
    }

    #[test]
    fn velocity_at_start_uses_forward_step() {
        let anim = Animation::new(AnimationConfig::easing(100, Curve::Linear), 0.0, 100.0, 1.0);
        let v = anim.velocity(1.0);
        assert!((v - 1000.0).abs() < 1e-6, "expected 1000.0, got {v}");
    }

    #[test]
    fn extreme_overdamped_spring_stays_finite() {
        for (damping, stiffness) in [(2.0, 10_000.0), (10.0, 100_000.0)] {
            let config = AnimationConfig::spring(damping, stiffness, 0.0001);
            let animation = Animation::new(config, 0.0, 1.0, 0.0);
            for step in 0..=2000 {
                let t = step as f64 * 0.01;
                let value = animation.value(t);
                assert!(
                    value.is_finite(),
                    "spring ({damping}, {stiffness}) produced {value} at {t}s"
                );
            }
            assert!(animation.is_done(20.1));
            assert!((animation.value(20.1) - 1.0).abs() <= 0.0001);
        }
    }
}
