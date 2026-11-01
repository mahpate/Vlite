//! Exact f64 scoring kernels that score several rows per pass (Speed P3d).
//!
//! `metric_distance` stays the oracle. Per dimension it widens both f32
//! components to f64 (exact) and forms `row - query` and its square, or
//! `row * query`, each one IEEE rounding with no fused multiply-add. It then
//! folds the terms left to right with `Iterator::sum`. That fold must not be
//! reassociated, so these kernels instead score up to `LANES` rows per pass.
//! Each row owns one f64 accumulator lane that performs the oracle's exact
//! sequence of operations, starting from the standard library's own empty
//! sum. Cosine row norms are folded the same way as `vector_norm`: the square
//! of a widened f32 is exact, so `powi(2)` and `x * x` agree. The final
//! metric step is the oracle's scalar code. Rows whose length differs from
//! the query are scored by the oracle itself.
//!
//! Dispatch is detected once: AVX2 on x86_64 when the CPU has it, otherwise
//! the portable kernel (SSE2 scalar on x86_64, scalar elsewhere, including
//! WebAssembly). Neither kernel enables FMA.

use crate::DistanceMetric;

/// Rows scored per pass.
pub(crate) const LANES: usize = 8;
/// Dimensions between squared-L2 cutoff checks.
const BLOCK: usize = 32;

/// What a pass accumulates per row.
const SQUARED_L2: u8 = 0;
const DOT: u8 = 1;
/// The dot product and the row's squared norm, for cosine without stored norms.
const DOT_AND_NORM: u8 = 2;

/// Per-row metric sums and (for `DOT_AND_NORM`) squared norms.
type Sums = ([f64; LANES], [f64; LANES]);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kernel {
    /// One scalar accumulator per row.
    Portable,
    /// Four rows per 256-bit vector of f64 accumulators.
    #[cfg(target_arch = "x86_64")]
    Avx2,
}

impl Kernel {
    /// The fastest kernel this CPU supports.
    pub(crate) fn detected() -> Self {
        #[cfg(target_arch = "x86_64")]
        if avx2::available() {
            return Self::Avx2;
        }
        Self::Portable
    }
}

#[cfg(test)]
thread_local! {
    static FORCED: std::cell::Cell<Option<Kernel>> = const { std::cell::Cell::new(None) };
}

/// Runs `run` with every `Scorer` created on this thread using `kernel`.
#[cfg(test)]
pub(crate) fn force_kernel<R>(kernel: Kernel, run: impl FnOnce() -> R) -> R {
    struct Restore(Option<Kernel>);
    impl Drop for Restore {
        fn drop(&mut self) {
            FORCED.set(self.0);
        }
    }
    #[cfg(target_arch = "x86_64")]
    assert!(kernel != Kernel::Avx2 || avx2::available());
    let _restore = Restore(FORCED.replace(Some(kernel)));
    run()
}

/// The standard library's empty f64 sum (`-0.0` on current toolchains), where
/// every `Iterator::sum` fold in the oracle starts.
fn seed() -> f64 {
    std::iter::empty::<f64>().sum()
}

/// Scores rows against one query exactly as `metric_distance` does.
pub(crate) struct Scorer<'q> {
    query: &'q [f32],
    /// The query widened to f64 once; widening is exact.
    wide: Vec<f64>,
    metric: DistanceMetric,
    query_norm: f64,
    kernel: Kernel,
}

impl<'q> Scorer<'q> {
    pub(crate) fn new(query: &'q [f32], metric: DistanceMetric, query_norm: f64) -> Self {
        #[cfg(test)]
        let kernel = FORCED.get().unwrap_or_else(Kernel::detected);
        #[cfg(not(test))]
        let kernel = Kernel::detected();
        Self {
            query,
            wide: query.iter().map(|&value| f64::from(value)).collect(),
            metric,
            query_norm,
            kernel,
        }
    }

    #[cfg(test)]
    pub(crate) fn kernel(&self) -> Kernel {
        self.kernel
    }

    /// `metric_distance(row, query, metric, norm, query_norm)` for each row,
    /// where `norm` is `norms[i]` if supplied and otherwise
    /// `vector_norm(row)`. Only cosine reads it.
    pub(crate) fn score(&self, rows: &[&[f32]], norms: Option<&[f64]>) -> Vec<Option<f64>> {
        let mut out = vec![None; rows.len()];
        for (batch, (rows, out)) in rows.chunks(LANES).zip(out.chunks_mut(LANES)).enumerate() {
            let norms = norms.map(|norms| &norms[batch * LANES..][..rows.len()]);
            self.score_batch(rows, norms, f64::INFINITY, out);
        }
        out
    }

    /// Scores 1..=`LANES` rows into `out` like `score`. For squared L2 only,
    /// returns false, leaving `out` unspecified, once every row's running sum
    /// exceeds `cutoff`. The remaining terms are nonnegative, so each full
    /// distance would exceed it too. Other metrics ignore `cutoff`.
    pub(crate) fn score_batch(
        &self,
        rows: &[&[f32]],
        norms: Option<&[f64]>,
        cutoff: f64,
        out: &mut [Option<f64>],
    ) -> bool {
        assert!((1..=LANES).contains(&rows.len()) && out.len() >= rows.len());
        if rows.iter().any(|row| row.len() != self.query.len()) {
            for (index, row) in rows.iter().enumerate() {
                let norm = match norms {
                    Some(norms) => norms[index],
                    None if self.metric == DistanceMetric::Cosine => crate::vector_norm(row),
                    None => 0.0,
                };
                out[index] =
                    crate::metric_distance(row, self.query, self.metric, norm, self.query_norm);
            }
            return true;
        }
        // Spare lanes repeat the first row, so they never change a cutoff test.
        let padded: [&[f32]; LANES] =
            std::array::from_fn(|lane| rows.get(lane).copied().unwrap_or(rows[0]));
        let sums = match self.metric {
            DistanceMetric::SquaredL2 => self.sums::<SQUARED_L2>(&padded, cutoff),
            DistanceMetric::Cosine if norms.is_none() => {
                self.sums::<DOT_AND_NORM>(&padded, f64::INFINITY)
            }
            _ => self.sums::<DOT>(&padded, f64::INFINITY),
        };
        let Some((sums, squares)) = sums else {
            return false;
        };
        for (index, out) in out[..rows.len()].iter_mut().enumerate() {
            let norm = norms.map_or_else(|| squares[index].sqrt(), |norms| norms[index]);
            *out = finish(self.metric, sums[index], norm, self.query_norm);
        }
        true
    }

    fn sums<const KIND: u8>(&self, rows: &[&[f32]; LANES], cutoff: f64) -> Option<Sums> {
        match self.kernel {
            #[cfg(target_arch = "x86_64")]
            // SAFETY: `Kernel::Avx2` is only selected after AVX2 was detected.
            Kernel::Avx2 => unsafe { avx2::sums::<KIND>(&self.wide, rows, cutoff) },
            Kernel::Portable => portable_sums::<KIND>(&self.wide, rows, cutoff),
        }
    }
}

/// `metric_distance` after its fold, unchanged.
fn finish(metric: DistanceMetric, sum: f64, row_norm: f64, query_norm: f64) -> Option<f64> {
    match metric {
        DistanceMetric::SquaredL2 => Some(sum),
        DistanceMetric::InnerProduct => Some(if sum == 0.0 { 0.0 } else { -sum }),
        DistanceMetric::Cosine if row_norm == 0.0 || query_norm == 0.0 => None,
        DistanceMetric::Cosine => Some(1.0 - (sum / (row_norm * query_norm)).clamp(-1.0, 1.0)),
    }
}

/// Adds one dimension's term to a row's accumulators, in the oracle's order.
#[inline(always)]
fn accumulate<const KIND: u8>(sum: &mut f64, square: &mut f64, row: f32, query: f64) {
    let row = f64::from(row);
    match KIND {
        SQUARED_L2 => {
            let difference = row - query;
            *sum += difference * difference;
        }
        DOT => *sum += row * query,
        _ => {
            *sum += row * query;
            *square += row * row;
        }
    }
}

/// One scalar accumulator per row. The rows are independent, so the CPU
/// overlaps their additions without any reassociation within a row.
fn portable_sums<const KIND: u8>(
    query: &[f64],
    rows: &[&[f32]; LANES],
    cutoff: f64,
) -> Option<Sums> {
    let dims = query.len();
    let rows = rows.map(|row| &row[..dims]);
    let (mut sums, mut squares) = ([seed(); LANES], [seed(); LANES]);
    let mut start = 0;
    while start < dims {
        let end = (start + BLOCK).min(dims);
        for (dim, &query) in query.iter().enumerate().take(end).skip(start) {
            for lane in 0..LANES {
                accumulate::<KIND>(&mut sums[lane], &mut squares[lane], rows[lane][dim], query);
            }
        }
        if KIND == SQUARED_L2 && end < dims && sums.iter().all(|&sum| sum > cutoff) {
            return None;
        }
        start = end;
    }
    Some((sums, squares))
}

#[cfg(target_arch = "x86_64")]
mod avx2 {
    use super::{BLOCK, DOT, LANES, SQUARED_L2, Sums, accumulate, seed};
    use std::arch::x86_64::{
        __m128, __m256d, _CMP_GT_OQ, _mm_loadu_ps, _mm_movehl_ps, _mm_movelh_ps, _mm_unpackhi_ps,
        _mm_unpacklo_ps, _mm256_add_pd, _mm256_cmp_pd, _mm256_cvtps_pd, _mm256_movemask_pd,
        _mm256_mul_pd, _mm256_set1_pd, _mm256_storeu_pd, _mm256_sub_pd,
    };

    /// Rows per 256-bit vector of f64 lanes.
    const WIDTH: usize = 4;
    const GROUPS: usize = LANES / WIDTH;

    pub(super) fn available() -> bool {
        static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *AVAILABLE.get_or_init(|| std::arch::is_x86_feature_detected!("avx2"))
    }

    /// Dimensions `4 * step..4 * step + 4` of four rows, transposed so that
    /// column `k` holds dimension `4 * step + k` of each row, widened to f64.
    #[target_feature(enable = "avx2")]
    fn columns(rows: &[&[[f32; 4]]], step: usize) -> [__m256d; 4] {
        let load = |row: &[[f32; 4]]| -> __m128 {
            let chunk = &row[step];
            // SAFETY: `chunk` is four readable f32s; the load is unaligned.
            unsafe { _mm_loadu_ps(chunk.as_ptr()) }
        };
        let (r0, r1, r2, r3) = (load(rows[0]), load(rows[1]), load(rows[2]), load(rows[3]));
        let low01 = _mm_unpacklo_ps(r0, r1);
        let low23 = _mm_unpacklo_ps(r2, r3);
        let high01 = _mm_unpackhi_ps(r0, r1);
        let high23 = _mm_unpackhi_ps(r2, r3);
        [
            _mm256_cvtps_pd(_mm_movelh_ps(low01, low23)),
            _mm256_cvtps_pd(_mm_movehl_ps(low23, low01)),
            _mm256_cvtps_pd(_mm_movelh_ps(high01, high23)),
            _mm256_cvtps_pd(_mm_movehl_ps(high23, high01)),
        ]
    }

    /// `portable_sums` with row `WIDTH * g + i` in lane `i` of group `g`.
    /// Each lane runs the same subtract, multiply and add instructions in
    /// the same order; FMA is not enabled. The last `dims % 4` dimensions
    /// continue each lane in scalar code.
    #[target_feature(enable = "avx2")]
    pub(super) fn sums<const KIND: u8>(
        query: &[f64],
        rows: &[&[f32]; LANES],
        cutoff: f64,
    ) -> Option<Sums> {
        let dims = query.len();
        let rows = rows.map(|row| &row[..dims]);
        let chunks = rows.map(|row| row.as_chunks::<4>().0);
        let mut sums = [_mm256_set1_pd(seed()); GROUPS];
        let mut squares = sums;
        let limit = _mm256_set1_pd(cutoff);
        let steps = dims / 4;
        for step in 0..steps {
            for group in 0..GROUPS {
                let columns = columns(&chunks[group * WIDTH..][..WIDTH], step);
                for (offset, row) in columns.into_iter().enumerate() {
                    let query = _mm256_set1_pd(query[4 * step + offset]);
                    if KIND == SQUARED_L2 {
                        let difference = _mm256_sub_pd(row, query);
                        sums[group] =
                            _mm256_add_pd(sums[group], _mm256_mul_pd(difference, difference));
                    } else {
                        sums[group] = _mm256_add_pd(sums[group], _mm256_mul_pd(row, query));
                        if KIND != DOT {
                            squares[group] = _mm256_add_pd(squares[group], _mm256_mul_pd(row, row));
                        }
                    }
                }
            }
            let done = 4 * (step + 1);
            if KIND == SQUARED_L2
                && done % BLOCK == 0
                && done < dims
                && sums.iter().all(|&sum| {
                    _mm256_movemask_pd(_mm256_cmp_pd::<_CMP_GT_OQ>(sum, limit)) == 0b1111
                })
            {
                return None;
            }
        }
        let (mut lane_sums, mut lane_squares) = ([0.0; LANES], [0.0; LANES]);
        for (group, (sum, square)) in sums.iter().zip(&squares).enumerate() {
            // SAFETY: each destination is four writable f64s; stores are unaligned.
            unsafe {
                _mm256_storeu_pd(lane_sums.as_chunks_mut::<4>().0[group].as_mut_ptr(), *sum);
                _mm256_storeu_pd(
                    lane_squares.as_chunks_mut::<4>().0[group].as_mut_ptr(),
                    *square,
                );
            }
        }
        for (dim, &query) in query.iter().enumerate().skip(4 * steps) {
            for lane in 0..LANES {
                accumulate::<KIND>(
                    &mut lane_sums[lane],
                    &mut lane_squares[lane],
                    rows[lane][dim],
                    query,
                );
            }
        }
        Some((lane_sums, lane_squares))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::RecordMap;
    use crate::{
        Condition, CreateOptions, Database, DistanceMetric, HnswConfig, Metadata, MetadataValue,
        Record, SearchHit, SearchStats, metric_distance, vector_norm,
    };
    use std::collections::{BTreeMap, BTreeSet};

    const METRICS: [DistanceMetric; 3] = [
        DistanceMetric::SquaredL2,
        DistanceMetric::Cosine,
        DistanceMetric::InnerProduct,
    ];

    /// Every kernel this host can run; AVX2 is skipped (and reported) when absent.
    fn kernels() -> Vec<Kernel> {
        #[allow(unused_mut)]
        let mut kernels = vec![Kernel::Portable];
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("avx2") {
            kernels.push(Kernel::Avx2);
        } else {
            eprintln!("AVX2 unavailable: only the portable kernel is exercised");
        }
        kernels
    }

    struct Rng(u64);

    impl Rng {
        fn bits(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 16
        }

        fn unit(&mut self) -> f32 {
            (self.bits() >> 24) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
        }

        /// Values over ~80 binades, so sums round and cancel at every step.
        fn wide(&mut self) -> f32 {
            let exponent = (self.bits() % 81) as i32 - 40;
            self.unit() * 2f32.powi(exponent)
        }

        fn pick(&mut self, values: &[f32]) -> f32 {
            values[self.bits() as usize % values.len()]
        }
    }

    /// `metric_distance` as the exact call sites use it: row first, and the
    /// row norm from `vector_norm` unless the caller supplies a stored one.
    fn oracle(
        row: &[f32],
        query: &[f32],
        metric: DistanceMetric,
        norm: Option<f64>,
        query_norm: f64,
    ) -> Option<f64> {
        let norm = norm.unwrap_or_else(|| {
            if metric == DistanceMetric::Cosine {
                vector_norm(row)
            } else {
                0.0
            }
        });
        metric_distance(row, query, metric, norm, query_norm)
    }

    fn query_norm(query: &[f32], metric: DistanceMetric) -> f64 {
        if metric == DistanceMetric::Cosine {
            vector_norm(query)
        } else {
            0.0
        }
    }

    /// Bit equality; Rust leaves NaN payload and sign unspecified, so two
    /// NaNs match.
    fn same(actual: Option<f64>, expected: Option<f64>) -> bool {
        match (actual, expected) {
            (None, None) => true,
            (Some(a), Some(b)) => a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan()),
            _ => false,
        }
    }

    /// Scores `rows` with and without supplied norms for every metric and
    /// compares each result with the oracle. For finite inputs the oracle is
    /// also evaluated query-first, as the rerank and named-vector paths call
    /// it, and must give the same bits.
    fn check(kernel: Kernel, query: &[f32], rows: &[Vec<f32>], label: &str) {
        let refs: Vec<&[f32]> = rows.iter().map(Vec::as_slice).collect();
        let norms: Vec<f64> = rows.iter().map(|row| vector_norm(row)).collect();
        let finite = query
            .iter()
            .chain(rows.iter().flatten())
            .all(|v| v.is_finite());
        force_kernel(kernel, || {
            for metric in METRICS {
                let query_norm = query_norm(query, metric);
                let scorer = Scorer::new(query, metric, query_norm);
                assert_eq!(scorer.kernel(), kernel);
                for supplied in [false, true] {
                    let actual = scorer.score(&refs, supplied.then_some(&norms[..]));
                    assert_eq!(actual.len(), rows.len());
                    for (index, row) in rows.iter().enumerate() {
                        let norm = supplied.then_some(norms[index]);
                        let expected = oracle(row, query, metric, norm, query_norm);
                        assert!(
                            same(actual[index], expected),
                            "{label} {kernel:?} {metric:?} supplied={supplied} row {index}: \
                             {:?} vs {expected:?}",
                            actual[index]
                        );
                        if finite {
                            let swapped =
                                metric_distance(query, row, metric, query_norm, norms[index]);
                            assert!(same(actual[index], swapped), "{label} swapped {metric:?}");
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn random_rows_match_metric_distance_bits_for_every_length_and_batch_size() {
        for kernel in kernels() {
            let mut rng = Rng(0x5eed);
            for dims in (0..=67).chain([128, 384, 768, 1536]) {
                let query: Vec<f32> = (0..dims).map(|_| rng.wide()).collect();
                for count in 1..=9 {
                    let rows: Vec<Vec<f32>> = (0..count)
                        .map(|_| (0..dims).map(|_| rng.wide()).collect())
                        .collect();
                    check(kernel, &query, &rows, &format!("dims={dims} count={count}"));
                }
            }
        }
    }

    #[test]
    fn adversarial_values_match_metric_distance_bits() {
        let finite = [
            0.0,
            -0.0,
            f32::from_bits(1),
            -f32::from_bits(1),
            f32::from_bits(0x007f_ffff),
            f32::MIN_POSITIVE,
            -f32::MIN_POSITIVE,
            f32::EPSILON,
            1.0,
            -1.0,
            1.5,
            3.0e-30,
            -7.0e29,
            1.0e30,
            f32::MAX,
            f32::MIN,
        ];
        let special: Vec<f32> = finite
            .iter()
            .copied()
            .chain([f32::INFINITY, f32::NEG_INFINITY, f32::NAN, -f32::NAN])
            .collect();
        for kernel in kernels() {
            let mut rng = Rng(42);
            for dims in [
                0usize, 1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65,
            ] {
                for values in [&finite[..], &special[..]] {
                    for count in [1, 3, 8, 9] {
                        let query: Vec<f32> = (0..dims).map(|_| rng.pick(values)).collect();
                        let rows: Vec<Vec<f32>> = (0..count)
                            .map(|_| (0..dims).map(|_| rng.pick(values)).collect())
                            .collect();
                        check(kernel, &query, &rows, &format!("picked dims={dims}"));
                    }
                }
                // Signed zeros, zero norms, an identical row, cancellation
                // that reassociation would change, the largest and smallest
                // magnitudes, and non-finite rows next to finite ones.
                let alternating: Vec<f32> = (0..dims)
                    .map(|dim| match dim % 4 {
                        0 => 1.0e30,
                        2 => -1.0e30,
                        _ => 1.0,
                    })
                    .collect();
                let query: Vec<f32> = (0..dims).map(|_| rng.wide()).collect();
                let rows = vec![
                    vec![-0.0; dims],
                    vec![0.0; dims],
                    query.clone(),
                    alternating.clone(),
                    vec![f32::MAX; dims],
                    vec![f32::from_bits(1); dims],
                    (0..dims)
                        .map(|dim| if dim == 0 { f32::NAN } else { 1.0 })
                        .collect(),
                    (0..dims)
                        .map(|dim| if dim == dims / 2 { f32::INFINITY } else { -1.0 })
                        .collect(),
                    vec![f32::MIN; dims],
                ];
                check(kernel, &query, &rows, &format!("structured dims={dims}"));
                for query in [
                    vec![-0.0; dims],
                    vec![1.0; dims],
                    vec![0.0; dims],
                    alternating,
                ] {
                    check(
                        kernel,
                        &query,
                        &rows,
                        &format!("structured query dims={dims}"),
                    );
                }
                // Lengths that differ from the query use the oracle's zip.
                let query: Vec<f32> = (0..dims).map(|_| rng.wide()).collect();
                let rows: Vec<Vec<f32>> = [0, dims.saturating_sub(1), dims + 1, dims, dims + 5]
                    .iter()
                    .map(|&length| (0..length).map(|_| rng.wide()).collect())
                    .collect();
                check(kernel, &query, &rows, &format!("mismatched dims={dims}"));
            }
        }
    }

    #[test]
    fn squared_l2_cutoff_only_drops_batches_whose_rows_all_exceed_it() {
        let mut pruned = 0;
        for kernel in kernels() {
            let mut rng = Rng(9);
            for dims in [1usize, 4, 31, 32, 33, 64, 100, 384] {
                let query: Vec<f32> = (0..dims).map(|_| rng.unit()).collect();
                for count in 1..=LANES {
                    let rows: Vec<Vec<f32>> = (0..count)
                        .map(|_| (0..dims).map(|_| rng.unit() * 2.0).collect())
                        .collect();
                    let refs: Vec<&[f32]> = rows.iter().map(Vec::as_slice).collect();
                    for metric in METRICS {
                        let query_norm = query_norm(&query, metric);
                        let expected: Vec<Option<f64>> = rows
                            .iter()
                            .map(|row| oracle(row, &query, metric, None, query_norm))
                            .collect();
                        let mut sorted: Vec<f64> = expected.iter().flatten().copied().collect();
                        sorted.sort_by(f64::total_cmp);
                        let cutoffs = [
                            -1.0,
                            0.0,
                            sorted[0],
                            sorted[sorted.len() / 2],
                            sorted[sorted.len() - 1],
                            f64::INFINITY,
                            f64::NAN,
                        ];
                        force_kernel(kernel, || {
                            let scorer = Scorer::new(&query, metric, query_norm);
                            for cutoff in cutoffs {
                                let mut out = [Some(f64::NAN); LANES];
                                let kept = scorer.score_batch(&refs, None, cutoff, &mut out);
                                let all_exceed =
                                    expected.iter().all(|d| d.is_some_and(|d| d > cutoff));
                                if !kept {
                                    pruned += 1;
                                    assert_eq!(metric, DistanceMetric::SquaredL2);
                                    assert!(all_exceed, "{kernel:?} dims={dims} cutoff={cutoff}");
                                    continue;
                                }
                                for (actual, expected) in out.iter().zip(&expected) {
                                    assert!(same(*actual, *expected), "{kernel:?} {metric:?}");
                                }
                                if metric == DistanceMetric::SquaredL2
                                    && dims > BLOCK
                                    && cutoff < 0.0
                                {
                                    panic!("{kernel:?} dims={dims}: a negative cutoff must prune");
                                }
                            }
                        });
                    }
                }
            }
        }
        assert!(pruned > 0);
    }

    #[test]
    fn dispatch_detects_once_and_test_override_is_scoped_to_the_thread() {
        #[cfg(target_arch = "x86_64")]
        let expected = if std::arch::is_x86_feature_detected!("avx2") {
            Kernel::Avx2
        } else {
            Kernel::Portable
        };
        #[cfg(not(target_arch = "x86_64"))]
        let expected = Kernel::Portable;
        assert_eq!(Kernel::detected(), expected);
        assert_eq!(
            Scorer::new(&[1.0], DistanceMetric::SquaredL2, 0.0).kernel(),
            expected
        );
        for kernel in kernels() {
            let inner = force_kernel(kernel, || {
                let nested = force_kernel(Kernel::Portable, || {
                    Scorer::new(&[1.0], DistanceMetric::SquaredL2, 0.0).kernel()
                });
                assert_eq!(nested, Kernel::Portable);
                Scorer::new(&[1.0], DistanceMetric::SquaredL2, 0.0).kernel()
            });
            assert_eq!(inner, kernel);
            let other =
                std::thread::spawn(|| Scorer::new(&[1.0], DistanceMetric::SquaredL2, 0.0).kernel());
            assert_eq!(other.join().unwrap(), expected);
        }
        assert_eq!(
            Scorer::new(&[1.0], DistanceMetric::SquaredL2, 0.0).kernel(),
            expected
        );
    }

    fn bits(hits: &[SearchHit]) -> Vec<(u64, u64)> {
        hits.iter()
            .map(|hit| (hit.id, hit.distance.to_bits()))
            .collect()
    }

    /// Rows with exact ties (every third row repeats its predecessor), a
    /// zero vector, coarse values that recur, and three metadata groups.
    fn tied_records(count: u64, dims: usize, seed: u64) -> RecordMap {
        let mut rng = Rng(seed);
        let mut previous = Vec::new();
        (0..count)
            .map(|id| {
                let vector: Vec<f32> = match id % 3 {
                    _ if id == 5 => vec![0.0; dims],
                    2 => previous.clone(),
                    1 => (0..dims)
                        .map(|_| (rng.unit() * 4.0).round() / 4.0)
                        .collect(),
                    _ => (0..dims).map(|_| rng.wide()).collect(),
                };
                previous = vector.clone();
                let metadata =
                    Metadata::from([("group".into(), MetadataValue::Integer((id % 3) as i64))]);
                (
                    id,
                    Record {
                        vector,
                        payload: Vec::new(),
                        metadata,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn exact_search_matches_full_sort_oracle_under_each_kernel() {
        let filter = [Condition::Eq {
            field: "group".into(),
            value: MetadataValue::Integer(2),
        }];
        let expiries: BTreeMap<u64, u64> = (0..3000).step_by(7).map(|id| (id, 100)).collect();
        let none = BTreeMap::new();
        for dims in [3usize, 37, 384] {
            let records = tied_records(3000, dims, dims as u64);
            let mut rng = Rng(77 + dims as u64);
            let subset: BTreeSet<u64> =
                (0..4000).filter(|_| rng.bits().is_multiple_of(3)).collect();
            let queries = [
                (0..dims).map(|_| rng.unit()).collect::<Vec<f32>>(),
                records[&4].vector.clone(),
            ];
            for kernel in kernels() {
                for query in &queries {
                    for metric in METRICS {
                        let query_norm = query_norm(query, metric);
                        for candidates in [None, Some(&subset)] {
                            for (conditions, expiries) in
                                [(&[][..], &none), (&filter[..], &expiries)]
                            {
                                let eligible: Vec<(&u64, &Record)> = records
                                    .iter()
                                    .filter(|(id, record)| {
                                        candidates.is_none_or(|ids| ids.contains(*id))
                                            && !expiries.get(*id).is_some_and(|e| *e <= 100)
                                            && crate::metadata::matches(
                                                &record.metadata,
                                                conditions,
                                            )
                                    })
                                    .collect();
                                let mut expected: Vec<SearchHit> = eligible
                                    .iter()
                                    .filter_map(|&(&id, record)| {
                                        oracle(&record.vector, query, metric, None, query_norm)
                                            .map(|distance| SearchHit { id, distance })
                                    })
                                    .collect();
                                expected.sort_by(|a, b| {
                                    a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id))
                                });
                                for k in [1, 10, 3001] {
                                    let mut stats = SearchStats::default();
                                    let actual = force_kernel(kernel, || {
                                        crate::exact_search(
                                            &records, expiries, query, query_norm, k, conditions,
                                            metric, candidates, 100, &mut stats,
                                        )
                                    });
                                    assert_eq!(
                                        bits(&actual),
                                        bits(&expected[..k.min(expected.len())]),
                                        "{kernel:?} dims={dims} {metric:?} k={k}"
                                    );
                                    assert_eq!(stats.distance_evaluations, eligible.len() as u64);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn ann_rerank_buffered_and_reranker_paths_report_oracle_bits_under_each_kernel() {
        use crate::quantize::{AnnOptions, Quantization};
        use crate::rerank::ExactReranker;
        let dims = 21;
        let config = HnswConfig {
            m: 8,
            ef_construction: 40,
            seed: 5,
        };
        let filter = [Condition::Eq {
            field: "group".into(),
            value: MetadataValue::Integer(1),
        }];
        let records = tied_records(600, dims, 3);
        let mut rng = Rng(11);
        // Wide queries make squared-L2 differences round, so the reported bits
        // depend on the exact operation order and not only on wiring.
        let queries: Vec<Vec<f32>> = (0..4)
            .map(|index| {
                (0..dims)
                    .map(|_| if index < 2 { rng.unit() } else { rng.wide() })
                    .collect()
            })
            .collect();
        for metric in [DistanceMetric::SquaredL2, DistanceMetric::Cosine] {
            for quantization in [Quantization::None, Quantization::ScalarU8] {
                let options = AnnOptions {
                    quantization,
                    rerank_multiplier: 3,
                };
                let mut db = Database::create_in_memory(dims, CreateOptions::default()).unwrap();
                for (&id, record) in records.iter() {
                    if !(metric == DistanceMetric::Cosine
                        && record.vector.iter().all(|v| *v == 0.0))
                    {
                        db.put_with_metadata(id, &record.vector, b"", &record.metadata)
                            .unwrap();
                    }
                }
                db.build_hnsw_with_options(config, metric, options).unwrap();
                // Buffered updates are merged with exact scores.
                for id in (0..600).step_by(37) {
                    let vector: Vec<f32> = (0..dims).map(|_| rng.unit()).collect();
                    db.put(id, &vector, b"").unwrap();
                }
                for query in &queries {
                    let query_norm = query_norm(query, metric);
                    let mut per_kernel = Vec::new();
                    for kernel in kernels() {
                        let hits = force_kernel(kernel, || {
                            let mut runs = Vec::new();
                            for conditions in [&[][..], &filter[..]] {
                                let (hits, _) = db
                                    .search_approximate_ready_with_options(
                                        query, 10, 32, config, options, conditions, metric,
                                    )
                                    .unwrap();
                                runs.push(hits);
                            }
                            let exact = db.search_with_metric(query, 40, metric).unwrap();
                            let reranked = db
                                .collection("default")
                                .unwrap()
                                .rerank(query, &exact, 15, &ExactReranker { metric })
                                .unwrap();
                            runs.push(reranked);
                            runs
                        });
                        for run in &hits {
                            assert!(!run.is_empty());
                            for hit in run {
                                let record = db.get(hit.id).unwrap().unwrap();
                                let expected =
                                    oracle(&record.vector, query, metric, None, query_norm);
                                assert!(same(Some(hit.distance), expected), "{kernel:?} {hit:?}");
                            }
                            assert!(run.windows(2).all(|pair| {
                                pair[0]
                                    .distance
                                    .total_cmp(&pair[1].distance)
                                    .then(pair[0].id.cmp(&pair[1].id))
                                    .is_lt()
                            }));
                        }
                        per_kernel.push(hits.iter().map(|run| bits(run)).collect::<Vec<_>>());
                    }
                    assert!(per_kernel.windows(2).all(|pair| pair[0] == pair[1]));
                }
            }
        }
    }

    #[test]
    fn named_vector_search_matches_query_first_oracle_under_each_kernel() {
        use crate::vectors::VectorSpec;
        let dims = 19;
        let records = tied_records(120, dims, 9);
        let mut rng = Rng(23);
        let queries: Vec<Vec<f32>> = (0..4)
            .map(|index| {
                (0..dims)
                    .map(|_| if index < 2 { rng.unit() } else { rng.wide() })
                    .collect()
            })
            .collect();
        let mut db = Database::create_in_memory(1, CreateOptions::default()).unwrap();
        for &id in records.keys() {
            db.put(id, &[1.0], b"").unwrap();
        }
        let mut collection = db.collection_mut("default").unwrap();
        let name = |metric: DistanceMetric| format!("{metric:?}").to_lowercase();
        for metric in METRICS {
            let spec = VectorSpec {
                dimensions: dims,
                metric,
                active_version: "v1".into(),
                history: 0,
            };
            collection.configure_vector(&name(metric), spec).unwrap();
            for (&id, record) in &records {
                collection
                    .put_vector_version(id, &name(metric), "v1", record.vector.clone())
                    .unwrap();
            }
        }
        let view = collection.view();
        for metric in METRICS {
            for query in &queries {
                let query_norm = query_norm(query, metric);
                // The call this path made before P3d: query first.
                let mut expected: Vec<SearchHit> = records
                    .iter()
                    .filter_map(|(&id, record)| {
                        let norm = vector_norm(&record.vector);
                        metric_distance(query, &record.vector, metric, query_norm, norm)
                            .map(|distance| SearchHit { id, distance })
                    })
                    .collect();
                expected.sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id)));
                expected.truncate(25);
                for kernel in kernels() {
                    let actual = force_kernel(kernel, || {
                        view.search_named(&name(metric), None, query, 25).unwrap()
                    });
                    assert_eq!(bits(&actual), bits(&expected), "{kernel:?} {metric:?}");
                }
            }
        }
    }

    /// Local, direction-only timing; the numbers are not a published claim.
    /// `cargo test --release --lib exact_kernels::tests::local_timing -- --ignored --nocapture`
    #[test]
    #[ignore = "local timing, direction only"]
    fn local_timing_direction_only() {
        use std::hint::black_box;
        use std::time::{Duration, Instant};
        let best = |run: &dyn Fn() -> Vec<Option<f64>>| {
            let mut best = Duration::MAX;
            let mut result = Vec::new();
            for _ in 0..7 {
                let start = Instant::now();
                result = black_box(run());
                best = best.min(start.elapsed());
            }
            (best, result)
        };
        for dims in [384usize, 768] {
            let mut rng = Rng(dims as u64);
            let data: Vec<Vec<f32>> = (0..20_000)
                .map(|_| (0..dims).map(|_| rng.unit()).collect())
                .collect();
            let rows: Vec<&[f32]> = data.iter().map(Vec::as_slice).collect();
            let query: Vec<f32> = (0..dims).map(|_| rng.unit()).collect();
            for metric in METRICS {
                let query_norm = query_norm(&query, metric);
                let (base, expected) = best(&|| {
                    rows.iter()
                        .map(|row| oracle(black_box(row), &query, metric, None, query_norm))
                        .collect()
                });
                let per_row = |time: Duration| time.as_nanos() as f64 / rows.len() as f64;
                print!("dims={dims} {metric:?}: oracle {:.0} ns/row", per_row(base));
                for kernel in kernels() {
                    let (time, actual) = best(&|| {
                        force_kernel(kernel, || {
                            Scorer::new(&query, metric, query_norm).score(black_box(&rows), None)
                        })
                    });
                    assert!(actual.iter().zip(&expected).all(|(a, e)| same(*a, *e)));
                    print!(
                        ", {kernel:?} {:.0} ns/row ({:.2}x)",
                        per_row(time),
                        base.as_secs_f64() / time.as_secs_f64()
                    );
                }
                println!();
            }
        }
    }
}
