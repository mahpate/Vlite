//! Opt-in v5 named, sparse and versioned embeddings. Indexes are disposable.
use crate::{
    AnnOptions, CollectionMut, CollectionRef, DistanceMetric, Error, HnswConfig, Metadata, Record,
    Result, SearchHit, SearchStats,
};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
pub struct VectorSpec {
    pub dimensions: usize,
    pub metric: DistanceMetric,
    pub active_version: String,
    /// Previous versions retained in addition to the active version (0..=64).
    pub history: usize,
}
#[derive(Clone, Debug, PartialEq)]
pub struct VersionedVector {
    pub version: String,
    pub vector: Vec<f32>,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecordVectors {
    pub dense: BTreeMap<String, Vec<VersionedVector>>,
    pub sparse: BTreeMap<String, Vec<(u32, f32)>>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReembedStatus {
    pub name: String,
    pub target_version: String,
    pub cursor: Option<u64>,
}
/// Source revision makes asynchronous provider results safe against concurrent edits.
#[derive(Clone, Debug)]
pub struct ReembedRecord {
    pub id: u64,
    pub revision: u64,
    pub record: Record,
}
#[derive(Clone, Debug)]
pub struct ReembedUpdate {
    pub id: u64,
    pub revision: u64,
    pub vector: Vec<f32>,
}
#[derive(Clone, Default)]
pub(crate) struct VectorData {
    pub specs: BTreeMap<String, VectorSpec>,
    pub records: BTreeMap<u64, RecordVectors>,
    pub jobs: BTreeMap<String, ReembedStatus>,
}
#[derive(Default)]
pub(crate) struct SparseIndex {
    postings: BTreeMap<(String, u32), BTreeMap<u64, f32>>,
    ids: BTreeMap<String, std::collections::BTreeSet<u64>>,
}
impl SparseIndex {
    pub(crate) fn build(data: &VectorData) -> Self {
        let mut index = Self::default();
        for (&id, record) in &data.records {
            for (name, values) in &record.sparse {
                index.ids.entry(name.clone()).or_default().insert(id);
                for &(term, value) in values {
                    index
                        .postings
                        .entry((name.clone(), term))
                        .or_default()
                        .insert(id, value);
                }
            }
        }
        index
    }
    pub(crate) fn remove(&mut self, id: u64) {
        self.ids.retain(|_, ids| {
            ids.remove(&id);
            !ids.is_empty()
        });
        self.postings.retain(|_, ids| {
            ids.remove(&id);
            !ids.is_empty()
        });
    }
}
fn invalid() -> Error {
    Error::InvalidQuery("invalid vector declaration, version, batch or sparse vector")
}
fn name(s: &str) -> Result<()> {
    if s.is_empty() || s.len() > 256 {
        Err(invalid())
    } else {
        Ok(())
    }
}
fn sparse(v: &[(u32, f32)]) -> Result<()> {
    if v.len() > crate::MAX_DIMENSIONS
        || v.iter().any(|(_, v)| !v.is_finite())
        || v.windows(2).any(|w| w[0].0 >= w[1].0)
    {
        Err(invalid())
    } else {
        Ok(())
    }
}
impl VectorData {
    pub(crate) fn invalidate_staged(&mut self, id: u64) {
        if let Some(record) = self.records.get_mut(&id) {
            for (name, job) in &self.jobs {
                if let Some(versions) = record.dense.get_mut(name) {
                    versions.retain(|v| v.version != job.target_version);
                }
            }
        }
    }

    fn validate(&self) -> Result<()> {
        if self.specs.len() > 32 || self.jobs.len() > 32 {
            return Err(invalid());
        }
        for (n, s) in &self.specs {
            name(n)?;
            name(&s.active_version)?;
            crate::validate_dimensions(s.dimensions)?;
            if s.history > 64 {
                return Err(invalid());
            }
        }
        for r in self.records.values() {
            if r.dense.len() > 32 || r.sparse.len() > 32 {
                return Err(invalid());
            }
            for (n, vs) in &r.dense {
                let s = self.specs.get(n).ok_or_else(invalid)?;
                if vs.len() > s.history + 2 {
                    return Err(invalid());
                }
                let mut seen = std::collections::BTreeSet::new();
                for v in vs {
                    name(&v.version)?;
                    if !seen.insert(&v.version) {
                        return Err(invalid());
                    }
                    crate::validate_vector(s.dimensions, &v.vector)?;
                }
            }
            for (n, v) in &r.sparse {
                name(n)?;
                sparse(v)?;
            }
        }
        for (n, j) in &self.jobs {
            if n != &j.name || !self.specs.contains_key(n) {
                return Err(invalid());
            }
            name(&j.target_version)?;
        }
        Ok(())
    }
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut w = Writer(Vec::new());
        w.u(1);
        w.u(self.specs.len() as u64);
        for (n, s) in &self.specs {
            w.s(n);
            w.u(s.dimensions as u64);
            w.u(match s.metric {
                DistanceMetric::SquaredL2 => 0,
                DistanceMetric::Cosine => 1,
                DistanceMetric::InnerProduct => 2,
            });
            w.s(&s.active_version);
            w.u(s.history as u64);
        }
        w.u(self.records.len() as u64);
        for (id, r) in &self.records {
            w.u(*id);
            w.u(r.dense.len() as u64);
            for (n, vs) in &r.dense {
                w.s(n);
                w.u(vs.len() as u64);
                for v in vs {
                    w.s(&v.version);
                    w.u(v.vector.len() as u64);
                    for x in &v.vector {
                        w.0.extend(x.to_le_bytes());
                    }
                }
            }
            w.u(r.sparse.len() as u64);
            for (n, v) in &r.sparse {
                w.s(n);
                w.u(v.len() as u64);
                for (i, x) in v {
                    w.u(u64::from(*i));
                    w.0.extend(x.to_le_bytes());
                }
            }
        }
        w.u(self.jobs.len() as u64);
        for (n, j) in &self.jobs {
            w.s(n);
            w.s(&j.target_version);
            w.u(u64::from(j.cursor.is_some()));
            w.u(j.cursor.unwrap_or(0));
        }
        if w.0.len() > crate::MAX_BATCH_BYTES {
            return Err(Error::BatchTooLarge);
        }
        Ok(w.0)
    }
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader(bytes);
        if r.u()? != 1 {
            return Err(invalid());
        }
        let mut data = Self::default();
        for _ in 0..r.count(32)? {
            let n = r.s()?;
            let dimensions = r.count(crate::MAX_DIMENSIONS)?;
            let metric = match r.u()? {
                0 => DistanceMetric::SquaredL2,
                1 => DistanceMetric::Cosine,
                2 => DistanceMetric::InnerProduct,
                _ => return Err(invalid()),
            };
            let active_version = r.s()?;
            let history = r.count(64)?;
            if data
                .specs
                .insert(
                    n,
                    VectorSpec {
                        dimensions,
                        metric,
                        active_version,
                        history,
                    },
                )
                .is_some()
            {
                return Err(invalid());
            }
        }
        for _ in 0..r.count(crate::MAX_BATCH_BYTES / 16)? {
            let id = r.u()?;
            let mut record = RecordVectors::default();
            for _ in 0..r.count(32)? {
                let n = r.s()?;
                let mut vs = Vec::new();
                for _ in 0..r.count(66)? {
                    let version = r.s()?;
                    let len = r.count(crate::MAX_DIMENSIONS)?;
                    let mut vector = Vec::new();
                    for _ in 0..len {
                        vector.push(r.f()?);
                    }
                    vs.push(VersionedVector { version, vector });
                }
                if record.dense.insert(n, vs).is_some() {
                    return Err(invalid());
                }
            }
            for _ in 0..r.count(32)? {
                let n = r.s()?;
                let mut vs = Vec::new();
                for _ in 0..r.count(crate::MAX_DIMENSIONS)? {
                    let i = u32::try_from(r.u()?).map_err(|_| invalid())?;
                    vs.push((i, r.f()?));
                }
                if record.sparse.insert(n, vs).is_some() {
                    return Err(invalid());
                }
            }
            if data.records.insert(id, record).is_some() {
                return Err(invalid());
            }
        }
        for _ in 0..r.count(32)? {
            let n = r.s()?;
            let target_version = r.s()?;
            let flag = r.u()?;
            if flag > 1 {
                return Err(invalid());
            }
            let cursor = r.u()?;
            let j = ReembedStatus {
                name: n.clone(),
                target_version,
                cursor: (flag == 1).then_some(cursor),
            };
            if data.jobs.insert(n, j).is_some() {
                return Err(invalid());
            }
        }
        if !r.0.is_empty() {
            return Err(invalid());
        }
        data.validate()?;
        Ok(data)
    }
    fn prune(&mut self, n: &str) {
        let s = &self.specs[n];
        let target = self.jobs.get(n).map(|j| j.target_version.as_str());
        for r in self.records.values_mut() {
            if let Some(vs) = r.dense.get_mut(n) {
                let mut previous = 0;
                let mut keep = vec![false; vs.len()];
                for (i, v) in vs.iter().enumerate().rev() {
                    if v.version == s.active_version || Some(v.version.as_str()) == target {
                        keep[i] = true;
                    } else if previous < s.history {
                        previous += 1;
                        keep[i] = true;
                    }
                }
                let mut i = 0;
                vs.retain(|_| {
                    let k = keep[i];
                    i += 1;
                    k
                });
            }
        }
    }
}
struct Writer(Vec<u8>);
impl Writer {
    fn u(&mut self, x: u64) {
        self.0.extend(x.to_le_bytes());
    }
    fn s(&mut self, s: &str) {
        self.u(s.len() as u64);
        self.0.extend(s.as_bytes());
    }
}
struct Reader<'a>(&'a [u8]);
impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if n > self.0.len() {
            return Err(invalid());
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().map_err(|_| invalid())?,
        ))
    }
    fn f(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(
            self.take(4)?.try_into().map_err(|_| invalid())?,
        ))
    }
    fn count(&mut self, max: usize) -> Result<usize> {
        let n = usize::try_from(self.u()?).map_err(|_| invalid())?;
        if n > max { Err(invalid()) } else { Ok(n) }
    }
    fn s(&mut self) -> Result<String> {
        let n = self.count(256)?;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| invalid())
    }
}
fn rank(mut hits: Vec<SearchHit>, k: usize) -> Vec<SearchHit> {
    hits.sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id)));
    hits.truncate(k);
    hits
}
impl<'a> CollectionRef<'a> {
    pub fn vector_specs(&self) -> Result<&'a BTreeMap<String, VectorSpec>> {
        self.db.ensure_healthy()?;
        Ok(&self.db.state(self.id).vectors.specs)
    }
    pub fn record_vectors(&self, id: u64) -> Result<Option<&'a RecordVectors>> {
        self.db.ensure_healthy()?;
        if self.get(id)?.is_none() {
            return Ok(None);
        }
        Ok(self.db.state(self.id).vectors.records.get(&id))
    }
    fn named_records(
        &self,
        n: &str,
        version: Option<&str>,
    ) -> Result<(VectorSpec, crate::records::RecordMap)> {
        self.db.ensure_healthy()?;
        let state = self.db.state(self.id);
        let spec = state.vectors.specs.get(n).ok_or_else(invalid)?.clone();
        let version = version.unwrap_or(&spec.active_version);
        name(version)?;
        let mut records = crate::records::RecordMap::new();
        for (&id, r) in &state.vectors.records {
            if state.records.contains_key(&id)
                && let Some(v) = r
                    .dense
                    .get(n)
                    .and_then(|vs| vs.iter().find(|v| v.version == version))
            {
                records.insert(
                    id,
                    Record {
                        vector: v.vector.clone(),
                        payload: Vec::new(),
                        metadata: Metadata::new(),
                    },
                );
            }
        }
        Ok((spec, records))
    }
    pub fn search_named(
        &self,
        n: &str,
        version: Option<&str>,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<SearchHit>> {
        self.search_named_at(n, version, query, k, crate::current_time_ms())
    }
    /// Exact named-vector search using one explicit expiry clock.
    pub fn search_named_at(
        &self,
        n: &str,
        version: Option<&str>,
        query: &[f32],
        k: usize,
        now_ms: u64,
    ) -> Result<Vec<SearchHit>> {
        let (s, rs) = self.named_records(n, version)?;
        crate::validate_vector(s.dimensions, query)?;
        let norm = crate::validate_metric_query(query, s.metric)?;
        Ok(rank(
            rs.iter()
                .filter(|(id, _)| self.get_at(**id, now_ms).ok().flatten().is_some())
                .filter_map(|(&id, r)| {
                    crate::metric_distance(
                        query,
                        &r.vector,
                        s.metric,
                        norm,
                        crate::vector_norm(&r.vector),
                    )
                    .map(|distance| SearchHit { id, distance })
                })
                .collect(),
            k,
        ))
    }
    /// Ready-only HNSW; reopen and mutations invalidate derived graphs.
    pub fn search_named_ann(
        &self,
        n: &str,
        version: Option<&str>,
        query: &[f32],
        k: usize,
        ef: usize,
    ) -> Result<Vec<SearchHit>> {
        self.search_named_ann_at(n, version, query, k, ef, crate::current_time_ms())
    }
    /// Ready-only ANN using one explicit expiry clock.
    pub fn search_named_ann_at(
        &self,
        n: &str,
        version: Option<&str>,
        query: &[f32],
        k: usize,
        ef: usize,
        now_ms: u64,
    ) -> Result<Vec<SearchHit>> {
        let (s, rs) = self.named_records(n, version)?;
        crate::validate_ann_metric(s.metric)?;
        crate::validate_vector(s.dimensions, query)?;
        crate::validate_metric_query(query, s.metric)?;
        if ef < k || ef > crate::MAX_EF_SEARCH || ef == 0 {
            return Err(Error::InvalidEfSearch);
        }
        let version = version.unwrap_or(&s.active_version);
        let index = self
            .db
            .state(self.id)
            .vector_indexes
            .get(&(n.into(), version.into()))
            .ok_or(Error::IndexNotReady)?;
        Ok(index.search_filtered_with_expiry(
            query,
            k,
            ef,
            AnnOptions::default(),
            &rs,
            &self.db.state(self.id).expiries,
            now_ms,
            &[],
            &mut SearchStats::default(),
        ))
    }
    /// Dot-product scoring through derived inverted postings (rebuilt on open).
    pub fn search_sparse(&self, n: &str, query: &[(u32, f32)], k: usize) -> Result<Vec<SearchHit>> {
        self.db.ensure_healthy()?;
        name(n)?;
        sparse(query)?;
        let state = self.db.state(self.id);
        let index = &state.sparse_index;
        let mut scores: BTreeMap<u64, f64> = BTreeMap::new();
        if let Some(ids) = index.ids.get(n) {
            for &id in ids {
                if self.get(id)?.is_some() {
                    scores.insert(id, 0.0);
                }
            }
        }
        for &(i, x) in query {
            if let Some(postings) = index.postings.get(&(n.to_string(), i)) {
                for (&id, &y) in postings {
                    if let Some(score) = scores.get_mut(&id) {
                        *score += f64::from(x) * f64::from(y);
                    }
                }
            }
        }
        Ok(rank(
            scores
                .into_iter()
                .map(|(id, score)| SearchHit {
                    id,
                    distance: -score,
                })
                .collect(),
            k,
        ))
    }

    pub fn search_dense_sparse(
        &self,
        query: &DenseSparseQuery,
    ) -> Result<Vec<crate::hybrid::HybridHit>> {
        let depth = query.fusion.candidates.unwrap_or(query.k.max(50));
        crate::hybrid::fuse_ranked(
            &self.search_named(
                &query.dense_name,
                query.version.as_deref(),
                &query.dense,
                depth,
            )?,
            &self.search_sparse(&query.sparse_name, &query.sparse, depth)?,
            query.k,
            query.fusion,
        )
    }
    pub fn reembed_status(&self, n: &str) -> Result<Option<&'a ReembedStatus>> {
        self.db.ensure_healthy()?;
        Ok(self.db.state(self.id).vectors.jobs.get(n))
    }
    /// Preferred iterator for work performed outside the database owner's lock.
    pub fn reembed_pending_checked(&self, n: &str, limit: usize) -> Result<Vec<ReembedRecord>> {
        self.reembed_pending(n, limit)?
            .into_iter()
            .map(|(id, record)| {
                let revision = self
                    .db
                    .state(self.id)
                    .revisions
                    .get(&id)
                    .copied()
                    .ok_or_else(invalid)?;
                Ok(ReembedRecord {
                    id,
                    revision,
                    record,
                })
            })
            .collect()
    }
    /// Returns missing target embeddings, including newly inserted IDs before the cursor.
    pub fn reembed_pending(&self, n: &str, limit: usize) -> Result<Vec<(u64, Record)>> {
        self.db.ensure_healthy()?;
        if limit > crate::MAX_BATCH_OPERATIONS {
            return Err(Error::BatchTooLarge);
        }
        let state = self.db.state(self.id);
        let j = state.vectors.jobs.get(n).ok_or_else(invalid)?;
        let mut out = Vec::new();
        for (&id, r) in &state.records {
            if out.len() == limit {
                break;
            }
            if self.get(id)?.is_some()
                && !state
                    .vectors
                    .records
                    .get(&id)
                    .and_then(|r| r.dense.get(n))
                    .is_some_and(|vs| vs.iter().any(|v| v.version == j.target_version))
            {
                out.push((id, r.clone()));
            }
        }
        Ok(out)
    }
}
#[derive(Clone, Debug)]
pub struct DenseSparseQuery {
    pub dense_name: String,
    pub version: Option<String>,
    pub dense: Vec<f32>,
    pub sparse_name: String,
    pub sparse: Vec<(u32, f32)>,
    pub k: usize,
    pub fusion: crate::hybrid::FusionOptions,
}
impl CollectionMut<'_> {
    fn commit_vectors(&mut self, data: VectorData) -> Result<()> {
        self.db.ensure_healthy()?;
        if self.db.version < crate::format::V5 {
            return Err(Error::InvalidQuery(
                "named and sparse vectors require format v5",
            ));
        }
        let bytes = data.encode()?;
        let seq = self
            .db
            .sequence
            .checked_add(1)
            .ok_or(Error::SequenceExhausted)?;
        let frame = crate::format::vector_frame(seq, self.id, &bytes);
        self.db.commit_frame(seq, &frame)?;
        let s = self.db.state_mut(self.id);
        s.sparse_index = SparseIndex::build(&data);
        s.vectors = data;
        s.vector_indexes.clear();
        Ok(())
    }
    pub fn configure_vector(&mut self, n: &str, spec: VectorSpec) -> Result<()> {
        name(n)?;
        let mut data = self.db.state(self.id).vectors.clone();
        if data.specs.contains_key(n) {
            return Err(invalid());
        }
        data.specs.insert(n.into(), spec);
        self.commit_vectors(data)
    }
    /// Atomic across all names and versions in this replacement; requires a live base record.
    pub fn put_vectors(&mut self, id: u64, vectors: RecordVectors) -> Result<()> {
        if self.view().get(id)?.is_none() {
            return Err(invalid());
        }
        let mut data = self.db.state(self.id).vectors.clone();
        data.records.insert(id, vectors);
        data.validate()?;
        let names: Vec<_> = data.specs.keys().cloned().collect();
        for n in names {
            data.prune(&n);
        }
        self.commit_vectors(data)
    }
    /// Upserts a model version and applies the collection's bounded retention policy.
    pub fn put_vector_version(
        &mut self,
        id: u64,
        n: &str,
        version: &str,
        vector: Vec<f32>,
    ) -> Result<()> {
        if self.view().get(id)?.is_none() {
            return Err(invalid());
        }
        name(version)?;
        let mut data = self.db.state(self.id).vectors.clone();
        let spec = data.specs.get(n).ok_or_else(invalid)?;
        crate::validate_vector(spec.dimensions, &vector)?;
        let vs = data
            .records
            .entry(id)
            .or_default()
            .dense
            .entry(n.into())
            .or_default();
        vs.retain(|v| v.version != version);
        vs.push(VersionedVector {
            version: version.into(),
            vector,
        });
        data.prune(n);
        self.commit_vectors(data)
    }
    pub fn prune_vector_history(&mut self, n: &str, history: usize) -> Result<()> {
        let mut data = self.db.state(self.id).vectors.clone();
        data.specs.get_mut(n).ok_or_else(invalid)?.history = history;
        data.prune(n);
        self.commit_vectors(data)
    }
    pub fn rebuild_named_hnsw(
        &mut self,
        n: &str,
        version: Option<&str>,
        config: HnswConfig,
    ) -> Result<()> {
        config.validate()?;
        let (s, rs) = self.view().named_records(n, version)?;
        crate::validate_ann_metric(s.metric)?;
        let version = version.unwrap_or(&s.active_version).to_string();
        let index = crate::hnsw::Index::build(&rs, config, s.metric, AnnOptions::default());
        self.db
            .state_mut(self.id)
            .vector_indexes
            .insert((n.into(), version), index);
        Ok(())
    }
    pub fn begin_reembed(&mut self, n: &str, target: &str) -> Result<()> {
        name(target)?;
        let mut data = self.db.state(self.id).vectors.clone();
        let spec = data.specs.get(n).ok_or_else(invalid)?;
        if data.jobs.contains_key(n) || spec.active_version == target {
            return Err(invalid());
        }
        data.jobs.insert(
            n.into(),
            ReembedStatus {
                name: n.into(),
                target_version: target.into(),
                cursor: None,
            },
        );
        self.commit_vectors(data)
    }
    /// Rejects an entire provider batch if any source changed while it was embedding.
    pub fn apply_reembed_batch_checked(&mut self, n: &str, batch: &[ReembedUpdate]) -> Result<()> {
        self.db.ensure_healthy()?;
        if batch.len() > crate::MAX_BATCH_OPERATIONS {
            return Err(Error::BatchTooLarge);
        }
        for update in batch {
            let current = self.db.state(self.id).revisions.get(&update.id).copied();
            crate::Expected::Revision(update.revision).check(current)?;
        }
        let batch: Vec<_> = batch.iter().map(|u| (u.id, u.vector.clone())).collect();
        self.apply_reembed_batch(n, &batch)
    }
    /// The cursor and all vectors commit in one checksummed frame. Retry is idempotent.
    pub fn apply_reembed_batch(&mut self, n: &str, batch: &[(u64, Vec<f32>)]) -> Result<()> {
        if batch.is_empty() || batch.len() > crate::MAX_BATCH_OPERATIONS {
            return Err(invalid());
        }
        let mut data = self.db.state(self.id).vectors.clone();
        let target = data.jobs.get(n).ok_or_else(invalid)?.target_version.clone();
        let dim = data.specs[n].dimensions;
        let mut seen = std::collections::BTreeSet::new();
        for (id, vector) in batch {
            if !seen.insert(*id) || self.view().get(*id)?.is_none() {
                return Err(invalid());
            }
            crate::validate_vector(dim, vector)?;
            let vs = data
                .records
                .entry(*id)
                .or_default()
                .dense
                .entry(n.into())
                .or_default();
            vs.retain(|v| v.version != target);
            vs.push(VersionedVector {
                version: target.clone(),
                vector: vector.clone(),
            });
        }
        let j = data.jobs.get_mut(n).ok_or_else(invalid)?;
        j.cursor = batch.iter().map(|(id, _)| *id).chain(j.cursor).max();
        data.prune(n);
        self.commit_vectors(data)
    }
    /// Validates completeness against current live records before atomically switching readers.
    pub fn finish_reembed(&mut self, n: &str) -> Result<()> {
        if !self.view().reembed_pending(n, 1)?.is_empty() {
            return Err(Error::InvalidQuery("re-embedding is incomplete"));
        }
        let mut data = self.db.state(self.id).vectors.clone();
        let job = data.jobs.remove(n).ok_or_else(invalid)?;
        data.specs.get_mut(n).ok_or_else(invalid)?.active_version = job.target_version;
        data.prune(n);
        self.commit_vectors(data)
    }
}
