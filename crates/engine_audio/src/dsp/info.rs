//! What the built-in effects are, for files and editors (ADR 0121's effects): each kind's
//! name, its parameters (names, ranges, units, defaults) and how to make one.
//!
//! A filter's shape is its kind (`low_pass`, `peak`, ...), as Godot's filters are, so every
//! parameter is a number. Parameters are named with their units (`attack_ms`, `gain_db`);
//! [`ParamInfo::scale`] takes a value in those units to the effect's own (a delay's
//! milliseconds to its seconds).

use super::{Biquad, Compressor, Delay, Effect, FilterKind, Limiter, Reverb};

/// A parameter's unit, for how an editor shows and types it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// Decibels.
    Db,
    /// Hertz (best dragged on a logarithmic scale).
    Hz,
    /// Milliseconds.
    Ms,
    /// Seconds.
    Seconds,
    /// A ratio such as 4:1.
    Ratio,
    /// A share from 0 to 1 (shown as a percentage).
    Share,
    /// A plain number.
    Number,
}

impl Unit {
    /// What follows a value of this unit when shown (" dB", ":1"; nothing for a number).
    pub fn suffix(self) -> &'static str {
        match self {
            Self::Db => " dB",
            Self::Hz => " Hz",
            Self::Ms => " ms",
            Self::Seconds => " s",
            Self::Ratio => ":1",
            Self::Share => " %",
            Self::Number => "",
        }
    }
}

/// A parameter of an effect kind.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParamInfo {
    /// Its name in files (`threshold_db`).
    pub name: &'static str,
    /// Its name for people ("Threshold").
    pub label: &'static str,
    /// Its index ([`Effect::set_param`]).
    pub index: u32,
    /// The least value it takes, in its unit.
    pub min: f32,
    /// The most.
    pub max: f32,
    /// What a new effect has.
    pub default: f32,
    /// Its unit.
    pub unit: Unit,
    /// The effect's own value for one in [`Self::unit`] (0.001 for milliseconds the effect
    /// takes in seconds).
    pub scale: f32,
}

impl ParamInfo {
    /// `value` (in its unit) within the parameter's range.
    pub fn clamp(&self, value: f32) -> f32 {
        if value.is_nan() {
            self.default
        } else {
            value.clamp(self.min, self.max)
        }
    }

    /// What the effect is given for `value` (in its unit): within range, scaled.
    pub fn effect_value(&self, value: f32) -> f32 {
        self.clamp(value) * self.scale
    }
}

/// A kind of built-in effect.
#[derive(Debug, Clone, Copy)]
pub struct EffectInfo {
    /// Its name in files (`compressor`, `low_pass`).
    pub kind: &'static str,
    /// Its name for people ("Compressor", "Low-pass").
    pub label: &'static str,
    /// Its parameters.
    pub params: &'static [ParamInfo],
    /// Another bus's sound can key it (a compressor ducking music under the voice).
    pub sidechain: bool,
    /// It lowers the gain and says by how much ([`crate::Audio::take_reduction`]).
    pub reduction: bool,
    /// One of the filters: its kind is its shape, and a shape changes to another.
    pub filter: bool,
    make: fn() -> Box<dyn Effect>,
}

impl EffectInfo {
    /// A new effect of this kind, its parameters at their defaults.
    pub fn make(&self) -> Box<dyn Effect> {
        let mut effect = (self.make)();
        for p in self.params {
            effect.set_param(p.index, p.default * p.scale);
        }
        effect
    }

    /// Its parameter called `name` in files.
    pub fn param(&self, name: &str) -> Option<&'static ParamInfo> {
        self.params.iter().find(|p| p.name == name)
    }
}

/// The built-in effects, in the order an editor offers them: the filters, then dynamics,
/// then time.
pub const EFFECTS: &[EffectInfo] = &[
    filter("low_pass", "Low-pass", &LOW_PASS, false, || {
        shape(FilterKind::LowPass)
    }),
    filter("high_pass", "High-pass", &HIGH_PASS, false, || {
        shape(FilterKind::HighPass)
    }),
    filter("band_pass", "Band-pass", &MIDDLE, false, || {
        shape(FilterKind::BandPass)
    }),
    filter("notch", "Notch", &MIDDLE, false, || {
        shape(FilterKind::Notch)
    }),
    filter("peak", "Peak EQ", &MIDDLE, true, || shape(FilterKind::Peak)),
    filter("low_shelf", "Low shelf", &LOW_SHELF, true, || {
        shape(FilterKind::LowShelf)
    }),
    filter("high_shelf", "High shelf", &HIGH_SHELF, true, || {
        shape(FilterKind::HighShelf)
    }),
    filter("all_pass", "All-pass", &ALL_PASS, false, || {
        shape(FilterKind::AllPass)
    }),
    EffectInfo {
        kind: "compressor",
        label: "Compressor",
        params: &[
            param(
                "threshold_db",
                "Threshold",
                Compressor::THRESHOLD_DB,
                -60.0,
                0.0,
                -18.0,
                Unit::Db,
            ),
            param(
                "ratio",
                "Ratio",
                Compressor::RATIO,
                1.0,
                20.0,
                4.0,
                Unit::Ratio,
            ),
            param(
                "attack_ms",
                "Attack",
                Compressor::ATTACK_MS,
                0.1,
                200.0,
                5.0,
                Unit::Ms,
            ),
            param(
                "release_ms",
                "Release",
                Compressor::RELEASE_MS,
                5.0,
                2_000.0,
                120.0,
                Unit::Ms,
            ),
            param(
                "knee_db",
                "Knee",
                Compressor::KNEE_DB,
                0.0,
                24.0,
                6.0,
                Unit::Db,
            ),
            param(
                "makeup_db",
                "Makeup",
                Compressor::MAKEUP_DB,
                -12.0,
                24.0,
                0.0,
                Unit::Db,
            ),
        ],
        sidechain: true,
        reduction: true,
        filter: false,
        make: || Box::new(Compressor::new(-18.0, 4.0)),
    },
    EffectInfo {
        kind: "limiter",
        label: "Limiter",
        params: &[
            param(
                "ceiling_db",
                "Ceiling",
                Limiter::CEILING_DB,
                -24.0,
                0.0,
                -1.0,
                Unit::Db,
            ),
            param(
                "release_ms",
                "Release",
                Limiter::RELEASE_MS,
                5.0,
                1_000.0,
                60.0,
                Unit::Ms,
            ),
        ],
        sidechain: false,
        reduction: true,
        filter: false,
        make: || Box::new(Limiter::new(-1.0)),
    },
    EffectInfo {
        kind: "delay",
        label: "Delay",
        params: &[
            ParamInfo {
                scale: 1e-3,
                ..param(
                    "time_ms",
                    "Time",
                    Delay::TIME,
                    1.0,
                    2_000.0,
                    250.0,
                    Unit::Ms,
                )
            },
            param(
                "feedback",
                "Feedback",
                Delay::FEEDBACK,
                0.0,
                0.98,
                0.35,
                Unit::Share,
            ),
            param("mix", "Mix", Delay::MIX, 0.0, 1.0, 0.35, Unit::Share),
        ],
        sidechain: false,
        reduction: false,
        filter: false,
        make: || Box::new(Delay::new(0.25, 0.35, 0.35)),
    },
    EffectInfo {
        kind: "reverb",
        label: "Reverb",
        params: &[
            param(
                "decay_s",
                "Decay",
                Reverb::DECAY,
                0.1,
                30.0,
                1.5,
                Unit::Seconds,
            ),
            param("size", "Size", Reverb::SIZE, 0.0, 1.0, 0.5, Unit::Share),
            param(
                "damping_hz",
                "Damping",
                Reverb::DAMPING,
                500.0,
                20_000.0,
                8_000.0,
                Unit::Hz,
            ),
            ParamInfo {
                scale: 1e-3,
                ..param(
                    "pre_delay_ms",
                    "Pre-delay",
                    Reverb::PRE_DELAY,
                    0.0,
                    500.0,
                    20.0,
                    Unit::Ms,
                )
            },
            param("early", "Early", Reverb::EARLY, 0.0, 1.0, 0.5, Unit::Share),
            // Fully wet: a reverb is meant for a bus that sends feed.
            param("mix", "Mix", Reverb::MIX, 0.0, 1.0, 1.0, Unit::Share),
        ],
        sidechain: false,
        reduction: false,
        filter: false,
        make: || Box::new(Reverb::new(1.5, 0.5)),
    },
];

/// The built-in effect kind called `kind` in files.
pub fn effect_info(kind: &str) -> Option<&'static EffectInfo> {
    EFFECTS.iter().find(|e| e.kind == kind)
}

const fn param(
    name: &'static str,
    label: &'static str,
    index: u32,
    min: f32,
    max: f32,
    default: f32,
    unit: Unit,
) -> ParamInfo {
    ParamInfo {
        name,
        label,
        index,
        min,
        max,
        default,
        unit,
        scale: 1.0,
    }
}

const FREQUENCY: &str = "frequency_hz";

/// A filter's frequency, Q and gain (peaks and shelves only), with a kind's defaults.
const fn filter_params(frequency: f32, q: f32) -> [ParamInfo; 3] {
    [
        param(
            FREQUENCY,
            "Frequency",
            Biquad::FREQUENCY,
            20.0,
            20_000.0,
            frequency,
            Unit::Hz,
        ),
        param("q", "Q", Biquad::Q, 0.1, 20.0, q, Unit::Number),
        param(
            "gain_db",
            "Gain",
            Biquad::GAIN_DB,
            -24.0,
            24.0,
            0.0,
            Unit::Db,
        ),
    ]
}

const LOW_PASS: [ParamInfo; 3] = filter_params(2_000.0, 0.707);
const HIGH_PASS: [ParamInfo; 3] = filter_params(200.0, 0.707);
const MIDDLE: [ParamInfo; 3] = filter_params(1_000.0, 1.0);
const LOW_SHELF: [ParamInfo; 3] = filter_params(200.0, 0.707);
const HIGH_SHELF: [ParamInfo; 3] = filter_params(4_000.0, 0.707);
const ALL_PASS: [ParamInfo; 3] = filter_params(1_000.0, 0.707);

/// A filter kind; shapes without a gain leave it out.
const fn filter(
    kind: &'static str,
    label: &'static str,
    params: &'static [ParamInfo; 3],
    gain: bool,
    make: fn() -> Box<dyn Effect>,
) -> EffectInfo {
    EffectInfo {
        kind,
        label,
        params: if gain { params } else { params.split_at(2).0 },
        sidechain: false,
        reduction: false,
        filter: true,
        make,
    }
}

fn shape(kind: FilterKind) -> Box<dyn Effect> {
    Box::new(Biquad::new(kind, 1_000.0, 0.707, 0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_is_named_once_and_its_defaults_are_in_range() {
        for (i, info) in EFFECTS.iter().enumerate() {
            assert!(
                EFFECTS[..i].iter().all(|e| e.kind != info.kind),
                "{} twice",
                info.kind
            );
            assert_eq!(effect_info(info.kind).map(|e| e.kind), Some(info.kind));
            for p in info.params {
                assert!(
                    p.min <= p.default && p.default <= p.max,
                    "{}.{}",
                    info.kind,
                    p.name
                );
                assert_eq!(info.param(p.name), Some(p));
            }
            let _ = info.make();
        }
        assert!(effect_info("chorus").is_none());
        let peak = effect_info("peak").unwrap();
        assert!(peak.param("gain_db").is_some() && peak.filter);
        assert!(effect_info("low_pass").unwrap().param("gain_db").is_none());
        assert_eq!(
            effect_info("low_pass")
                .unwrap()
                .param(FREQUENCY)
                .unwrap()
                .default,
            2_000.0
        );
        assert_eq!(
            effect_info("high_shelf")
                .unwrap()
                .param(FREQUENCY)
                .unwrap()
                .default,
            4_000.0
        );
        let time = effect_info("delay").unwrap().param("time_ms").unwrap();
        assert_eq!(time.effect_value(250.0), 0.25);
        assert_eq!(time.effect_value(5_000.0), 2.0, "within range");
        assert_eq!(time.clamp(f32::NAN), 250.0);
    }
}
