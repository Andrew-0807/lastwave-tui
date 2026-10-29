//! 10-band EQ + "Studio Clarity", ported from LastWave's equalizer_models.dart.
//! Rendered to the same mpv `af` chain the desktop app uses.

use serde::{Deserialize, Serialize};

pub const BANDS: [u32; 10] = [31, 63, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
pub const MAX_GAIN: f64 = 8.0;

pub const PRESETS: &[(&str, [f64; 10])] = &[
    ("Default", [0.0; 10]),
    ("Studio Master", [2.7, 2.2, 0.6, -2.6, -0.6, 1.2, 3.0, 4.0, 4.3, 4.8]),
    ("Vocal Clarity", [-1.2, -0.5, 0.0, 1.0, 2.5, 3.5, 2.8, 2.0, 1.5, 1.0]),
    ("Deep Bass Clean", [4.8, 3.5, 2.5, 0.0, -0.5, 0.0, 0.0, 0.0, 0.5, 0.5]),
    ("Rock", [3.8, 3.0, 1.5, -1.0, -0.8, 0.5, 2.0, 3.0, 3.8, 4.0]),
    ("Pop", [-0.8, 0.0, 1.0, 2.0, 1.8, 1.0, 0.5, 1.0, 1.8, 2.5]),
    ("Electronic", [4.2, 3.0, 2.0, -0.5, 0.5, 2.0, 2.5, 2.5, 3.2, 4.0]),
    ("R&B", [4.8, 3.5, 2.0, -0.5, -0.8, 0.5, 1.8, 2.5, 3.0, 3.0]),
    ("Acoustic", [2.8, 2.0, 1.5, 1.0, 1.8, 2.0, 1.2, 1.0, 1.2, 1.5]),
    ("Classical", [2.8, 2.0, 1.5, 0.0, 0.0, 0.5, 1.8, 2.0, 2.8, 3.0]),
    ("Jazz", [2.2, 1.5, 1.0, -0.5, 0.2, 1.0, 1.8, 2.0, 2.5, 2.5]),
    ("Treble Air", [0.0, 0.0, -0.5, -0.5, 0.2, 1.0, 1.8, 3.0, 4.0, 5.0]),
];

/// (name, stage trims) — same 8-stage layout as the Dart ClarityPreset.
pub const CLARITY: &[(&str, [f64; 8])] = &[
    ("Reference", [0.0; 8]),
    ("Speaker", [0.0, -1.5, 0.0, 0.0, 0.0, -0.5, 0.0, 0.0]),
    ("Headphone", [0.0, -0.5, 0.0, 0.0, -1.0, 0.0, 0.0, 0.0]),
    ("DAC", [0.0, 0.0, 0.0, 0.0, 0.0, -1.5, 0.0, -1.0]),
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct EqState {
    pub enabled: bool,
    pub preset: String,
    pub gains: [f64; 10],
    pub clarity: bool,
    pub clarity_preset: usize,
}

impl Default for EqState {
    fn default() -> Self {
        Self { enabled: false, preset: "Default".into(), gains: [0.0; 10], clarity: false, clarity_preset: 0 }
    }
}

impl EqState {
    pub fn combined(&self) -> [f64; 10] {
        let studio = PRESETS[1].1;
        let trims = CLARITY[self.clarity_preset.min(CLARITY.len() - 1)].1;
        let mut out = [0.0; 10];
        for (i, g) in out.iter_mut().enumerate() {
            let mut gain = if self.enabled { self.gains[i] } else { 0.0 };
            if self.clarity {
                let trim = match i {
                    1 => trims[1],
                    3 => trims[2],
                    4 => trims[3],
                    6 | 7 => trims[4],
                    8.. => trims[5],
                    _ => 0.0,
                };
                gain += studio[i] + trim;
            }
            *g = gain.clamp(-MAX_GAIN, MAX_GAIN);
        }
        out
    }

    pub fn active(&self) -> bool {
        self.enabled || self.clarity
    }

    /// mpv `af` value; empty string means no processing.
    pub fn filter(&self) -> String {
        if !self.active() {
            return String::new();
        }
        let gains = self.combined();
        let mut stages: Vec<String> = gains
            .iter()
            .zip(BANDS)
            .filter(|(g, _)| g.abs() >= 0.05)
            .map(|(g, f)| format!("equalizer=f={f}:t=o:w=1:g={g:.1}"))
            .collect();
        if stages.is_empty() {
            return String::new();
        }
        // Headroom: cut by the largest boost first so stacked boosts can't clip.
        let boost = gains.iter().copied().fold(0.0, f64::max);
        if boost >= 0.05 {
            stages.insert(0, format!("volume=volume=-{boost:.1}dB"));
        }
        format!("format=floatp,lavfi=[{}]", stages.join(","))
    }

    pub fn apply_preset(&mut self, idx: usize) {
        let (name, gains) = PRESETS[idx % PRESETS.len()];
        self.preset = name.into();
        self.gains = gains;
        self.enabled = true;
    }

    pub fn preset_index(&self) -> usize {
        PRESETS.iter().position(|(n, _)| *n == self.preset).unwrap_or(0)
    }
}

pub fn band_label(hz: u32) -> String {
    if hz >= 1000 { format!("{}k", hz / 1000) } else { hz.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_has_headroom_for_largest_boost() {
        let mut eq = EqState { enabled: true, ..Default::default() };
        eq.gains[0] = 3.0;
        eq.gains[5] = 6.0;
        eq.gains[9] = -2.0;
        assert_eq!(
            eq.filter(),
            "format=floatp,lavfi=[volume=volume=-6.0dB,equalizer=f=31:t=o:w=1:g=3.0,\
             equalizer=f=1000:t=o:w=1:g=6.0,equalizer=f=16000:t=o:w=1:g=-2.0]"
        );
    }

    #[test]
    fn cuts_only_need_no_preamp() {
        let mut eq = EqState { enabled: true, ..Default::default() };
        eq.gains[2] = -4.0;
        assert_eq!(eq.filter(), "format=floatp,lavfi=[equalizer=f=125:t=o:w=1:g=-4.0]");
        assert_eq!(EqState::default().filter(), "");
    }
}
