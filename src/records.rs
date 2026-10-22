use crate::{
    CollectionMut, CollectionRef, Condition, Database, DistanceMetric, Error, MetadataValue,
    Record, Result, SearchHit, current_time_ms, metadata, validate_vector,
};
use std::collections::HashMap;

/// Maximum number of records returned by one embedded browse call.
pub const MAX_BROWSE_PAGE: usize = 1_000;

#[derive(Clone, Debug, PartialEq)]
pub struct BrowseHit {
    pub id: u64,
    pub record: Record,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BrowsePage {
    pub hits: Vec<BrowseHit>,
    pub next_after: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchGroup {
    pub value: MetadataValue,
    pub hits: Vec<SearchHit>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FacetCount {
    pub value: MetadataValue,
    pub count: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NumericStats {
    pub count: u64,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub sum: f64,
    pub average: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeoSort {
    VectorDistance,
    GeoDistance,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GeoSearchHit {
    pub id: u64,
    pub vector_distance: f64,
    pub geo_distance_meters: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecommendationQuery {
    pub positive_ids: Vec<u64>,
    pub negative_ids: Vec<u64>,
    pub positive_vectors: Vec<Vec<f32>>,
    pub negative_vectors: Vec<Vec<f32>>,
    pub k: usize,
    pub conditions: Vec<Condition>,
    pub metric: DistanceMetric,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GeoSearchQuery {
    pub vector: Vec<f32>,
    pub k: usize,
    pub field: String,
    pub latitude: f64,
    pub longitude: f64,
    pub radius_meters: f64,
    pub conditions: Vec<Condition>,
    pub metric: DistanceMetric,
    pub sort: GeoSort,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ValueKey {
    String(String),
    Integer(i64),
    Float(u64),
    Boolean(bool),
    Geo(u64, u64),
}

fn value_key(value: &MetadataValue) -> ValueKey {
    match value {
        MetadataValue::String(value) => ValueKey::String(value.clone()),
        MetadataValue::Integer(value) => ValueKey::Integer(*value),
        MetadataValue::Float(value) => {
            ValueKey::Float(if *value == 0.0 { 0.0f64 } else { *value }.to_bits())
        }
        MetadataValue::Boolean(value) => ValueKey::Boolean(*value),
        MetadataValue::GeoPoint {
            latitude,
            longitude,
        } => ValueKey::Geo(
            if *latitude == 0.0 { 0.0f64 } else { *latitude }.to_bits(),
            if *longitude == 0.0 {
                0.0f64
            } else {
                *longitude
            }
            .to_bits(),
        ),
    }
}

fn compare_values(left: &MetadataValue, right: &MetadataValue) -> std::cmp::Ordering {
    use MetadataValue::*;
    let rank = |value: &MetadataValue| match value {
        String(_) => 0,
        Integer(_) => 1,
        Float(_) => 2,
        Boolean(_) => 3,
        GeoPoint { .. } => 4,
    };
    rank(left)
        .cmp(&rank(right))
        .then_with(|| match (left, right) {
            (String(a), String(b)) => a.cmp(b),
            (Integer(a), Integer(b)) => a.cmp(b),
            (Float(a), Float(b)) => a.total_cmp(b),
            (Boolean(a), Boolean(b)) => a.cmp(b),
            (
                GeoPoint {
                    latitude: a1,
                    longitude: a2,
                },
                GeoPoint {
                    latitude: b1,
                    longitude: b2,
                },
            ) => a1.total_cmp(b1).then_with(|| a2.total_cmp(b2)),
            _ => std::cmp::Ordering::Equal,
        })
}

fn valid_field(field: &str) -> bool {
    !field.is_empty() && field.len() <= 256
}

impl CollectionRef<'_> {
    /// Exact filtered retrieval at a caller-provided Unix-millisecond time.
    pub fn search_filtered_with_metric_at(
        &self,
        query: &[f32],
        k: usize,
        conditions: &[Condition],
        metric: DistanceMetric,
        now_ms: u64,
    ) -> Result<Vec<SearchHit>> {
        self.db.search_in_at(
            self.id,
            query,
            k,
            conditions,
            metric,
            now_ms,
            &mut crate::SearchStats::default(),
        )
    }

    /// Returns a bounded page of records by ascending ID, excluding expired records.
    pub fn browse(&self, after: Option<u64>, limit: usize) -> Result<BrowsePage> {
        self.browse_at(after, limit, current_time_ms())
    }

    /// Deterministic form of `browse` for tests and applications with an injected clock.
    pub fn browse_at(&self, after: Option<u64>, limit: usize, now_ms: u64) -> Result<BrowsePage> {
        self.db.ensure_healthy()?;
        if !(1..=MAX_BROWSE_PAGE).contains(&limit) {
            return Err(Error::InvalidQuery("browse limit must be 1..=1000"));
        }
        let state = self.db.state(self.id);
        let mut selected = Vec::with_capacity(limit.saturating_add(1));
        for (&id, record) in state
            .records
            .iter()
            .filter(|(id, _)| after.is_none_or(|after| **id > after))
        {
            if state
                .expiries
                .get(&id)
                .is_some_and(|expiry| *expiry <= now_ms)
            {
                continue;
            }
            selected.push(BrowseHit {
                id,
                record: record.clone(),
            });
            if selected.len() > limit {
                break;
            }
        }
        let has_more = selected.len() > limit;
        selected.truncate(limit);
        let next_after = has_more.then(|| selected.last().expect("nonempty page").id);
        Ok(BrowsePage {
            hits: selected,
            next_after,
        })
    }

    /// Groups exact vector-search hits by a typed metadata value. Records without
    /// the grouping field are omitted; groups follow their nearest hit ordering.
    pub fn search_grouped(
        &self,
        query: &[f32],
        groups: usize,
        hits_per_group: usize,
        field: &str,
        conditions: &[Condition],
        metric: DistanceMetric,
    ) -> Result<Vec<SearchGroup>> {
        self.db.ensure_healthy()?;
        if !valid_field(field) {
            return Err(Error::InvalidQuery("group field must be 1..=256 bytes"));
        }
        let state = self.db.state(self.id);
        let hits = self.db.search_in(
            self.id,
            query,
            state.records.len(),
            conditions,
            metric,
            &mut crate::SearchStats::default(),
        )?;
        if groups == 0 || hits_per_group == 0 {
            return Ok(Vec::new());
        }
        let mut output: Vec<SearchGroup> = Vec::new();
        let mut positions: HashMap<ValueKey, usize> = HashMap::new();
        for hit in hits {
            let Some(value) = state.records[&hit.id].metadata.get(field) else {
                continue;
            };
            let key = value_key(value);
            if let Some(&position) = positions.get(&key) {
                if output[position].hits.len() < hits_per_group {
                    output[position].hits.push(hit);
                }
            } else if output.len() < groups {
                positions.insert(key, output.len());
                output.push(SearchGroup {
                    value: value.clone(),
                    hits: vec![hit],
                });
            }
        }
        Ok(output)
    }

    /// Recommendation from positive and negative example IDs and/or vectors.
    /// The query is the positive mean minus the negative mean. Example IDs are
    /// excluded from results. At least one positive example is required.
    pub fn recommend(&self, request: &RecommendationQuery) -> Result<Vec<SearchHit>> {
        let positive_ids = &request.positive_ids;
        let negative_ids = &request.negative_ids;
        let positive_vectors = &request.positive_vectors;
        let negative_vectors = &request.negative_vectors;
        let k = request.k;
        let conditions = &request.conditions;
        let metric = request.metric;
        self.db.ensure_healthy()?;
        let state = self.db.state(self.id);
        metadata::validate_filter(conditions)?;
        let now_ms = current_time_ms();
        let mut excluded = std::collections::BTreeSet::new();
        let mut positives = Vec::new();
        let mut negatives = Vec::new();
        for &id in positive_ids.iter().chain(negative_ids) {
            if !excluded.insert(id) {
                return Err(Error::InvalidQuery(
                    "recommendation example IDs must be unique",
                ));
            }
            let record = state
                .records
                .get(&id)
                .filter(|_| {
                    !state
                        .expiries
                        .get(&id)
                        .is_some_and(|expiry| *expiry <= now_ms)
                })
                .ok_or(Error::InvalidQuery(
                    "recommendation example ID is missing or expired",
                ))?;
            if positive_ids.contains(&id) {
                positives.push(record.vector.as_slice());
            } else {
                negatives.push(record.vector.as_slice());
            }
        }
        for vector in positive_vectors.iter().chain(negative_vectors) {
            validate_vector(state.dimensions, vector)?;
        }
        if positives.is_empty() && positive_vectors.is_empty() {
            return Err(Error::InvalidQuery(
                "recommendation needs at least one positive example",
            ));
        }
        let positive_count = positives.len() + positive_vectors.len();
        let negative_count = negatives.len() + negative_vectors.len();
        let mut query = vec![0.0f32; state.dimensions];
        for dimension in 0..state.dimensions {
            let positive_sum: f64 = positives
                .iter()
                .map(|vector| f64::from(vector[dimension]))
                .sum::<f64>()
                + positive_vectors
                    .iter()
                    .map(|vector| f64::from(vector[dimension]))
                    .sum::<f64>();
            let negative_sum: f64 = negatives
                .iter()
                .map(|vector| f64::from(vector[dimension]))
                .sum::<f64>()
                + negative_vectors
                    .iter()
                    .map(|vector| f64::from(vector[dimension]))
                    .sum::<f64>();
            let value = positive_sum / positive_count as f64
                - if negative_count == 0 {
                    0.0
                } else {
                    negative_sum / negative_count as f64
                };
            query[dimension] = value as f32;
            if !query[dimension].is_finite() {
                return Err(Error::NonFiniteVector);
            }
        }
        let all = self.db.search_in_at(
            self.id,
            &query,
            state.records.len(),
            conditions,
            metric,
            now_ms,
            &mut crate::SearchStats::default(),
        )?;
        Ok(all
            .into_iter()
            .filter(|hit| !excluded.contains(&hit.id))
            .take(k)
            .collect())
    }

    /// Counts typed values for one metadata field under an optional AND filter.
    pub fn facets(&self, field: &str, conditions: &[Condition]) -> Result<Vec<FacetCount>> {
        self.db.ensure_healthy()?;
        if !valid_field(field) {
            return Err(Error::InvalidQuery("facet field must be 1..=256 bytes"));
        }
        metadata::validate_filter(conditions)?;
        let now_ms = current_time_ms();
        let state = self.db.state(self.id);
        let mut counts: HashMap<ValueKey, (MetadataValue, u64)> = HashMap::new();
        for (&id, record) in &state.records {
            if state
                .expiries
                .get(&id)
                .is_some_and(|expiry| *expiry <= now_ms)
                || !metadata::matches(&record.metadata, conditions)
            {
                continue;
            }
            if let Some(value) = record.metadata.get(field) {
                let entry = counts
                    .entry(value_key(value))
                    .or_insert_with(|| (value.clone(), 0));
                entry.1 += 1;
            }
        }
        let mut facets: Vec<_> = counts
            .into_values()
            .map(|(value, count)| FacetCount { value, count })
            .collect();
        facets.sort_by(|a, b| compare_values(&a.value, &b.value));
        Ok(facets)
    }

    /// Numeric min/max/sum/count/average over integer and float metadata values.
    pub fn numeric_stats(&self, field: &str, conditions: &[Condition]) -> Result<NumericStats> {
        self.db.ensure_healthy()?;
        if !valid_field(field) {
            return Err(Error::InvalidQuery("stats field must be 1..=256 bytes"));
        }
        metadata::validate_filter(conditions)?;
        let now_ms = current_time_ms();
        let state = self.db.state(self.id);
        let mut result = NumericStats::default();
        for (&id, record) in &state.records {
            if state
                .expiries
                .get(&id)
                .is_some_and(|expiry| *expiry <= now_ms)
                || !metadata::matches(&record.metadata, conditions)
            {
                continue;
            }
            let value = match record.metadata.get(field) {
                Some(MetadataValue::Integer(value)) => *value as f64,
                Some(MetadataValue::Float(value)) => *value,
                _ => continue,
            };
            result.count += 1;
            result.min = Some(result.min.map_or(value, |current| current.min(value)));
            result.max = Some(result.max.map_or(value, |current| current.max(value)));
            result.sum += value;
            if !result.sum.is_finite() {
                return Err(Error::InvalidQuery("numeric stats sum overflow"));
            }
        }
        result.average = (result.count != 0).then_some(result.sum / result.count as f64);
        Ok(result)
    }

    /// Vector search constrained to a spherical radius around a geo-point field.
    /// Vector distance remains available; `sort` selects vector or geographic ordering.
    pub fn search_geo(&self, request: &GeoSearchQuery) -> Result<Vec<GeoSearchHit>> {
        let query = &request.vector;
        let k = request.k;
        let field = request.field.as_str();
        let latitude = request.latitude;
        let longitude = request.longitude;
        let radius_meters = request.radius_meters;
        let conditions = &request.conditions;
        let metric = request.metric;
        let sort = request.sort;
        if !valid_field(field) {
            return Err(Error::InvalidQuery("geo field must be 1..=256 bytes"));
        }
        let mut filters = conditions.to_vec();
        filters.push(Condition::GeoRadius {
            field: field.to_owned(),
            latitude,
            longitude,
            radius_meters,
        });
        metadata::validate_filter(&filters)?;
        let state = self.db.state(self.id);
        let limit = if sort == GeoSort::GeoDistance {
            state.records.len()
        } else {
            k
        };
        let hits = self.db.search_in(
            self.id,
            query,
            limit,
            &filters,
            metric,
            &mut crate::SearchStats::default(),
        )?;
        let mut output: Vec<_> = hits
            .into_iter()
            .map(|hit| {
                let (lat, lon) = match state.records[&hit.id]
                    .metadata
                    .get(field)
                    .expect("radius filter matched")
                {
                    MetadataValue::GeoPoint {
                        latitude,
                        longitude,
                    } => (*latitude, *longitude),
                    _ => unreachable!("radius filter only matches geo points"),
                };
                GeoSearchHit {
                    id: hit.id,
                    vector_distance: hit.distance,
                    geo_distance_meters: metadata::geo_distance_meters(
                        latitude, longitude, lat, lon,
                    ),
                }
            })
            .collect();
        output.sort_by(|a, b| match sort {
            GeoSort::VectorDistance => a
                .vector_distance
                .total_cmp(&b.vector_distance)
                .then_with(|| a.id.cmp(&b.id)),
            GeoSort::GeoDistance => a
                .geo_distance_meters
                .total_cmp(&b.geo_distance_meters)
                .then_with(|| a.id.cmp(&b.id)),
        });
        output.truncate(k);
        Ok(output)
    }
}

impl CollectionMut<'_> {
    pub fn browse(&self, after: Option<u64>, limit: usize) -> Result<BrowsePage> {
        self.view().browse(after, limit)
    }
    pub fn search_grouped(
        &self,
        query: &[f32],
        groups: usize,
        hits_per_group: usize,
        field: &str,
        conditions: &[Condition],
        metric: DistanceMetric,
    ) -> Result<Vec<SearchGroup>> {
        self.view()
            .search_grouped(query, groups, hits_per_group, field, conditions, metric)
    }
    pub fn recommend(&self, query: &RecommendationQuery) -> Result<Vec<SearchHit>> {
        self.view().recommend(query)
    }
    pub fn facets(&self, field: &str, conditions: &[Condition]) -> Result<Vec<FacetCount>> {
        self.view().facets(field, conditions)
    }
    pub fn numeric_stats(&self, field: &str, conditions: &[Condition]) -> Result<NumericStats> {
        self.view().numeric_stats(field, conditions)
    }
    pub fn search_geo(&self, query: &GeoSearchQuery) -> Result<Vec<GeoSearchHit>> {
        self.view().search_geo(query)
    }
}

impl Database {
    pub fn browse(&self, after: Option<u64>, limit: usize) -> Result<BrowsePage> {
        CollectionRef { db: self, id: 0 }.browse(after, limit)
    }
    pub fn browse_at(&self, after: Option<u64>, limit: usize, now_ms: u64) -> Result<BrowsePage> {
        CollectionRef { db: self, id: 0 }.browse_at(after, limit, now_ms)
    }
    pub fn search_grouped(
        &self,
        query: &[f32],
        groups: usize,
        hits_per_group: usize,
        field: &str,
        conditions: &[Condition],
        metric: DistanceMetric,
    ) -> Result<Vec<SearchGroup>> {
        CollectionRef { db: self, id: 0 }.search_grouped(
            query,
            groups,
            hits_per_group,
            field,
            conditions,
            metric,
        )
    }
    pub fn recommend(&self, query: &RecommendationQuery) -> Result<Vec<SearchHit>> {
        CollectionRef { db: self, id: 0 }.recommend(query)
    }
    pub fn facets(&self, field: &str, conditions: &[Condition]) -> Result<Vec<FacetCount>> {
        CollectionRef { db: self, id: 0 }.facets(field, conditions)
    }
    pub fn numeric_stats(&self, field: &str, conditions: &[Condition]) -> Result<NumericStats> {
        CollectionRef { db: self, id: 0 }.numeric_stats(field, conditions)
    }
    pub fn search_geo(&self, query: &GeoSearchQuery) -> Result<Vec<GeoSearchHit>> {
        CollectionRef { db: self, id: 0 }.search_geo(query)
    }
}
