//! IEEE 754 half-precision (binary16) conversion, for packing GPU data (colors, small
//! parameters) at half the size.
//!
//! `f16` is not stable in Rust yet. The scalar conversion rounds to nearest-even with the
//! float-arithmetic technique described by Fabian Giesen ("float->half variants", public
//! domain; re-implemented here, no code copied). On x86-64 CPUs with F16C (every CPU with
//! AVX2) [`pack4`] uses the hardware instruction, selected at run time: the baseline is
//! x86-64-v2 (CLAUDE.md §5), which does not include F16C.

/// Converts to half precision (round to nearest even; overflow → ±∞; NaN stays NaN).
pub fn from_f32(value: f32) -> u16 {
    const INFINITY: u32 = 0xFF << 23;
    // 65536.0: the smallest float whose half rounds to infinity regardless of mantissa.
    const HALF_OVERFLOW: u32 = (127 + 16) << 23;
    // 2^-14: the smallest normal half.
    const HALF_MIN_NORMAL: u32 = (127 - 14) << 23;
    // Adding 0.5 (this float) aligns the 10 mantissa bits of a half subnormal at the
    // bottom of the float's mantissa; the FPU does the rounding.
    const SUBNORMAL_MAGIC: u32 = ((127 - 15) + (23 - 10) + 1) << 23;

    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let magnitude = bits & 0x7FFF_FFFF;
    let half = if magnitude >= HALF_OVERFLOW {
        if magnitude > INFINITY { 0x7E00 } else { 0x7C00 }
    } else if magnitude < HALF_MIN_NORMAL {
        let aligned = f32::from_bits(magnitude) + f32::from_bits(SUBNORMAL_MAGIC);
        (aligned.to_bits() - SUBNORMAL_MAGIC) as u16
    } else {
        let odd = (magnitude >> 13) & 1;
        // Rebias the exponent and add just under half an ulp (plus one when the kept
        // mantissa is odd): round to nearest, ties to even. A carry into the exponent is
        // correct, including up to infinity.
        let rounded = magnitude
            .wrapping_add((15u32.wrapping_sub(127) << 23).wrapping_add(0xFFF))
            .wrapping_add(odd);
        (rounded >> 13) as u16
    };
    sign | half
}

/// Converts from half precision (exact).
pub fn to_f32(half: u16) -> f32 {
    let sign = u32::from(half & 0x8000) << 16;
    let exponent = u32::from(half >> 10) & 0x1F;
    let mantissa = u32::from(half & 0x3FF);
    let bits = match exponent {
        0 if mantissa == 0 => sign,
        0 => {
            // Subnormal: value = mantissa × 2^-24.
            let value = mantissa as f32 * f32::from_bits((127 - 24) << 23);
            sign | value.to_bits()
        }
        0x1F => sign | 0x7F80_0000 | (mantissa << 13),
        _ => sign | ((exponent + 127 - 15) << 23) | (mantissa << 13),
    };
    f32::from_bits(bits)
}

/// Four values packed as halves, `v[0]` in the low 16 bits of the first word (the layout
/// of an `f16×4` / two `uint` fields read with `f16tof32` in shaders).
pub fn pack4(v: [f32; 4]) -> [u32; 2] {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("f16c") {
            // SAFETY: F16C is available (checked above).
            return unsafe { pack4_f16c(v) };
        }
    }
    pack4_scalar(v)
}

/// [`pack4`] without the hardware path (for testing and comparison).
pub fn pack4_scalar(v: [f32; 4]) -> [u32; 2] {
    let h = v.map(from_f32);
    [
        u32::from(h[0]) | (u32::from(h[1]) << 16),
        u32::from(h[2]) | (u32::from(h[3]) << 16),
    ]
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "f16c")]
fn pack4_f16c(v: [f32; 4]) -> [u32; 2] {
    use core::arch::x86_64::{
        _MM_FROUND_TO_NEAREST_INT, _mm_cvtps_ph, _mm_cvtsi128_si64, _mm_loadu_ps,
    };
    // SAFETY: `v` is four readable floats; the intrinsics need only F16C and SSE2, which
    // this function's target features guarantee.
    let packed = unsafe {
        let halves = _mm_cvtps_ph::<_MM_FROUND_TO_NEAREST_INT>(_mm_loadu_ps(v.as_ptr()));
        _mm_cvtsi128_si64(halves) as u64
    };
    [packed as u32, (packed >> 32) as u32]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_values_round_trip() {
        for (value, bits) in [
            (0.0f32, 0x0000u16),
            (-0.0, 0x8000),
            (1.0, 0x3C00),
            (-2.0, 0xC000),
            (0.5, 0x3800),
            (65504.0, 0x7BFF),
            (6.103_515_6e-5, 0x0400), // smallest normal
            (5.960_464_5e-8, 0x0001), // smallest subnormal
            (f32::INFINITY, 0x7C00),
            (f32::NEG_INFINITY, 0xFC00),
        ] {
            assert_eq!(from_f32(value), bits, "{value}");
            assert_eq!(to_f32(bits).to_bits(), value.to_bits(), "{bits:#06x}");
        }
        assert!(to_f32(from_f32(f32::NAN)).is_nan());
    }

    #[test]
    fn rounding_and_overflow() {
        // 1 + 2^-11 is halfway between 1 and 1 + 2^-10: ties to even (1.0).
        assert_eq!(from_f32(1.0 + 2f32.powi(-11)), 0x3C00);
        // 1 + 3 × 2^-11 is halfway between 1 + 2^-10 (odd) and 1 + 2^-9: rounds up.
        assert_eq!(from_f32(1.0 + 3.0 * 2f32.powi(-11)), 0x3C02);
        assert_eq!(from_f32(65519.0), 0x7BFF);
        assert_eq!(from_f32(65520.0), 0x7C00);
        assert_eq!(from_f32(1e10), 0x7C00);
        assert_eq!(from_f32(1e-10), 0x0000);
    }

    /// Every half value converts to a float and back unchanged, and every float rounds to
    /// the nearest half (checked against the neighbors) on a dense sweep.
    #[test]
    fn exhaustive_halves_and_nearest_rounding() {
        for bits in 0..=u16::MAX {
            let value = to_f32(bits);
            if value.is_nan() {
                assert!(to_f32(from_f32(value)).is_nan());
            } else {
                assert_eq!(from_f32(value), bits, "{bits:#06x}");
            }
        }
        // Up to the largest half (overflow is checked in `rounding_and_overflow`).
        let mut x = 1e-9f32;
        while x <= 65504.0 {
            let h = from_f32(x);
            let err = (to_f32(h) - x).abs();
            for neighbor in [h.wrapping_sub(1), h.wrapping_add(1)] {
                let n = to_f32(neighbor);
                if n.is_finite() && neighbor & 0x7C00 != 0x7C00 {
                    assert!(err <= (n - x).abs(), "{x}: {h:#06x} vs {neighbor:#06x}");
                }
            }
            x *= 1.000_37;
        }
    }

    #[test]
    fn hardware_and_scalar_packing_agree() {
        let mut x = -70000.0f32;
        while x < 70000.0 {
            let v = [x, x * 0.001, -x * 1e-6, x.recip()];
            assert_eq!(pack4(v), pack4_scalar(v), "{v:?}");
            x += 13.37;
        }
        assert_eq!(pack4([1.0, 0.0, -2.0, 0.5]), [0x0000_3C00, 0x3800_C000]);
    }
}
