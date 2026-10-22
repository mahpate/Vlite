//! Approximate-index options and the scalar-u8 traversal codec.
//!
//! Quantization only changes how the in-memory HNSW graph scores candidates
//! during traversal. Stored records stay f32, every returned distance is
//! recomputed exactly from the current f32 record. The datafile never stores
//! codes; only the optional, validated ANN snapshot sidecar may cache them.

use std::io::{self, Read, Write};

use crate::{
    DistanceMetric, Error, Result,
    ann_snapshot::{Parse, Source, corrupt},
};

/// Vector encoding used for HNSW traversal. Stored records always stay f32.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Quantization {
    /// Full-precision f32 traversal (existing behavior).
    #[default]
    None,
    /// Per-dimension scalar u8 codes with exact f32 reranking.
    ScalarU8,
}

impl Quantization {
    pub(crate) fn code(self) -> u8 {
        match self {
            Self::None => 0,
            Self::ScalarU8 => 1,
        }
    }

    pub(crate) fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::None),
            1 => Some(Self::ScalarU8),
            _ => None,
        }
    }
}

/// Options that, together with `HnswConfig` and metric, identify an ANN index.
///
/// `rerank_multiplier` is valid in `1..=16`; the default is 4. With
/// `ScalarU8`, the nearest `min(ef_search, k × rerank_multiplier)` (at least
/// `k`) eligible graph candidates are re-scored with exact f32 distances. It
/// is ignored for `Quantization::None`, whose distances are already exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnnOptions {
    pub quantization: Quantization,
    pub rerank_multiplier: u8,
}

impl Default for AnnOptions {
    fn default() -> Self {
        Self {
            quantization: Quantization::None,
            rerank_multiplier: 4,
        }
    }
}

impl AnnOptions {
    /// Rejects `rerank_multiplier` outside `1..=16` with `InvalidQuery`.
    pub(crate) fn validate(self) -> Result<()> {
        if (1..=16).contains(&self.rerank_multiplier) {
            Ok(())
        } else {
            Err(Error::InvalidQuery("rerank_multiplier must be 1..=16"))
        }
    }

    /// Exact rerank width for one query: `min(ef, k × multiplier)`, at least `k`.
    pub(crate) fn rerank_width(self, k: usize, ef: usize) -> usize {
        ef.min(k.saturating_mul(usize::from(self.rerank_multiplier)))
            .max(k)
    }
}

/// Per-dimension min/max scalar quantizer trained on one build snapshot.
///
/// `code = round((x − min) / scale)` with `scale = (max − min) / 255`; a
/// constant dimension (scale 0) always encodes to 0. Values outside the
/// trained range clamp to 0 or 255. Bounds are kept in f64 so extreme finite
/// f32 ranges cannot overflow.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ScalarU8 {
    min: Vec<f64>,
    scale: Vec<f64>,
}

impl ScalarU8 {
    /// Trains bounds over `vectors`, each exactly `dimensions` long.
    pub fn train<'a>(dimensions: usize, vectors: impl IntoIterator<Item = &'a [f32]>) -> Self {
        let mut min = vec![f64::INFINITY; dimensions];
        let mut max = vec![f64::NEG_INFINITY; dimensions];
        for vector in vectors {
            for ((value, low), high) in vector.iter().zip(&mut min).zip(&mut max) {
                let value = f64::from(*value);
                *low = low.min(value);
                *high = high.max(value);
            }
        }
        let scale = min
            .iter_mut()
            .zip(&max)
            .map(|(low, &high)| {
                if !low.is_finite() {
                    // No training vectors: every code decodes to 0.0.
                    *low = 0.0;
                    return 0.0;
                }
                (high - *low) / 255.0
            })
            .collect();
        Self { min, scale }
    }

    pub fn dimensions(&self) -> usize {
        self.min.len()
    }

    /// Bytes held by the per-dimension bounds.
    pub fn codebook_bytes(&self) -> usize {
        (self.min.len() + self.scale.len()) * size_of::<f64>()
    }

    /// Appends one code per dimension to `codes`.
    pub fn encode_into(&self, vector: &[f32], codes: &mut Vec<u8>) {
        codes.extend(vector.iter().zip(self.min.iter().zip(&self.scale)).map(
            |(&value, (&min, &scale))| {
                if scale == 0.0 {
                    0
                } else {
                    ((f64::from(value) - min) / scale).round().clamp(0.0, 255.0) as u8
                }
            },
        ));
    }

    fn decoded(&self, codes: &[u8]) -> impl Iterator<Item = f64> {
        codes
            .iter()
            .zip(self.min.iter().zip(&self.scale))
            .map(|(&code, (&min, &scale))| min + f64::from(code) * scale)
    }

    /// Dequantized vector, used as a construction-time query.
    pub fn decode(&self, codes: &[u8]) -> Vec<f32> {
        self.decoded(codes).map(|value| value as f32).collect()
    }

    /// Euclidean norm of the dequantized vector.
    pub fn norm(&self, codes: &[u8]) -> f64 {
        self.decoded(codes)
            .map(|value| value * value)
            .sum::<f64>()
            .sqrt()
    }

    /// ANN snapshot body: `dimensions u32`, then `min` and `scale` as f64 bits.
    pub(crate) fn write_snapshot(&self, w: &mut impl Write) -> io::Result<()> {
        let dimensions = u32::try_from(self.dimensions())
            .map_err(|_| io::Error::other("quantizer dimensions exceed snapshot format"))?;
        w.write_all(&dimensions.to_le_bytes())?;
        for value in self.min.iter().chain(&self.scale) {
            w.write_all(&value.to_bits().to_le_bytes())?;
        }
        Ok(())
    }

    /// Reads a codec written by `write_snapshot`. Its dimension count must be
    /// `dimensions` or 0 (a quantizer trained on an empty collection); bounds
    /// must be finite with non-negative scales, as `train` produces.
    pub(crate) fn read_snapshot<R: Read>(source: &mut Source<R>, dimensions: usize) -> Parse<Self> {
        let stored = source.u32()? as usize;
        if stored != 0 && stored != dimensions {
            return Err(corrupt("quantizer dimensions"));
        }
        let min = source.f64s(stored)?;
        let scale = source.f64s(stored)?;
        if min.iter().any(|value| !value.is_finite())
            || scale
                .iter()
                .any(|value| !value.is_finite() || value.is_sign_negative())
        {
            return Err(corrupt("quantizer bounds"));
        }
        Ok(Self { min, scale })
    }

    /// Asymmetric distance between an f32 query and one code vector.
    ///
    /// Squared L2 and cosine only. A zero dequantized norm scores cosine 1.0
    /// (orthogonal) so traversal stays total; final distances are exact.
    pub fn distance(
        &self,
        metric: DistanceMetric,
        query: &[f32],
        query_norm: f64,
        codes: &[u8],
        codes_norm: f64,
    ) -> f64 {
        if metric == DistanceMetric::SquaredL2 {
            return query
                .iter()
                .zip(self.decoded(codes))
                .map(|(&query, value)| {
                    let difference = f64::from(query) - value;
                    difference * difference
                })
                .sum();
        }
        if query_norm == 0.0 || codes_norm == 0.0 {
            return 1.0;
        }
        let dot: f64 = query
            .iter()
            .zip(self.decoded(codes))
            .map(|(&query, value)| f64::from(query) * value)
            .sum();
        1.0 - (dot / (query_norm * codes_norm)).clamp(-1.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_follow_documented_rounding_clamping_and_constant_dimensions() {
        let a = [0.0f32, 5.0, -1.0];
        let b = [255.0f32, 5.0, 1.0];
        let quantizer = ScalarU8::train(3, [&a[..], &b[..]]);
        let mut codes = Vec::new();
        quantizer.encode_into(&[127.4, 5.0, 0.0], &mut codes);
        quantizer.encode_into(&[1000.0, -7.0, -9.0], &mut codes);
        assert_eq!(codes, [127, 0, 128, 255, 0, 0]);
        let decoded = quantizer.decode(&codes[..3]);
        assert_eq!(decoded[..2], [127.0, 5.0]);
        assert!((decoded[2] - 1.0 / 255.0).abs() < 1e-6);
        assert_eq!(quantizer.codebook_bytes(), 48);
        let empty = ScalarU8::train(2, std::iter::empty());
        let mut codes = Vec::new();
        empty.encode_into(&[3.0, -3.0], &mut codes);
        assert_eq!((codes, empty.decode(&[0, 0])), (vec![0, 0], vec![0.0, 0.0]));
    }

    #[test]
    fn asymmetric_distances_track_exact_distances_within_quantization_error() {
        let vectors: Vec<Vec<f32>> = (0..64)
            .map(|i| vec![(i as f32).sin(), (i as f32 * 0.7).cos(), i as f32 / 64.0])
            .collect();
        let quantizer = ScalarU8::train(3, vectors.iter().map(Vec::as_slice));
        let query = [0.25f32, -0.5, 0.75];
        let query_norm = crate::vector_norm(&query);
        for vector in &vectors {
            let mut codes = Vec::new();
            quantizer.encode_into(vector, &mut codes);
            let exact = crate::metric_distance(vector, &query, DistanceMetric::SquaredL2, 0.0, 0.0)
                .unwrap();
            let approximate =
                quantizer.distance(DistanceMetric::SquaredL2, &query, 0.0, &codes, 0.0);
            assert!((exact - approximate).abs() < 0.02, "{exact} {approximate}");
            let norm = quantizer.norm(&codes);
            let exact = crate::metric_distance(
                vector,
                &query,
                DistanceMetric::Cosine,
                crate::vector_norm(vector),
                query_norm,
            )
            .unwrap();
            let approximate =
                quantizer.distance(DistanceMetric::Cosine, &query, query_norm, &codes, norm);
            assert!((exact - approximate).abs() < 0.05, "{exact} {approximate}");
        }
        assert_eq!(
            quantizer.distance(DistanceMetric::Cosine, &query, query_norm, &[0, 0, 0], 0.0),
            1.0
        );
    }

    #[test]
    fn rerank_width_and_multiplier_bounds() {
        let options = AnnOptions::default();
        assert_eq!(options.rerank_width(10, 100), 40);
        assert_eq!(options.rerank_width(10, 20), 20);
        assert_eq!(options.rerank_width(10, 5), 10);
        for multiplier in [0, 17] {
            let options = AnnOptions {
                quantization: Quantization::ScalarU8,
                rerank_multiplier: multiplier,
            };
            assert!(matches!(options.validate(), Err(Error::InvalidQuery(_))));
        }
    }
}
