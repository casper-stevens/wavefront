//! 2nd-order Butterworth biquad filters (Direct Form 1) plus pan/gain mixing.

use crate::state::{ClientConfig, Pan, Role};

const SAMPLE_RATE: f32 = 48_000.0;
const Q: f32 = 0.7071;

#[derive(Debug, Clone, Copy, PartialEq)]
enum FilterKind {
    LowPass,
    HighPass,
    None,
}

/// A single Direct-Form-1 biquad, stateful per channel.
#[derive(Debug, Clone, Copy, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    fn new_low_pass(freq: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * freq / SAMPLE_RATE;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * Q);

        let b0 = (1.0 - cos_w0) / 2.0;
        let b1 = 1.0 - cos_w0;
        let b2 = (1.0 - cos_w0) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;

        Biquad {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            ..Default::default()
        }
    }

    fn new_high_pass(freq: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * freq / SAMPLE_RATE;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * Q);

        let b0 = (1.0 + cos_w0) / 2.0;
        let b1 = -(1.0 + cos_w0);
        let b2 = (1.0 + cos_w0) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;

        Biquad {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            ..Default::default()
        }
    }

    fn process(&mut self, x0: f32) -> f32 {
        let y0 =
            self.b0 * x0 + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x0;
        self.y2 = self.y1;
        self.y1 = y0;
        y0
    }

    fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }
}

/// Per-client DSP chain: pan/mix, then role filter (sub=lowpass, tweeter=highpass,
/// full=passthrough), then gain. Stateful biquads that reset on config filter change.
pub struct DspChain {
    kind: FilterKind,
    freq: f32,
    filter_l: Biquad,
    filter_r: Biquad,
    config: ClientConfig,
}

impl DspChain {
    pub fn new(config: ClientConfig, crossover_hz: f32) -> Self {
        let kind = Self::kind_for_role(config.role);
        let (filter_l, filter_r) = Self::build_filters(kind, crossover_hz);
        DspChain {
            kind,
            freq: crossover_hz,
            filter_l,
            filter_r,
            config,
        }
    }

    fn kind_for_role(role: Role) -> FilterKind {
        match role {
            Role::Sub => FilterKind::LowPass,
            Role::Tweeter => FilterKind::HighPass,
            Role::Full => FilterKind::None,
        }
    }

    fn build_filters(kind: FilterKind, freq: f32) -> (Biquad, Biquad) {
        match kind {
            FilterKind::LowPass => (Biquad::new_low_pass(freq), Biquad::new_low_pass(freq)),
            FilterKind::HighPass => (Biquad::new_high_pass(freq), Biquad::new_high_pass(freq)),
            FilterKind::None => (Biquad::default(), Biquad::default()),
        }
    }

    /// Update config; rebuilds (and resets) the filters if the role or crossover changed.
    pub fn set_config(&mut self, config: ClientConfig, crossover_hz: f32) {
        let new_kind = Self::kind_for_role(config.role);
        if new_kind != self.kind || (crossover_hz - self.freq).abs() > f32::EPSILON {
            let (fl, fr) = Self::build_filters(new_kind, crossover_hz);
            self.filter_l = fl;
            self.filter_r = fr;
            self.filter_l.reset();
            self.filter_r.reset();
            self.kind = new_kind;
            self.freq = crossover_hz;
        }
        self.config = config;
    }

    /// Process one interleaved stereo s16 buffer in place.
    pub fn process(&mut self, samples: &mut [i16]) {
        let gain = self.config.gain;
        let pan = self.config.pan;

        let mut i = 0;
        while i + 1 < samples.len() {
            let l = samples[i] as f32 / i16::MAX as f32;
            let r = samples[i + 1] as f32 / i16::MAX as f32;

            let (mut out_l, mut out_r) = match pan {
                Pan::Left => (l, l),
                Pan::Right => (r, r),
                Pan::Mid => {
                    let m = (l + r) * 0.5;
                    (m, m)
                }
            };

            out_l = self.filter_l.process(out_l);
            out_r = self.filter_r.process(out_r);

            out_l *= gain;
            out_r *= gain;

            samples[i] = (out_l.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            samples[i + 1] = (out_r.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;

            i += 2;
        }
    }
}
