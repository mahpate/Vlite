//! Gap 4: validity segments of a temporal edge.
//!
//! A temporal edge is one logical relationship (one ID, an optional key,
//! fixed endpoints and relationship type) that owns an ordered series of
//! segments. Each segment is a half-open validity interval
//! `[valid_from, valid_to)` in signed `i64` milliseconds since the Unix
//! epoch (UTC) with its own properties. `None` means negative infinity for
//! `valid_from` and positive infinity for `valid_to`; there are no integer
//! sentinels.
//!
//! A valid series has at most [`MAX_SEGMENTS`] segments; each has
//! `valid_from < valid_to` when both ends are finite; segments are sorted by
//! `valid_from` (only the first may be open-started) and never overlap.
//! Touching intervals and gaps are allowed and are never merged. These are
//! the rules of the independent reference `tests/temporal_interval_oracle.py`,
//! checked in the same order, so an invalid series reports the same category.

use super::error::GraphError;
use super::model::Properties;
use super::properties;

/// Gap 4: segments per temporal edge. Bounds binary search, correction
/// validation and per-edge memory growth.
pub const MAX_SEGMENTS: usize = 256;

/// Gap 4: whether this build admits temporal edges. Until temporal reads
/// exist (slice C) only test builds do: any other build refuses to give a
/// state the capability ([`GraphState::set_temporal`]) or to load a
/// snapshot holding a temporal edge, so it can neither plan nor apply a
/// temporal entry.
///
/// [`GraphState::set_temporal`]: super::GraphState::set_temporal
pub(crate) const fn supported() -> bool {
    cfg!(test)
}

/// `GraphError::InvalidTemporal` reasons. Each starts with the category
/// name used by the reference oracle (`bounds`, `order`, `overlap`,
/// `segment_limit`) or by a temporal mutation.
pub mod reason {
    pub const BOUNDS: &str = "bounds: valid_from must be before valid_to";
    pub const ORDER: &str =
        "order: segments must be sorted by valid_from and only the first may be open-started";
    pub const OVERLAP: &str = "overlap: segments must not overlap";
    pub const SEGMENT_LIMIT: &str = "segment_limit: a temporal edge has at most 256 segments";
    pub const NO_SEGMENT: &str = "no_segment: no segment has these exact bounds";
    pub const CLOSE: &str = "close: the closing time must be inside the segment";
    pub const NOT_TEMPORAL: &str = "not_temporal: the edge has no validity segments";
    pub const TEMPORAL_EDGE: &str =
        "temporal_edge: a temporal edge changes only through temporal operations";
    pub const SERIES_BYTES: &str = "series_bytes: the encoded series must fit one graph body";
    pub const EXPECTED: &str =
        "expected: temporal corrections and promotion need an expected revision";
    pub const ALREADY_TEMPORAL: &str = "already_temporal: the edge already has validity segments";
    pub const CAPABILITY: &str = "capability: the graph state does not have temporal edges enabled";
}

/// One validity segment: `[valid_from, valid_to)` and its properties.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub valid_from: Option<i64>,
    pub valid_to: Option<i64>,
    pub properties: Properties,
}

impl Segment {
    /// Whether `t` lies in `[valid_from, valid_to)`.
    pub fn contains(&self, t: i64) -> bool {
        self.valid_from.is_none_or(|from| from <= t) && self.valid_to.is_none_or(|to| t < to)
    }

    /// Whether the segment has exactly these bounds.
    pub fn has_bounds(&self, valid_from: Option<i64>, valid_to: Option<i64>) -> bool {
        self.valid_from == valid_from && self.valid_to == valid_to
    }
}

fn invalid(reason: &'static str) -> GraphError {
    GraphError::InvalidTemporal(reason)
}

/// `valid_from < valid_to` when both are finite.
pub fn check_bounds(valid_from: Option<i64>, valid_to: Option<i64>) -> Result<(), GraphError> {
    match (valid_from, valid_to) {
        (Some(from), Some(to)) if from >= to => Err(invalid(reason::BOUNDS)),
        _ => Ok(()),
    }
}

/// One segment's properties, then its bounds.
pub fn validate_segment(segment: &Segment) -> Result<(), GraphError> {
    properties::validate(&segment.properties)?;
    check_bounds(segment.valid_from, segment.valid_to)
}

/// The ordering rules between two consecutive segments.
fn check_after(prior: &Segment, next: &Segment) -> Result<(), GraphError> {
    let Some(start) = next.valid_from else {
        return Err(invalid(reason::ORDER));
    };
    if prior
        .valid_from
        .is_some_and(|prior_start| start < prior_start)
    {
        return Err(invalid(reason::ORDER));
    }
    if prior.valid_to.is_none_or(|prior_end| start < prior_end) {
        return Err(invalid(reason::OVERLAP));
    }
    Ok(())
}

/// Validates a whole series: the segment limit first, then each segment's
/// properties, bounds, order and overlap with its predecessor.
pub fn validate_series(series: &[Segment]) -> Result<(), GraphError> {
    if series.len() > MAX_SEGMENTS {
        return Err(invalid(reason::SEGMENT_LIMIT));
    }
    let mut prior = None;
    for segment in series {
        validate_segment(segment)?;
        if let Some(prior) = prior {
            check_after(prior, segment)?;
        }
        prior = Some(segment);
    }
    Ok(())
}

/// The unique segment containing `t`, found by binary search.
pub fn segment_at(series: &[Segment], t: i64) -> Option<&Segment> {
    let started = series.partition_point(|s| s.valid_from.is_none_or(|from| from <= t));
    let candidate = &series[started.checked_sub(1)?];
    candidate
        .valid_to
        .is_none_or(|to| t < to)
        .then_some(candidate)
}

/// Index of the segment with exactly these bounds, found by binary search.
pub fn position(
    series: &[Segment],
    valid_from: Option<i64>,
    valid_to: Option<i64>,
) -> Option<usize> {
    let index = series.partition_point(|s| s.valid_from < valid_from);
    series
        .get(index)
        .is_some_and(|s| s.has_bounds(valid_from, valid_to))
        .then_some(index)
}

/// Inserts a segment at its ordered position (backfill is allowed). A
/// rejected insert leaves `series` unchanged and reports what
/// [`validate_series`] would report for the result.
pub fn insert(series: &mut Vec<Segment>, segment: Segment) -> Result<usize, GraphError> {
    if series.len() >= MAX_SEGMENTS {
        return Err(invalid(reason::SEGMENT_LIMIT));
    }
    validate_segment(&segment)?;
    // `Option` orders `None` (negative infinity) first; an equal start goes
    // after the existing segment, as a stable sort would place it.
    let index = series.partition_point(|s| s.valid_from <= segment.valid_from);
    if let Some(prior) = index.checked_sub(1).map(|prior| &series[prior]) {
        check_after(prior, &segment)?;
    }
    if let Some(next) = series.get(index) {
        check_after(&segment, next)?;
    }
    series.insert(index, segment);
    Ok(index)
}

/// Removes the segment with exactly these bounds and returns it.
pub fn remove(
    series: &mut Vec<Segment>,
    valid_from: Option<i64>,
    valid_to: Option<i64>,
) -> Result<(usize, Segment), GraphError> {
    let index = position(series, valid_from, valid_to).ok_or(invalid(reason::NO_SEGMENT))?;
    Ok((index, series.remove(index)))
}

/// Replaces the segment with exactly these bounds by `segment`, which may
/// change bounds as well as properties. Returns the old segment; a rejected
/// replacement leaves `series` unchanged.
pub fn replace(
    series: &mut Vec<Segment>,
    valid_from: Option<i64>,
    valid_to: Option<i64>,
    segment: Segment,
) -> Result<Segment, GraphError> {
    let (index, old) = remove(series, valid_from, valid_to)?;
    match insert(series, segment) {
        Ok(_) => Ok(old),
        Err(error) => {
            series.insert(index, old);
            Err(error)
        }
    }
}

/// Closes the segment with exactly these bounds at `at`, which must lie
/// strictly inside it; closing at its existing finite end is a no-op.
/// Returns whether the series changed.
pub fn close(
    series: &mut [Segment],
    valid_from: Option<i64>,
    valid_to: Option<i64>,
    at: i64,
) -> Result<bool, GraphError> {
    let index = position(series, valid_from, valid_to).ok_or(invalid(reason::NO_SEGMENT))?;
    let changes = check_close(valid_from, valid_to, at)?;
    if changes {
        series[index].valid_to = Some(at);
    }
    Ok(changes)
}

/// The state-independent rules of closing `[valid_from, valid_to)` at `at`:
/// valid bounds, then `valid_from < at < valid_to` with open ends as
/// infinities, or `at == valid_to` (a no-op). Returns whether it changes
/// the segment.
pub fn check_close(
    valid_from: Option<i64>,
    valid_to: Option<i64>,
    at: i64,
) -> Result<bool, GraphError> {
    check_bounds(valid_from, valid_to)?;
    if valid_to == Some(at) {
        return Ok(false);
    }
    if valid_from.is_none_or(|from| from < at) && valid_to.is_none_or(|to| at < to) {
        Ok(true)
    } else {
        Err(invalid(reason::CLOSE))
    }
}
#[cfg(test)]
mod tests {
    use super::super::model::PropertyValue;
    use super::*;

    fn segment(valid_from: Option<i64>, valid_to: Option<i64>) -> Segment {
        Segment {
            valid_from,
            valid_to,
            properties: Properties::from([(
                "v".into(),
                PropertyValue::String(format!("{valid_from:?}..{valid_to:?}")),
            )]),
        }
    }

    fn err(reason: &'static str) -> Result<(), GraphError> {
        Err(GraphError::InvalidTemporal(reason))
    }

    #[test]
    fn bounds_are_half_open_with_open_ends_and_i64_extremes() {
        let s = segment(Some(100), Some(200));
        for (t, inside) in [(99, false), (100, true), (199, true), (200, false)] {
            assert_eq!(s.contains(t), inside, "{t}");
        }
        let all = segment(None, None);
        for t in [i64::MIN, -1, 0, i64::MAX] {
            assert!(all.contains(t));
        }
        let before = segment(None, Some(-5));
        assert!(before.contains(i64::MIN) && before.contains(-6) && !before.contains(-5));
        let after = segment(Some(i64::MIN), None);
        assert!(after.contains(i64::MIN) && after.contains(i64::MAX));
        assert!(segment(Some(i64::MAX - 1), Some(i64::MAX)).contains(i64::MAX - 1));
        assert!(!segment(Some(i64::MAX - 1), Some(i64::MAX)).contains(i64::MAX));
        // Pre-epoch values are ordinary timestamps.
        assert!(segment(Some(-200), Some(-100)).contains(-150));

        assert_eq!(check_bounds(Some(1), Some(2)), Ok(()));
        assert_eq!(check_bounds(None, Some(i64::MIN)), Ok(()));
        assert_eq!(check_bounds(Some(i64::MAX), None), Ok(()));
        assert_eq!(check_bounds(None, None), Ok(()));
        assert_eq!(check_bounds(Some(2), Some(2)), err(reason::BOUNDS));
        assert_eq!(check_bounds(Some(3), Some(2)), err(reason::BOUNDS));
        assert_eq!(
            check_bounds(Some(i64::MAX), Some(i64::MIN)),
            err(reason::BOUNDS)
        );
    }

    #[test]
    fn series_rules_match_the_oracle_categories_in_order() {
        assert_eq!(validate_series(&[]), Ok(()));
        let ok = [
            segment(None, Some(0)),
            segment(Some(0), Some(10)),
            segment(Some(20), Some(30)),
            segment(Some(30), None),
        ];
        assert_eq!(validate_series(&ok), Ok(()));
        // Bounds of a later segment are checked before its order.
        assert_eq!(
            validate_series(&[segment(Some(5), Some(6)), segment(Some(1), Some(1))]),
            err(reason::BOUNDS)
        );
        // Only the first segment may be open-started.
        assert_eq!(
            validate_series(&[segment(Some(5), Some(6)), segment(None, Some(1))]),
            err(reason::ORDER)
        );
        assert_eq!(
            validate_series(&[segment(None, Some(1)), segment(None, Some(2))]),
            err(reason::ORDER)
        );
        assert_eq!(
            validate_series(&[segment(Some(5), Some(6)), segment(Some(1), Some(2))]),
            err(reason::ORDER)
        );
        // Overlap, even with identical properties and the same start.
        assert_eq!(
            validate_series(&[segment(Some(1), Some(6)), segment(Some(5), Some(9))]),
            err(reason::OVERLAP)
        );
        assert_eq!(
            validate_series(&[segment(Some(1), Some(6)), segment(Some(1), Some(9))]),
            err(reason::OVERLAP)
        );
        assert_eq!(
            validate_series(&[segment(Some(1), None), segment(Some(5), Some(9))]),
            err(reason::OVERLAP)
        );
        // Properties are validated before bounds.
        let mut bad = segment(Some(2), Some(1));
        bad.properties = Properties::from([(String::new(), PropertyValue::Integer(1))]);
        assert!(matches!(
            validate_series(&[bad]),
            Err(GraphError::InvalidProperties(_))
        ));
    }

    #[test]
    fn the_segment_cap_is_256() {
        let series: Vec<_> = (0..MAX_SEGMENTS as i64)
            .map(|i| segment(Some(i), Some(i + 1)))
            .collect();
        assert_eq!(validate_series(&series), Ok(()));
        let mut over = series.clone();
        over.push(segment(Some(256), Some(257)));
        assert_eq!(validate_series(&over), err(reason::SEGMENT_LIMIT));
        // The limit is reported before any other error.
        over[0] = segment(Some(9), Some(1));
        assert_eq!(validate_series(&over), err(reason::SEGMENT_LIMIT));
        let mut full = series;
        assert_eq!(
            insert(&mut full, segment(Some(1000), None)),
            Err(GraphError::InvalidTemporal(reason::SEGMENT_LIMIT))
        );
        assert_eq!(full.len(), MAX_SEGMENTS);
    }

    #[test]
    fn point_lookup_selects_exactly_one_segment_or_none_in_gaps() {
        let series = [
            segment(None, Some(-10)),
            segment(Some(0), Some(10)),
            segment(Some(10), Some(20)),
            segment(Some(30), None),
        ];
        let at = |t| segment_at(&series, t).map(|s| (s.valid_from, s.valid_to));
        assert_eq!(at(i64::MIN), Some((None, Some(-10))));
        assert_eq!(at(-11), Some((None, Some(-10))));
        assert_eq!(at(-10), None);
        assert_eq!(at(-1), None);
        assert_eq!(at(0), Some((Some(0), Some(10))));
        assert_eq!(at(9), Some((Some(0), Some(10))));
        // Touching segments: exactly the later one at the boundary.
        assert_eq!(at(10), Some((Some(10), Some(20))));
        assert_eq!(at(19), Some((Some(10), Some(20))));
        assert_eq!(at(20), None);
        assert_eq!(at(29), None);
        assert_eq!(at(30), Some((Some(30), None)));
        assert_eq!(at(i64::MAX), Some((Some(30), None)));
        assert_eq!(segment_at(&[], 0), None);
        // Agrees with a linear scan over a full series.
        let full: Vec<_> = (0..256i64)
            .map(|i| segment(Some(i * 3), Some(i * 3 + 2)))
            .collect();
        for t in -2..800 {
            let linear = full.iter().find(|s| s.contains(t));
            assert_eq!(segment_at(&full, t), linear, "{t}");
        }
        assert_eq!(position(&series, Some(10), Some(20)), Some(2));
        assert_eq!(position(&series, None, Some(-10)), Some(0));
        assert_eq!(position(&series, Some(10), Some(21)), None);
        assert_eq!(position(&series, Some(11), Some(20)), None);
    }

    #[test]
    fn insert_backfills_in_order_and_rejects_without_mutation() {
        let mut series = vec![segment(Some(100), Some(200))];
        assert_eq!(insert(&mut series, segment(Some(300), None)), Ok(1));
        // Backfill before the first segment and into a gap; touching is fine.
        assert_eq!(insert(&mut series, segment(None, Some(100))), Ok(0));
        assert_eq!(insert(&mut series, segment(Some(200), Some(250))), Ok(2));
        assert_eq!(validate_series(&series), Ok(()));
        let before = series.clone();
        for (rejected, reason) in [
            (segment(Some(240), Some(260)), reason::OVERLAP),
            (segment(Some(250), Some(301)), reason::OVERLAP),
            (segment(None, Some(5)), reason::ORDER),
            (segment(Some(260), Some(260)), reason::BOUNDS),
            (segment(Some(150), Some(160)), reason::OVERLAP),
        ] {
            assert_eq!(
                insert(&mut series, rejected.clone()),
                Err(GraphError::InvalidTemporal(reason)),
                "{rejected:?}"
            );
            assert_eq!(series, before);
            // The same category as validating the would-be series.
            let mut candidate = before.clone();
            candidate.push(rejected);
            candidate.sort_by_key(|s| s.valid_from);
            assert_eq!(
                validate_series(&candidate),
                Err(GraphError::InvalidTemporal(reason))
            );
        }
        assert_eq!(insert(&mut series, segment(Some(250), Some(300))), Ok(3));
    }

    #[test]
    fn replace_close_and_remove_address_exact_bounds() {
        let mut series = vec![segment(Some(100), Some(200)), segment(Some(300), None)];
        assert_eq!(
            replace(&mut series, Some(100), Some(201), segment(Some(1), Some(2))),
            Err(GraphError::InvalidTemporal(reason::NO_SEGMENT))
        );
        // A correction may change properties and bounds.
        let mut corrected = segment(Some(100), Some(300));
        corrected.properties = Properties::new();
        let old = replace(&mut series, Some(100), Some(200), corrected.clone()).unwrap();
        assert_eq!(old, segment(Some(100), Some(200)));
        assert_eq!(series[0], corrected);
        // A rejected replacement leaves the series unchanged.
        let before = series.clone();
        assert_eq!(
            replace(
                &mut series,
                Some(100),
                Some(300),
                segment(Some(100), Some(301))
            ),
            Err(GraphError::InvalidTemporal(reason::OVERLAP))
        );
        assert_eq!(series, before);

        // Close strictly inside; at the existing finite end is a no-op.
        let rejected = |reason| Err(GraphError::InvalidTemporal(reason));
        assert_eq!(
            close(&mut series, Some(300), None, 300),
            rejected(reason::CLOSE)
        );
        assert_eq!(
            close(&mut series, Some(300), None, 299),
            rejected(reason::CLOSE)
        );
        assert_eq!(close(&mut series, Some(300), None, 400), Ok(true));
        assert_eq!(series[1].valid_to, Some(400));
        assert_eq!(close(&mut series, Some(300), Some(400), 400), Ok(false));
        assert_eq!(
            close(&mut series, Some(300), Some(400), 401),
            rejected(reason::CLOSE)
        );
        assert_eq!(
            close(&mut series, Some(300), None, 350),
            rejected(reason::NO_SEGMENT)
        );
        // Open ends are infinities: i64::MIN is strictly after negative infinity.
        let mut open = vec![segment(None, None)];
        assert_eq!(close(&mut open, None, None, i64::MIN), Ok(true));
        assert_eq!(open[0].valid_to, Some(i64::MIN));
        assert_eq!(validate_series(&open), Ok(()));

        let removed = remove(&mut series, Some(300), Some(400)).unwrap();
        assert_eq!(removed.0, 1);
        assert_eq!(removed.1.valid_to, Some(400));
        assert_eq!(
            remove(&mut series, Some(300), Some(400)),
            Err(GraphError::InvalidTemporal(reason::NO_SEGMENT))
        );
        remove(&mut series, Some(100), Some(300)).unwrap();
        assert!(series.is_empty());
    }
}
