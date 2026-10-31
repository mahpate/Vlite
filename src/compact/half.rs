//! P3a canonical float32 to float16 conversion
//! (`docs/projects/roadmap/p3-compact-vectors.md`), the one binary16
//! algorithm of the crate: the dense rows codec stores with it, and replay
//! of a `FLAG_PRECISION` log checks with it that every dense value of a
//! float16 collection is exactly a stored value.

use super::catalog::Precision;

/// Why a float32 value has no stored value in a precision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComponentError {
    /// Infinity or NaN, refused in every precision.
    NonFinite,
    /// Finite, but rounds to a float16 infinity.
    Overflow,
}

impl Precision {
    /// The value stored for `value`: `value` itself in float32, the nearest
    /// float16 (ties to even) otherwise.
    pub(crate) fn canonical(self, value: f32) -> std::result::Result<f32, ComponentError> {
        match self {
            Self::Float32 if value.is_finite() => Ok(value),
            Self::Float32 => Err(ComponentError::NonFinite),
            Self::Float16 => to_half(value).map(from_half),
        }
    }

    /// Whether every component of `vector` is exactly its stored value, bit
    /// for bit, so signed zeros count and nothing is rounded.
    pub(crate) fn holds(self, vector: &[f32]) -> bool {
        vector.iter().all(|&value| {
            self.canonical(value)
                .is_ok_and(|stored| stored.to_bits() == value.to_bits())
        })
    }
}

/// The float16 bits nearest to `value`, ties to even. Signed zero and
/// subnormals are kept; anything that would round to infinity is refused.
pub(crate) fn to_half(value: f32) -> std::result::Result<u16, ComponentError> {
    if !value.is_finite() {
        return Err(ComponentError::NonFinite);
    }
    let bits = value.to_bits();
    let magnitude = bits & 0x7fff_ffff;
    let exponent = magnitude >> 23;
    let half = match exponent {
        // Below half the smallest subnormal, 2^-25: zero.
        ..102 => 0,
        // Subnormal: the significand in units of 2^-24.
        102..113 => round_shift(magnitude & 0x7f_ffff | 0x80_0000, 126 - exponent),
        // Normal: rebias the exponent from 127 to 15. A carry out of the
        // significand correctly bumps the exponent.
        _ => round_shift(magnitude - (112 << 23), 13),
    };
    if half >= 0x7c00 {
        return Err(ComponentError::Overflow);
    }
    Ok((bits >> 16) as u16 & 0x8000 | half as u16)
}

/// `value >> shift`, rounded to nearest, ties to even; `shift` is 13 to 24.
fn round_shift(value: u32, shift: u32) -> u32 {
    let kept = value >> shift;
    let rest = value & ((1 << shift) - 1);
    let half = 1 << (shift - 1);
    kept + u32::from(rest > half || (rest == half && kept & 1 == 1))
}

/// The exact float32 value of float16 `bits`.
pub(crate) fn from_half(bits: u16) -> f32 {
    let sign = u32::from(bits & 0x8000) << 16;
    let exponent = u32::from(bits >> 10 & 0x1f);
    let mantissa = u32::from(bits & 0x3ff);
    let magnitude = match exponent {
        // Subnormal: mantissa * 2^-24, exact in float32.
        0 => (f32::from(bits & 0x3ff) * f32::from_bits(0x3380_0000)).to_bits(),
        0x1f => 0x7f80_0000 | mantissa << 13,
        _ => (exponent + 112) << 23 | mantissa << 13,
    };
    f32::from_bits(sign | magnitude)
}
