//! Continuous log archive of a published baseline set and its exact
//! checkpoint replay (blob-pitr EPIC-PITR-001).
//!
//! A baseline is a published set whose database object is a byte copy of the
//! live file, so the live log continues it under the same lineage and file
//! header. Each archive checkpoint adds, under the baseline's own set
//! directory in the data container:
//!
//! - `objects/{sha256}` with the verified log segment (`VLSEG001`) that
//!   extends the previous checkpoint (or the baseline) by whole commits, and
//! - `objects/{sha256}` with the recovery bundle captured with that segment,
//!   bound to the checkpoint's chain hash instead of a database object;
//!
//! Legacy publication then replaces `archive.json` by ETag compare-and-swap.
//! HA publication instead creates an immutable index and commits its exact
//! hash through the writer authority head CAS; it never updates `archive.json`.
//! The committed index is the
//! only authority: a checkpoint exists only once the index names it, after
//! both objects were written and read back. The checkpoints form a
//! contiguous chain from the baseline sequence; each chain hash covers the
//! previous one, so a replay that verifies every hash up to the target
//! proves the exact segment sequence the archiver cut.
//!
//! Recovery targets are exact checkpoints only (`latest` or a checkpoint's
//! sequence): any other sequence or a time is refused, never rounded.
//! Objects live in the baseline set, so the set's pins, retention and sweep
//! cover the archive with the baseline it depends on.
use super::*;
use crate::server::authority::{AuthorityError, AuthorityStore, Claim, HeadRef, Observed, Phase};
use crate::{LogSegment, ReplicaFollower};

pub(in crate::server) const ARCHIVE_SCHEMA: &str = "vlite.cloud-backup-archive/1";
pub(in crate::server) const ROLE_SEGMENT: &str = "archive_segment";
pub(in crate::server) const ROLE_ARCHIVE_BUNDLE: &str = "archive_bundle";
/// Checkpoints one baseline can carry; the next baseline starts a new chain.
pub(in crate::server) const MAX_CHECKPOINTS: usize = 256;
/// Total segment bytes one baseline's archive can carry.
pub(in crate::server) const MAX_ARCHIVE_BYTES: u64 = 1 << 30;
/// Soft bound of one exported segment: a delta that does not fit in one
/// segment stops the chain (`pitr_delta_too_large`) rather than splitting a
/// checkpoint whose configuration was captured once.
pub(in crate::server) const MAX_SEGMENT_BYTES: usize = 64 << 20;
/// Frames replayed per follower apply (each apply replays the whole file).
const REPLAY_BATCH: usize = 64 << 20;

/// One archived recovery point: the database at exactly `sequence`, with
/// the configuration captured when its segment was cut.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::server) struct Checkpoint {
    pub sequence: u64,
    pub base_sequence: u64,
    pub segment_sha256: String,
    pub segment_bytes: u64,
    pub bundle_sha256: String,
    pub bundle_bytes: u64,
    pub chain_sha256: String,
    /// When the segment was cut (not the commit time of `sequence`).
    pub captured_unix_ms: u64,
}

/// `archive.json`: the authoritative list of a baseline's checkpoints.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::server) struct Index {
    pub schema: String,
    pub set_id: String,
    pub installation_id: String,
    pub source_lineage: String,
    /// SHA-256 of the file header every segment carries.
    pub header_sha256: String,
    pub baseline_sequence: u64,
    pub baseline_sha256: String,
    pub checkpoints: Vec<Checkpoint>,
}

/// A replay target. Only exact archived checkpoints are supported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) enum ArchiveTarget {
    Latest,
    Sequence(u64),
}

impl ArchiveTarget {
    /// `latest` or a decimal sequence; times and anything else are refused.
    pub(in crate::server) fn parse(text: &str) -> Result<Self, String> {
        if text == "latest" {
            return Ok(Self::Latest);
        }
        // Only the canonical decimal spelling: no sign, padding or spaces.
        if let Ok(sequence) = text.parse::<u64>()
            && sequence.to_string() == text
        {
            return Ok(Self::Sequence(sequence));
        }
        Err("--archive-checkpoint must be latest or the exact sequence of an archived checkpoint; times and other sequences are not supported".into())
    }
}

/// A baseline's archive as read for appending.
pub(in crate::server) struct Archive {
    pub set_id: String,
    pub manifest: Manifest,
    database_sha256: String,
    index: Option<(Index, String)>,
}

impl Archive {
    pub(in crate::server) fn checkpoints(&self) -> &[Checkpoint] {
        self.index.as_ref().map_or(&[], |(i, _)| &i.checkpoints)
    }

    /// The sequence the next segment continues from.
    pub(in crate::server) fn next_base(&self) -> u64 {
        self.checkpoints()
            .last()
            .map_or(self.manifest.database.commits, |c| c.sequence)
    }

    fn chain_tip(&self) -> &str {
        self.checkpoints()
            .last()
            .map_or(&self.database_sha256, |c| &c.chain_sha256)
    }

    fn bytes(&self) -> u64 {
        self.checkpoints().iter().map(|c| c.segment_bytes).sum()
    }
}

/// A database replayed to an exact checkpoint.
pub(in crate::server) struct Replayed {
    pub checkpoint: Checkpoint,
    pub bundle: recovery_bundle::Bundle,
    pub segments: usize,
    pub verification: maintenance::Verification,
}

pub(in crate::server) fn pitr(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, code, message)
}

/// Chain hash of a checkpoint: binds the previous hash (the baseline
/// database object for the first one), the segment and its sequence.
fn chain(previous: &str, segment_sha256: &str, sequence: u64) -> String {
    sha_hex(format!("{ARCHIVE_SCHEMA}\n{previous}\n{segment_sha256}\n{sequence}").as_bytes())
}

/// The recovery bundle of a checkpoint must name its chain hash, the
/// baseline's lineage, installation and admitted policy.
fn check_bundle(
    bundle: &recovery_bundle::Bundle,
    manifest: &Manifest,
    chain_sha256: &str,
) -> Result<(), ApiError> {
    if bundle.installation_id != manifest.installation_id
        || bundle.source_lineage != manifest.source_lineage
        || bundle.database_sha256 != chain_sha256
        || bundle.policy != manifest.policy
    {
        return Err(corrupt(
            "the checkpoint recovery bundle belongs to another capture",
        ));
    }
    Ok(())
}

impl Index {
    /// Checks identity against the baseline and the whole chain.
    fn check(
        &self,
        set_id: &str,
        manifest: &Manifest,
        database_sha256: &str,
    ) -> Result<(), ApiError> {
        let bad = || corrupt("the archive index is invalid or belongs to another baseline");
        if self.schema != ARCHIVE_SCHEMA
            || self.set_id != set_id
            || self.installation_id != manifest.installation_id
            || Some(&self.source_lineage) != manifest.source_lineage.as_ref()
            || self.baseline_sequence != manifest.database.commits
            || self.baseline_sha256 != database_sha256
            || !is_sha256(&self.header_sha256)
            || self.checkpoints.is_empty()
            || self.checkpoints.len() > MAX_CHECKPOINTS
        {
            return Err(bad());
        }
        let (mut base, mut tip, mut total) = (self.baseline_sequence, database_sha256, 0u64);
        for c in &self.checkpoints {
            total = total.saturating_add(c.segment_bytes);
            if c.base_sequence != base
                || c.sequence <= base
                || !is_sha256(&c.segment_sha256)
                || !is_sha256(&c.bundle_sha256)
                || c.bundle_bytes > recovery_bundle::MAX_BYTES
                || c.chain_sha256 != chain(tip, &c.segment_sha256, c.sequence)
                || total > MAX_ARCHIVE_BYTES
            {
                return Err(bad());
            }
            base = c.sequence;
            tip = &c.chain_sha256;
        }
        Ok(())
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::server) const HA_INDEX_SCHEMA: &str = "vlite.cloud-backup-ha-archive/1";
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::server) const HA_CHECKPOINT_SCHEMA: &str = "vlite.cloud-backup-ha-checkpoint/1";
const HA_CHAIN_DOMAIN: &str = "vlite.cloud-backup-ha-chain/1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::server) struct HaCheckpoint {
    pub schema: String,
    pub authority: HaIdentity,
    pub checkpoint: Checkpoint,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::server) struct HaIndex {
    pub schema: String,
    pub authority: HaIdentity,
    pub prior_head_sha256: String,
    pub prior_head: HeadRef,
    pub set_id: String,
    pub installation_id: String,
    pub header_sha256: String,
    pub baseline_sequence: u64,
    pub baseline_sha256: String,
    pub checkpoints: Vec<HaCheckpoint>,
}

#[derive(Debug)]
pub(in crate::server) struct HaArchive {
    pub set_id: String,
    pub manifest: HaManifest,
    pub head: HeadRef,
    database_sha256: String,
    index: Option<HaIndex>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl HaArchive {
    pub(in crate::server) fn checkpoints(&self) -> &[HaCheckpoint] {
        self.index.as_ref().map_or(&[], |i| &i.checkpoints)
    }

    pub(in crate::server) fn next_base(&self) -> u64 {
        self.head.sequence
    }
}

/// Keep fencing and the full conflicting observation available to the caller.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub(in crate::server) enum HaArchiveError {
    Authority(AuthorityError),
    Api(ApiError),
}

impl From<AuthorityError> for HaArchiveError {
    fn from(error: AuthorityError) -> Self {
        Self::Authority(error)
    }
}

impl From<ApiError> for HaArchiveError {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}

impl From<HaArchiveError> for ApiError {
    fn from(error: HaArchiveError) -> Self {
        match error {
            HaArchiveError::Authority(error) => ha_authority_error(error),
            HaArchiveError::Api(error) => error,
        }
    }
}

/// Canonical digest of the complete prior authority head, including its index.
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::server) fn ha_head_digest(head: &HeadRef) -> Result<String, ApiError> {
    Ok(sha_hex(&encode(head)?))
}

fn ha_chain(
    previous: &str,
    identity: &HaIdentity,
    base_sequence: u64,
    sequence: u64,
    segment_sha256: &str,
) -> String {
    // Length-prefix the variable fields; no identity spelling can alias a boundary.
    let mut bytes = HA_CHAIN_DOMAIN.as_bytes().to_vec();
    for field in [previous, &identity.writer_id, segment_sha256] {
        bytes.extend_from_slice(&(field.len() as u64).to_be_bytes());
        bytes.extend_from_slice(field.as_bytes());
    }
    bytes.extend_from_slice(&identity.epoch.to_be_bytes());
    bytes.extend_from_slice(&base_sequence.to_be_bytes());
    bytes.extend_from_slice(&sequence.to_be_bytes());
    sha_hex(&bytes)
}

#[cfg_attr(not(test), allow(dead_code))]
impl HaIndex {
    pub(in crate::server) fn check(
        &self,
        set_id: &str,
        manifest: &HaManifest,
        head: &HeadRef,
    ) -> Result<(), ApiError> {
        manifest.check()?;
        let baseline = &manifest.manifest;
        let database = baseline.check(set_id, &baseline.installation_id)?.0;
        check_database_size(database.bytes)?;
        let bad = || corrupt("the HA archive index is invalid or belongs to another baseline");
        if self.schema != HA_INDEX_SCHEMA
            || self.authority != manifest.authority
            || self.set_id != set_id
            || self.installation_id != baseline.installation_id
            || self.baseline_sequence != baseline.database.commits
            || self.baseline_sha256 != database.sha256
            || !is_sha256(&self.header_sha256)
            || self.checkpoints.is_empty()
            || self.checkpoints.len() > MAX_CHECKPOINTS
            || head.epoch != self.authority.epoch
            || head.set_id != set_id
            || !is_sha256(&head.manifest_sha256)
            || !head.index_sha256.as_deref().is_some_and(is_sha256)
            || self.prior_head_sha256 != ha_head_digest(&self.prior_head)?
            || self.prior_head.epoch != head.epoch
            || self.prior_head.set_id != head.set_id
            || self.prior_head.manifest_sha256 != head.manifest_sha256
        {
            return Err(bad());
        }
        let (mut base, mut tip, mut total, mut epoch) = (
            self.baseline_sequence,
            self.baseline_sha256.as_str(),
            0u64,
            manifest.authority.epoch,
        );
        for (n, entry) in self.checkpoints.iter().enumerate() {
            let c = &entry.checkpoint;
            total = total.saturating_add(c.segment_bytes);
            if entry.schema != HA_CHECKPOINT_SCHEMA
                || entry.authority.epoch < epoch
                || entry.authority != manifest.authority
                || c.base_sequence != base
                || c.sequence <= base
                || !is_sha256(&c.segment_sha256)
                || !is_sha256(&c.bundle_sha256)
                || c.segment_bytes == 0
                || c.segment_bytes > MAX_SEGMENT_BYTES as u64
                || c.bundle_bytes == 0
                || c.bundle_bytes > recovery_bundle::MAX_BYTES
                || c.chain_sha256
                    != ha_chain(tip, &entry.authority, base, c.sequence, &c.segment_sha256)
                || total > MAX_ARCHIVE_BYTES
            {
                return Err(bad());
            }
            if n + 1 == self.checkpoints.len()
                && (self.prior_head.sequence != base
                    || self.prior_head.chain_sha256 != tip
                    || (n == 0 && self.prior_head.index_sha256.is_some())
                    || (n > 0
                        && !self
                            .prior_head
                            .index_sha256
                            .as_deref()
                            .is_some_and(is_sha256)))
            {
                return Err(bad());
            }
            base = c.sequence;
            tip = &c.chain_sha256;
            epoch = entry.authority.epoch;
        }
        if head.sequence != base || head.chain_sha256 != tip {
            return Err(bad());
        }
        Ok(())
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::server) fn parse_ha_index(bytes: &[u8]) -> Result<HaIndex, ApiError> {
    if bytes.len() as u64 > META_MAX {
        return Err(corrupt("the HA archive index is too large"));
    }
    let index: HaIndex = decode("HA archive index", bytes)?;
    if index.schema != HA_INDEX_SCHEMA {
        return Err(corrupt("unsupported HA archive index schema"));
    }
    Ok(index)
}

#[cfg_attr(not(test), allow(dead_code))]
#[allow(clippy::result_large_err)]
impl Sets {
    /// Opens only the exact immutable index named by `head`; never archive.json.
    /// This also supports an explicitly selected frozen head for recovery.
    pub(in crate::server) fn open_archive_ha_at(
        &self,
        head: &HeadRef,
    ) -> Result<HaArchive, ApiError> {
        if !valid_set_id(&head.set_id)
            || head.epoch == 0
            || !is_sha256(&head.manifest_sha256)
            || !is_sha256(&head.chain_sha256)
            || head
                .index_sha256
                .as_deref()
                .is_some_and(|sha| !is_sha256(sha))
        {
            return Err(corrupt("the HA archive head is invalid"));
        }
        let id = &head.set_id;
        let key = self.record_key(id);
        let blob = self.meta.get(&key, META_MAX).map_err(missing_object)?;
        let record = decode_record(&key, &blob.bytes, id)?;
        if record.state != SetState::Published || record.manifest_sha256 != head.manifest_sha256 {
            return Err(corrupt("the HA head does not name a published baseline"));
        }
        let blob = self
            .data
            .get(&self.manifest_key(id), META_MAX)
            .map_err(missing_object)?;
        if sha_hex(&blob.bytes) != head.manifest_sha256 {
            return Err(corrupt("the HA manifest does not match the authority head"));
        }
        let manifest = parse_ha_manifest(&blob.bytes)?;
        if manifest.authority.epoch != head.epoch {
            return Err(corrupt(
                "the HA manifest epoch differs from the authority head",
            ));
        }
        let (database, bundle) = manifest.manifest.check(id, &record.installation_id)?;
        check_database_size(database.bytes)?;
        if bundle.bytes > recovery_bundle::MAX_BYTES {
            return Err(corrupt("the HA baseline recovery bundle is too large"));
        }
        let database_sha256 = database.sha256.clone();
        let index = match &head.index_sha256 {
            Some(sha) => {
                let blob = self
                    .data
                    .get(&self.object_key(id, sha), META_MAX)
                    .map_err(missing_object)?;
                if sha_hex(&blob.bytes) != *sha {
                    return Err(corrupt("the HA index does not match the authority head"));
                }
                let index = parse_ha_index(&blob.bytes)?;
                index.check(id, &manifest, head)?;
                Some(index)
            }
            None => {
                if head.sequence != manifest.manifest.database.commits
                    || head.chain_sha256 != database_sha256
                {
                    return Err(corrupt("the HA head does not match its baseline"));
                }
                None
            }
        };
        Ok(HaArchive {
            set_id: id.clone(),
            manifest,
            head: head.clone(),
            database_sha256,
            index,
        })
    }

    pub(in crate::server) fn open_archive_ha(
        &self,
        authority: &AuthorityStore,
        claim: &Claim,
    ) -> Result<HaArchive, HaArchiveError> {
        let observed = authority.verify_writer(claim)?;
        if observed.record.phase != Phase::Serving {
            return Err(AuthorityError::Invalid.into());
        }
        let head = observed
            .record
            .head
            .as_ref()
            .ok_or(AuthorityError::Invalid)?;
        let archive = self.open_archive_ha_at(head)?;
        Self::check_archive_authority(&archive, &observed)?;
        Ok(archive)
    }

    fn check_archive_authority(
        archive: &HaArchive,
        observed: &Observed,
    ) -> Result<(), HaArchiveError> {
        let identity = &archive.manifest.authority;
        if observed.record.phase != Phase::Serving {
            return Err(AuthorityError::Invalid.into());
        }
        if observed.record.head.as_ref() != Some(&archive.head) {
            return Err(AuthorityError::HeadMoved(observed.clone()).into());
        }
        if observed.record.writer_id.as_deref() != Some(&identity.writer_id)
            || observed.record.epoch != identity.epoch
            || observed.record.scope_id != identity.scope_id
            || observed.record.lineage != identity.lineage
            || observed.record.installation_id != archive.manifest.manifest.installation_id
        {
            return Err(AuthorityError::Invalid.into());
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::server) fn append_checkpoint_ha(
        &self,
        archive: &HaArchive,
        segment: &LogSegment,
        bundle: &dyn Fn(&str) -> Result<Vec<u8>, ApiError>,
        now: u64,
        authorize: Authorize<'_>,
        authority: &AuthorityStore,
        claim: &Claim,
    ) -> Result<HaCheckpoint, HaArchiveError> {
        let observed = authority.verify_writer(claim)?;
        Self::check_archive_authority(archive, &observed)?;
        let id = &archive.set_id;
        let manifest = &archive.manifest.manifest;
        let identity = &archive.manifest.authority;
        if archive.head.set_id != *id {
            return Err(corrupt("the HA archive set differs from its head").into());
        }
        if Some(lineage_hex(segment.lineage)) != manifest.source_lineage
            || segment.base_sequence != archive.next_base()
            || segment.last_sequence > segment.head_sequence
            || archive
                .index
                .as_ref()
                .is_some_and(|i| i.header_sha256 != sha_hex(&segment.header))
        {
            return Err(pitr(
                "pitr_source_diverged",
                "the segment does not continue the HA archive",
            )
            .into());
        }
        if segment.last_sequence < segment.head_sequence {
            return Err(pitr(
                "pitr_delta_too_large",
                "the commits exceed one archive segment",
            )
            .into());
        }
        if segment.is_empty() || segment.last_sequence < segment.base_sequence {
            return Err(internal("an empty or reversed segment is not a checkpoint").into());
        }
        if archive.checkpoints().len() >= MAX_CHECKPOINTS {
            return Err(pitr(
                "pitr_archive_full",
                "the HA archive reached its checkpoint limit",
            )
            .into());
        }
        // Check before serializing so an oversized segment is never copied.
        let bytes = (segment.frames.len() as u64)
            .saturating_add(segment.header.len() as u64)
            .saturating_add(84);
        if bytes > MAX_SEGMENT_BYTES as u64 {
            return Err(pitr(
                "pitr_delta_too_large",
                "the HA segment exceeds its size limit",
            )
            .into());
        }
        let total: u64 = archive
            .checkpoints()
            .iter()
            .map(|c| c.checkpoint.segment_bytes)
            .sum();
        if total.saturating_add(bytes) > MAX_ARCHIVE_BYTES {
            return Err(pitr("pitr_archive_full", "the HA archive reached its size limit").into());
        }
        let segment_upload = Upload::bytes(ROLE_SEGMENT, segment.to_bytes());
        let chain_sha256 = ha_chain(
            &archive.head.chain_sha256,
            identity,
            segment.base_sequence,
            segment.last_sequence,
            &segment_upload.sha256,
        );
        let bundle_bytes = bundle(&chain_sha256)?;
        let parsed = recovery_bundle::parse(&bundle_bytes).map_err(internal)?;
        check_bundle(&parsed, manifest, &chain_sha256)
            .map_err(|_| internal("the checkpoint bundle does not match its baseline"))?;
        let bundle_upload = Upload::bytes(ROLE_ARCHIVE_BUNDLE, bundle_bytes);
        let checkpoint = HaCheckpoint {
            schema: HA_CHECKPOINT_SCHEMA.into(),
            authority: identity.clone(),
            checkpoint: Checkpoint {
                sequence: segment.last_sequence,
                base_sequence: segment.base_sequence,
                segment_sha256: segment_upload.sha256.clone(),
                segment_bytes: segment_upload.bytes,
                bundle_sha256: bundle_upload.sha256.clone(),
                bundle_bytes: bundle_upload.bytes,
                chain_sha256,
                captured_unix_ms: now,
            },
        };
        let mut next = HaIndex {
            schema: HA_INDEX_SCHEMA.into(),
            authority: identity.clone(),
            prior_head_sha256: ha_head_digest(&archive.head)?,
            prior_head: archive.head.clone(),
            set_id: id.clone(),
            installation_id: manifest.installation_id.clone(),
            header_sha256: sha_hex(&segment.header),
            baseline_sequence: manifest.database.commits,
            baseline_sha256: archive.database_sha256.clone(),
            checkpoints: archive.checkpoints().to_vec(),
        };
        next.checkpoints.push(checkpoint.clone());
        let index_bytes = encode(&next)?;
        if index_bytes.len() as u64 > META_MAX {
            return Err(pitr(
                "pitr_archive_full",
                "the HA archive index exceeds its metadata limit",
            )
            .into());
        }
        let index_upload = Upload::bytes("ha_archive_index", index_bytes);
        let head = HeadRef {
            epoch: claim.epoch,
            set_id: id.clone(),
            manifest_sha256: archive.head.manifest_sha256.clone(),
            index_sha256: Some(index_upload.sha256.clone()),
            sequence: checkpoint.checkpoint.sequence,
            chain_sha256: checkpoint.checkpoint.chain_sha256.clone(),
        };
        next.check(id, &archive.manifest, &head)?;
        authorize()?;
        self.upload(id, &segment_upload)?;
        self.upload(id, &bundle_upload)?;
        self.upload(id, &index_upload)?;
        self.open_archive_ha_at(&archive.head)?;
        authorize()?;
        // Never retry with a successor ETag. AuthorityStore reconciles Unknown
        // only against its exact intended bytes, retaining typed CAS failures.
        authority.advance_head(claim, &observed, head)?;
        Ok(checkpoint)
    }
}

impl Sets {
    fn archive_key(&self, id: &str) -> String {
        format!("{}archive.json", self.set_dir(id))
    }

    /// The published baseline, its manifest and its current archive.
    pub(in crate::server) fn open_archive(&self, id: &str) -> Result<Archive, ApiError> {
        let missing = || {
            pitr(
                "pitr_baseline_missing",
                "the baseline set is not published in the configured target; the next snapshot publishes a new baseline",
            )
        };
        let key = self.record_key(id);
        let record = match self.meta.get(&key, META_MAX) {
            Ok(blob) => decode_record(&key, &blob.bytes, id)?,
            Err(StoreError::NotFound) => return Err(missing()),
            Err(e) => return Err(e.into()),
        };
        if record.state != SetState::Published {
            return Err(missing());
        }
        let blob = self
            .data
            .get(&self.manifest_key(id), META_MAX)
            .map_err(|e| match e {
                StoreError::NotFound => corrupt("a published set object is missing"),
                e => e.into(),
            })?;
        if sha_hex(&blob.bytes) != record.manifest_sha256 {
            return Err(corrupt("the manifest does not match the published record"));
        }
        let manifest = parse_manifest(&blob.bytes)?;
        let database_sha256 = manifest
            .check(id, &record.installation_id)?
            .0
            .sha256
            .clone();
        if manifest.source_lineage.is_none() {
            return Err(pitr(
                "pitr_baseline_unsupported",
                "the baseline database has no lineage; it cannot anchor a log archive",
            ));
        }
        let key = self.archive_key(id);
        let index = match self.data.get(&key, META_MAX) {
            Ok(blob) => {
                let index: Index = decode(&key, &blob.bytes)?;
                index.check(id, &manifest, &database_sha256)?;
                Some((index, blob.etag))
            }
            Err(StoreError::NotFound) => None,
            Err(e) => return Err(e.into()),
        };
        Ok(Archive {
            set_id: id.into(),
            manifest,
            database_sha256,
            index,
        })
    }

    /// Publishes one checkpoint after `archive`: verifies that `segment`
    /// continues it, uploads and reads back the segment and the bundle that
    /// `bundle` builds for the checkpoint's chain hash, rechecks that the
    /// baseline is still published, then commits the index by CAS. An
    /// ambiguous commit is accepted only when the index now holds exactly
    /// this checkpoint.
    pub(in crate::server) fn append_checkpoint(
        &self,
        archive: &Archive,
        segment: &LogSegment,
        bundle: &dyn Fn(&str) -> Result<Vec<u8>, ApiError>,
        now: u64,
        authorize: Authorize<'_>,
    ) -> Result<Checkpoint, ApiError> {
        let id = &archive.set_id;
        let manifest = &archive.manifest;
        let diverged = |what: &str| {
            pitr(
                "pitr_source_diverged",
                format!("{what}; archiving stops until the next baseline snapshot"),
            )
        };
        if Some(lineage_hex(segment.lineage)) != manifest.source_lineage {
            return Err(diverged(
                "the live database lineage differs from the baseline",
            ));
        }
        if segment.base_sequence != archive.next_base() {
            return Err(diverged("the segment does not continue the archive"));
        }
        if segment.last_sequence < segment.head_sequence {
            return Err(pitr(
                "pitr_delta_too_large",
                "the commits since the last checkpoint exceed one archive segment; shorten the archive interval (archiving resumes after the next baseline snapshot)",
            ));
        }
        if segment.is_empty() {
            return Err(internal("an empty segment is not a checkpoint"));
        }
        let header_sha256 = sha_hex(&segment.header);
        if let Some((index, _)) = &archive.index
            && index.header_sha256 != header_sha256
        {
            return Err(diverged(
                "the live database header differs from the archive",
            ));
        }
        if archive.checkpoints().len() >= MAX_CHECKPOINTS {
            return Err(pitr(
                "pitr_archive_full",
                "the baseline's archive holds the maximum number of checkpoints; the next baseline snapshot starts a new archive",
            ));
        }
        let data = segment.to_bytes();
        if archive.bytes().saturating_add(data.len() as u64) > MAX_ARCHIVE_BYTES {
            return Err(pitr(
                "pitr_archive_full",
                "the baseline's archive reached its size limit; the next baseline snapshot starts a new archive",
            ));
        }
        let segment_upload = Upload::bytes(ROLE_SEGMENT, data);
        let chain_sha256 = chain(
            archive.chain_tip(),
            &segment_upload.sha256,
            segment.last_sequence,
        );
        let bundle_bytes = bundle(&chain_sha256)?;
        let parsed = recovery_bundle::parse(&bundle_bytes).map_err(internal)?;
        check_bundle(&parsed, manifest, &chain_sha256)
            .map_err(|_| internal("the checkpoint bundle does not match its baseline"))?;
        let bundle_upload = Upload::bytes(ROLE_ARCHIVE_BUNDLE, bundle_bytes);
        let checkpoint = Checkpoint {
            sequence: segment.last_sequence,
            base_sequence: segment.base_sequence,
            segment_sha256: segment_upload.sha256.clone(),
            segment_bytes: segment_upload.bytes,
            bundle_sha256: bundle_upload.sha256.clone(),
            bundle_bytes: bundle_upload.bytes,
            chain_sha256,
            captured_unix_ms: now,
        };
        let mut next = archive.index.as_ref().map_or_else(
            || Index {
                schema: ARCHIVE_SCHEMA.into(),
                set_id: id.clone(),
                installation_id: manifest.installation_id.clone(),
                source_lineage: lineage_hex(segment.lineage),
                header_sha256,
                baseline_sequence: manifest.database.commits,
                baseline_sha256: archive.database_sha256.clone(),
                checkpoints: Vec::new(),
            },
            |(index, _)| index.clone(),
        );
        next.checkpoints.push(checkpoint.clone());

        authorize()?;
        self.upload(id, &segment_upload)?;
        self.upload(id, &bundle_upload)?;
        // Objects first; the index names them only while the baseline is
        // still published (a collected set never gains an archive).
        let current = self.open_archive(id)?;
        if current.index.as_ref().map(|(i, _)| i) != archive.index.as_ref().map(|(i, _)| i) {
            return Err(pitr(
                "pitr_archive_conflict",
                "the archive changed while this checkpoint was prepared; the next run retries",
            ));
        }
        authorize()?;
        let bytes = encode(&next)?;
        let cond = match &archive.index {
            Some((_, etag)) => Cond::Match(etag),
            None => Cond::Create,
        };
        match self.data.put(&self.archive_key(id), &bytes, cond) {
            Ok(_) => Ok(checkpoint),
            Err(StoreError::Conflict | StoreError::Unknown(_)) => {
                match self.open_archive(id)?.index {
                    Some((index, _)) if index == next => Ok(checkpoint),
                    _ => Err(pitr(
                        "pitr_archive_conflict",
                        "the archive index commit was not confirmed; the next run retries from the committed index",
                    )),
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Replays the verified archive of `fetched`'s set onto its downloaded
    /// baseline (in place) up to exactly `target`, verifying every segment
    /// hash, the chain, contiguity, lineage and header, then the result and
    /// the checkpoint's recovery bundle.
    pub(in crate::server) fn replay_archive(
        &self,
        fetched: &Fetched,
        target: ArchiveTarget,
        dir: &FilePath,
    ) -> Result<Replayed, ApiError> {
        let id = &fetched.record.set_id;
        let manifest = &fetched.manifest;
        let Some(index) = self.read_index(id, &fetched.record.installation_id, manifest)? else {
            return Err(not_found(
                "pitr_archive_missing",
                "the set has no archived checkpoints; restore the snapshot without --archive-checkpoint",
            ));
        };
        let position = match target {
            ArchiveTarget::Latest => index.checkpoints.len() - 1,
            ArchiveTarget::Sequence(sequence) => index
                .checkpoints
                .iter()
                .position(|c| c.sequence == sequence)
                .ok_or_else(|| {
                    let list: Vec<String> =
                        index.checkpoints.iter().map(|c| c.sequence.to_string()).collect();
                    pitr(
                        "pitr_target_unsupported",
                        format!(
                            "sequence {sequence} is not an archived checkpoint of this set; supported targets are exactly: {}",
                            list.join(", ")
                        ),
                    )
                })?,
        };
        let path = &index.checkpoints[..=position];
        let needed: u64 = path.iter().map(|c| c.segment_bytes).sum();
        check_restore_space(
            free_bytes(dir),
            restore_space_needed(fetched.verification.bytes.saturating_add(needed), 0),
        )?;
        let mut follower = ReplicaFollower::open(&fetched.database, None)
            .map_err(|e| corrupt(format!("the baseline cannot be replayed: {e}")))?;
        if follower.applied_sequence() != index.baseline_sequence {
            return Err(corrupt("the baseline does not match the archive"));
        }
        self.apply_segments(id, manifest, &index, path, &mut follower)?;
        let checkpoint = path[position].clone();
        let verification = seal(follower, &fetched.database, checkpoint.sequence, manifest)?;
        let bundle = self.checkpoint_bundle(id, manifest, &checkpoint)?;
        Ok(Replayed {
            checkpoint,
            bundle,
            segments: path.len(),
            verification,
        })
    }

    /// Advances `database`, a verified copy of `record`'s baseline that
    /// earlier follows may already have advanced, to the newest archived
    /// checkpoint. Only the verified checkpoints after the copy's own
    /// position are applied, with the same checks as a replay, and the head
    /// checkpoint's recovery bundle is verified before any of them is; the
    /// copy is never moved past the verified head. A copy that is not at the
    /// baseline or an archived checkpoint of this chain cannot continue it
    /// and is reported for a re-seed. Store and verification failures are
    /// errors and leave the copy at a whole checkpoint.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(in crate::server) fn follow_archive(
        &self,
        record: &Record,
        manifest: &Manifest,
        database: &FilePath,
    ) -> Result<Follow, ApiError> {
        let id = &record.set_id;
        let mut follower = match ReplicaFollower::open(database, None) {
            Ok(follower) => follower,
            Err(e) => return Ok(Follow::Reseed(format!("the copy cannot be followed: {e}"))),
        };
        if follower.db().lineage().map(lineage_hex) != manifest.source_lineage {
            return Ok(Follow::Reseed("the copy has another lineage".into()));
        }
        let applied = follower.applied_sequence();
        let Some(index) = self.read_index(id, &record.installation_id, manifest)? else {
            return Ok(if applied == manifest.database.commits {
                Follow::Current { sequence: applied }
            } else {
                Follow::Reseed("the copy is not at the baseline and the set has no archive".into())
            });
        };
        let start = if applied == index.baseline_sequence {
            0
        } else {
            match index.checkpoints.iter().position(|c| c.sequence == applied) {
                Some(position) => position + 1,
                None => {
                    return Ok(Follow::Reseed(format!(
                        "the copy at sequence {applied} is not an archived checkpoint of this set"
                    )));
                }
            }
        };
        let path = &index.checkpoints[start..];
        let Some(checkpoint) = path.last() else {
            return Ok(Follow::Current { sequence: applied });
        };
        let needed: u64 = path.iter().map(|c| c.segment_bytes).sum();
        let dir = database.parent().unwrap_or(FilePath::new("."));
        check_restore_space(free_bytes(dir), restore_space_needed(needed, 0))?;
        self.checkpoint_bundle(id, manifest, checkpoint)?;
        self.apply_segments(id, manifest, &index, path, &mut follower)?;
        seal(follower, database, checkpoint.sequence, manifest)?;
        Ok(Follow::Applied {
            sequence: checkpoint.sequence,
            segments: path.len(),
        })
    }

    /// The recovery bundle `checkpoint` names, checked against its hash
    /// and against `manifest` and the checkpoint's chain hash.
    fn checkpoint_bundle(
        &self,
        id: &str,
        manifest: &Manifest,
        checkpoint: &Checkpoint,
    ) -> Result<recovery_bundle::Bundle, ApiError> {
        let blob = self
            .data
            .get(
                &self.object_key(id, &checkpoint.bundle_sha256),
                checkpoint.bundle_bytes,
            )
            .map_err(missing_object)?;
        if blob.bytes.len() as u64 != checkpoint.bundle_bytes
            || sha_hex(&blob.bytes) != checkpoint.bundle_sha256
        {
            return Err(corrupt(
                "the checkpoint recovery bundle does not match its hash",
            ));
        }
        let bundle = recovery_bundle::parse(&blob.bytes).map_err(corrupt)?;
        check_bundle(&bundle, manifest, &checkpoint.chain_sha256)?;
        Ok(bundle)
    }

    /// The set's archive index checked against its baseline; `None` when
    /// the set has no archive.
    fn read_index(
        &self,
        id: &str,
        installation_id: &str,
        manifest: &Manifest,
    ) -> Result<Option<Index>, ApiError> {
        let database_sha256 = &manifest.check(id, installation_id)?.0.sha256;
        let key = self.archive_key(id);
        let index: Index = match self.data.get(&key, META_MAX) {
            Ok(blob) => decode(&key, &blob.bytes)?,
            Err(StoreError::NotFound) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        index.check(id, manifest, database_sha256)?;
        Ok(Some(index))
    }

    /// Downloads, verifies and applies `path`, contiguous checkpoints of
    /// `index` that continue `follower`, in batches that end on a
    /// checkpoint.
    fn apply_segments(
        &self,
        id: &str,
        manifest: &Manifest,
        index: &Index,
        path: &[Checkpoint],
        follower: &mut ReplicaFollower,
    ) -> Result<(), ApiError> {
        let mut batch: Option<LogSegment> = None;
        for (n, c) in path.iter().enumerate() {
            let blob = self
                .data
                .get(&self.object_key(id, &c.segment_sha256), c.segment_bytes)
                .map_err(missing_object)?;
            if blob.bytes.len() as u64 != c.segment_bytes
                || sha_hex(&blob.bytes) != c.segment_sha256
            {
                return Err(corrupt("an archived segment does not match its hash"));
            }
            let segment = LogSegment::from_bytes(&blob.bytes)
                .map_err(|_| corrupt("an archived segment is invalid"))?;
            if segment.base_sequence != c.base_sequence
                || segment.last_sequence != c.sequence
                || Some(lineage_hex(segment.lineage)) != manifest.source_lineage
                || sha_hex(&segment.header) != index.header_sha256
            {
                return Err(corrupt("an archived segment does not match its checkpoint"));
            }
            match &mut batch {
                Some(open) => {
                    open.frames.extend_from_slice(&segment.frames);
                    open.last_sequence = segment.last_sequence;
                    open.head_sequence = segment.last_sequence;
                }
                None => batch = Some(segment),
            }
            let last = n + 1 == path.len();
            if last
                || batch
                    .as_ref()
                    .is_some_and(|b| b.frames.len() >= REPLAY_BATCH)
            {
                let ready = batch.take().expect("a batch is open");
                follower.apply(&ready).map_err(|e| {
                    corrupt(format!(
                        "the archived segments do not extend the baseline: {e}"
                    ))
                })?;
            }
        }
        Ok(())
    }
}

/// What one [`Sets::follow_archive`] step did to a copy.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::server) enum Follow {
    /// `segments` verified checkpoints were applied, up to `sequence`.
    Applied { sequence: u64, segments: usize },
    /// The copy is at the newest archived checkpoint (or the baseline).
    Current { sequence: u64 },
    /// The copy cannot continue this archive; seed a fresh copy.
    Reseed(String),
}

fn missing_object(e: StoreError) -> ApiError {
    match e {
        StoreError::NotFound => corrupt("an archived checkpoint object is missing"),
        StoreError::TooLarge => corrupt("an archived object is larger than its index entry"),
        e => e.into(),
    }
}

/// Closes `follower` at exactly `sequence` and verifies the file it wrote.
fn seal(
    follower: ReplicaFollower,
    database: &FilePath,
    sequence: u64,
    manifest: &Manifest,
) -> Result<maintenance::Verification, ApiError> {
    if follower.applied_sequence() != sequence {
        return Err(corrupt("the replay did not reach the checkpoint"));
    }
    follower
        .promote()
        .close()
        .map_err(|e| corrupt(format!("the replayed database cannot be closed: {e}")))?;
    let verification = maintenance::verify(database)
        .map_err(|e| corrupt(format!("the replayed database failed verification: {e}")))?;
    let detail = maintenance::inspect_detail(database)
        .map_err(|e| corrupt(format!("the replayed database failed inspection: {e}")))?;
    if verification.inspection.commits != sequence
        || verification.inspection.incomplete_tail_bytes != 0
        || detail.lineage.map(lineage_hex) != manifest.source_lineage
    {
        return Err(corrupt("the replayed database differs from its checkpoint"));
    }
    Ok(verification)
}

#[cfg(test)]
mod tests {
    use super::super::fake::{self, Fault, Memory, Op};
    use super::*;
    use crate::Database;

    const PREFIX: &str = "ha/";
    const AUTHORITY_KEY: &str = "ha/authority.json";
    const INSTALL: &str = "installation";

    struct Env {
        meta: Arc<Memory>,
        data: Arc<Memory>,
        sets: Sets,
        authority: Arc<AuthorityStore>,
        claim: Claim,
        observed: Observed,
        archive: HaArchive,
        segment: LogSegment,
        dir: tempfile::TempDir,
    }

    fn bundle_bytes(manifest: &Manifest, chain: &str) -> Vec<u8> {
        let config = serde_json::from_value(serde_json::json!({
            "tenants": [], "accounts": [], "credentials": [],
            "webhook_subscriptions": 0, "secret_references": [],
            "excluded": recovery_bundle::EXCLUDED,
        }))
        .unwrap();
        let bundle = recovery_bundle::bundle(
            config,
            &manifest.installation_id,
            manifest
                .source_lineage
                .as_ref()
                .map(|s| u64::from_str_radix(s, 16).unwrap()),
            chain,
            manifest.policy.clone(),
            None,
            2,
        );
        serde_json::to_vec(&bundle).unwrap()
    }

    fn setup() -> Env {
        let meta = Arc::new(Memory::default());
        let data = Arc::new(Memory::default());
        let sets = Sets::new(meta.clone(), data.clone(), false, PREFIX.into());
        let authority = Arc::new(AuthorityStore::new(meta.clone(), PREFIX.into()));
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.vlite");
        let mut database = Database::create(&source, 3).unwrap();
        database.put(1, &[1.0, 0.0, 0.0], b"one").unwrap();
        database.close().unwrap();
        let verification = maintenance::verify(&source).unwrap();
        let detail = maintenance::inspect_detail(&source).unwrap();
        let database_upload = Upload::file(ROLE_DATABASE, source.clone()).unwrap();
        let id = new_set_id(1_700_000_000_000).unwrap();
        let mut manifest = Manifest::new(
            &id,
            INSTALL,
            1,
            &verification,
            &detail,
            &[],
            fake::policy_capture(),
        );
        let baseline_bundle = bundle_bytes(&manifest, &database_upload.sha256);
        let uploads = [database_upload, Upload::bytes(ROLE_BUNDLE, baseline_bundle)];
        manifest.objects = uploads
            .iter()
            .map(|u| ObjectEntry {
                role: u.role.into(),
                sha256: u.sha256.clone(),
                bytes: u.bytes,
            })
            .collect();
        let created = authority
            .create(INSTALL, "scope", manifest.source_lineage.as_ref().unwrap())
            .unwrap();
        let (claim, _) = authority.claim(&created, 1).unwrap();
        let manifest = HaManifest {
            schema: HA_MANIFEST_SCHEMA.into(),
            authority: HaIdentity {
                writer_id: claim.writer_id.clone(),
                epoch: claim.epoch,
                scope_id: created.record.scope_id,
                lineage: manifest.source_lineage.clone().unwrap(),
            },
            manifest,
        };
        let observed = sets
            .publish_ha(&authority, &claim, &manifest, &uploads, 1, &fake::open)
            .unwrap();
        let archive = sets.open_archive_ha(&authority, &claim).unwrap();
        let mut database = Database::open(&source).unwrap();
        database.put(2, &[0.0, 1.0, 0.0], b"two").unwrap();
        let segment = database
            .export_log(archive.next_base(), MAX_SEGMENT_BYTES)
            .unwrap();
        database.close().unwrap();
        Env {
            meta,
            data,
            sets,
            authority,
            claim,
            observed,
            archive,
            segment,
            dir,
        }
    }

    #[allow(clippy::result_large_err)]
    fn append(env: &Env) -> Result<HaCheckpoint, HaArchiveError> {
        env.sets.append_checkpoint_ha(
            &env.archive,
            &env.segment,
            &|chain| Ok(bundle_bytes(&env.archive.manifest.manifest, chain)),
            2,
            &fake::open,
            &env.authority,
            &env.claim,
        )
    }

    #[test]
    fn ha_published_baseline_appends_and_reopens() {
        let env = setup();
        let baseline = env.observed.record.head.as_ref().unwrap();
        assert_eq!(&env.archive.head, baseline);
        assert_eq!(
            baseline.manifest_sha256,
            sha_hex(&encode(&env.archive.manifest).unwrap())
        );
        assert!(baseline.index_sha256.is_none());
        let first = append(&env).unwrap();
        let sets = Sets::new(env.meta.clone(), env.data.clone(), false, PREFIX.into());
        let archive = sets.open_archive_ha(&env.authority, &env.claim).unwrap();
        assert_eq!(archive.checkpoints(), std::slice::from_ref(&first));
        assert_eq!(archive.next_base(), env.segment.last_sequence);
        let index = archive.index.as_ref().unwrap();
        assert_eq!(&index.prior_head, baseline);
        assert_eq!(index.prior_head_sha256, ha_head_digest(baseline).unwrap());
        assert_eq!(
            archive.head.index_sha256.as_ref().unwrap(),
            &sha_hex(&encode(index).unwrap())
        );
        let mut database = Database::open(env.dir.path().join("source.vlite")).unwrap();
        database.put(3, &[0.0, 0.0, 1.0], b"three").unwrap();
        let segment = database
            .export_log(archive.next_base(), MAX_SEGMENT_BYTES)
            .unwrap();
        database.close().unwrap();
        let second = sets
            .append_checkpoint_ha(
                &archive,
                &segment,
                &|chain| Ok(bundle_bytes(&archive.manifest.manifest, chain)),
                3,
                &fake::open,
                &env.authority,
                &env.claim,
            )
            .unwrap();
        let reopened = sets.open_archive_ha(&env.authority, &env.claim).unwrap();
        assert_eq!(reopened.checkpoints(), &[first, second]);
        assert_eq!(reopened.manifest, env.archive.manifest);
        let index = reopened.index.as_ref().unwrap();
        assert_eq!(index.authority, reopened.manifest.authority);
        assert_eq!(index.prior_head, archive.head);
        assert_eq!(
            index.prior_head_sha256,
            ha_head_digest(&archive.head).unwrap()
        );
        let (resolved, record, manifest) =
            sets.resolve_ha_baseline(&env.authority).unwrap().unwrap();
        assert_eq!(resolved.record.head.as_ref(), Some(&reopened.head));
        assert_eq!(record.set_id, env.archive.set_id);
        assert_eq!(manifest, env.archive.manifest);
        assert!(env.data.raw(&sets.archive_key(&record.set_id)).is_none());
        assert!(sets.open_archive(&record.set_id).is_err());
    }

    #[test]
    fn ha_off_wire_and_archive_unchanged() {
        let checkpoint = Checkpoint {
            sequence: 12,
            base_sequence: 10,
            segment_sha256: "segment".into(),
            segment_bytes: 123,
            bundle_sha256: "bundle".into(),
            bundle_bytes: 456,
            chain_sha256: "chain".into(),
            captured_unix_ms: 789,
        };
        assert_eq!(
            serde_json::to_vec(&checkpoint).unwrap(),
            br#"{"sequence":12,"base_sequence":10,"segment_sha256":"segment","segment_bytes":123,"bundle_sha256":"bundle","bundle_bytes":456,"chain_sha256":"chain","captured_unix_ms":789}"#
        );
        let index = Index {
            schema: ARCHIVE_SCHEMA.into(),
            set_id: "set".into(),
            installation_id: "install".into(),
            source_lineage: "lineage".into(),
            header_sha256: "header".into(),
            baseline_sequence: 10,
            baseline_sha256: "database".into(),
            checkpoints: vec![checkpoint],
        };
        assert_eq!(
            serde_json::to_vec(&index).unwrap(),
            br#"{"schema":"vlite.cloud-backup-archive/1","set_id":"set","installation_id":"install","source_lineage":"lineage","header_sha256":"header","baseline_sequence":10,"baseline_sha256":"database","checkpoints":[{"sequence":12,"base_sequence":10,"segment_sha256":"segment","segment_bytes":123,"bundle_sha256":"bundle","bundle_bytes":456,"chain_sha256":"chain","captured_unix_ms":789}]}"#
        );
    }

    #[test]
    fn ha_index_refused_by_legacy_parser() {
        let env = setup();
        let checkpoint = append(&env).unwrap();
        let archive = env
            .sets
            .open_archive_ha(&env.authority, &env.claim)
            .unwrap();
        let index = archive.index.as_ref().unwrap();
        let bytes = encode(index).unwrap();
        assert!(decode::<Index>("index", &bytes).is_err());
        assert!(decode::<Checkpoint>("checkpoint", &encode(&checkpoint).unwrap()).is_err());
        assert_eq!(parse_ha_index(&bytes).unwrap(), *index);
        let mut value = serde_json::to_value(index).unwrap();
        value["extra"] = serde_json::json!(true);
        assert!(parse_ha_index(&serde_json::to_vec(&value).unwrap()).is_err());
        let mut value = serde_json::to_value(index).unwrap();
        value["checkpoints"][0]["extra"] = serde_json::json!(true);
        assert!(parse_ha_index(&serde_json::to_vec(&value).unwrap()).is_err());
        // Even a misleading legacy pointer cannot make the legacy opener
        // accept either the HA manifest or the HA index.
        env.data
            .set_raw(&env.sets.archive_key(&archive.set_id), &bytes);
        assert!(env.sets.open_archive(&archive.set_id).is_err());
    }

    #[test]
    fn ha_chain_is_domain_separated_from_legacy() {
        let identity = HaIdentity {
            writer_id: "a".repeat(32),
            epoch: 1,
            scope_id: "scope".into(),
            lineage: "0123456789abcdef".into(),
        };
        let previous = "b".repeat(64);
        let segment = "c".repeat(64);
        let tip = ha_chain(&previous, &identity, 10, 12, &segment);
        assert_ne!(tip, chain(&previous, &segment, 12));
        let mut changed = identity.clone();
        changed.epoch += 1;
        assert_ne!(tip, ha_chain(&previous, &changed, 10, 12, &segment));
        changed = identity.clone();
        changed.writer_id = "d".repeat(32);
        assert_ne!(tip, ha_chain(&previous, &changed, 10, 12, &segment));
        assert_ne!(tip, ha_chain(&"d".repeat(64), &identity, 10, 12, &segment));
        assert_ne!(tip, ha_chain(&previous, &identity, 11, 12, &segment));
        assert_ne!(tip, ha_chain(&previous, &identity, 10, 13, &segment));
        assert_ne!(tip, ha_chain(&previous, &identity, 10, 12, &"d".repeat(64)));
    }

    #[test]
    fn head_epoch_bump_invalidates_old_etag() {
        let env = setup();
        let authority = env.authority.clone();
        let current = env.observed.clone();
        env.meta.hook(Op::Put, AUTHORITY_KEY, move || {
            let prepared = authority
                .prepare_transfer(&current, current.record.epoch, &current.record.revision)
                .unwrap();
            authority.claim(&prepared, 3).unwrap();
        });
        assert!(matches!(
            append(&env),
            Err(HaArchiveError::Authority(AuthorityError::Fenced {
                observed_epoch: 2
            }))
        ));
        let current = env.authority.read().unwrap();
        assert_eq!(current.record.phase, Phase::Recovering);
        assert_eq!(current.record.base.as_ref(), Some(&env.archive.head));
        assert!(current.record.head.is_none());
        assert!(env.archive.checkpoints().is_empty());
        assert!(
            env.data
                .raw(&env.sets.archive_key(&env.archive.set_id))
                .is_none()
        );
    }

    #[test]
    fn ha_epoch_bump_during_bundle_preparation_does_not_publish() {
        let env = setup();
        let successor = std::cell::RefCell::new(None);
        let result = env.sets.append_checkpoint_ha(
            &env.archive,
            &env.segment,
            &|chain| {
                let prepared = env
                    .authority
                    .prepare_transfer(
                        &env.observed,
                        env.claim.epoch,
                        &env.observed.record.revision,
                    )
                    .unwrap();
                let (_, observed) = env.authority.claim(&prepared, 3).unwrap();
                successor.replace(Some(observed));
                Ok(bundle_bytes(&env.archive.manifest.manifest, chain))
            },
            2,
            &fake::open,
            &env.authority,
            &env.claim,
        );
        assert!(matches!(
            result,
            Err(HaArchiveError::Authority(AuthorityError::Fenced {
                observed_epoch: 2
            }))
        ));
        let current = env.authority.read().unwrap();
        assert_eq!(Some(&current), successor.borrow().as_ref());
        assert!(current.record.head.is_none());
        assert_eq!(current.record.base.as_ref(), Some(&env.archive.head));
        assert!(env.archive.checkpoints().is_empty());
        assert!(
            env.sets
                .resolve_ha_baseline(&env.authority)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn head_unknown_commit_requires_exact_reference() {
        let env = setup();
        env.meta.fault(Op::Put, AUTHORITY_KEY, Fault::After, 0, 1);
        let checkpoint = append(&env).unwrap();
        let current = env
            .sets
            .open_archive_ha(&env.authority, &env.claim)
            .unwrap();
        assert_eq!(current.checkpoints(), &[checkpoint]);
        assert!(
            env.data
                .raw(&env.sets.archive_key(&env.archive.set_id))
                .is_none()
        );

        let env = setup();
        env.meta.fault(Op::Put, AUTHORITY_KEY, Fault::Before, 0, 1);
        assert!(matches!(
            append(&env),
            Err(HaArchiveError::Authority(AuthorityError::HeadMoved(_)))
        ));
        assert_eq!(env.authority.read().unwrap(), env.observed);
        assert!(env.archive.checkpoints().is_empty());

        let env = setup();
        // Change just the immutable reference on ambiguous readback, retaining
        // the same writer, epoch and sequence. It must not count as success.
        env.meta.fault(Op::Put, AUTHORITY_KEY, Fault::After, 0, 1);
        let meta = env.meta.clone();
        env.data.hook(Op::Put, "/objects/", move || {
            let target = meta.clone();
            meta.hook(Op::Get, AUTHORITY_KEY, move || {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&target.raw(AUTHORITY_KEY).unwrap()).unwrap();
                value["head"]["index_sha256"] = serde_json::json!("e".repeat(64));
                target.set_raw(AUTHORITY_KEY, &serde_json::to_vec(&value).unwrap());
            });
        });
        assert!(matches!(
            append(&env),
            Err(HaArchiveError::Authority(AuthorityError::HeadMoved(_)))
        ));
        assert!(env.archive.checkpoints().is_empty());

        let env = setup();
        env.meta.fault(Op::Put, AUTHORITY_KEY, Fault::After, 0, 1);
        env.meta.fault(Op::Get, AUTHORITY_KEY, Fault::Before, 1, 1);
        assert!(matches!(
            append(&env),
            Err(HaArchiveError::Authority(AuthorityError::Unresolved))
        ));
        assert!(env.archive.checkpoints().is_empty());
    }

    #[test]
    fn head_moved_same_writer_requires_rebase() {
        let env = setup();
        let rival = Sets::new(env.meta.clone(), env.data.clone(), false, PREFIX.into());
        let authority = env.authority.clone();
        let claim = env.claim.clone();
        let segment = env.segment.to_bytes();
        env.meta.hook(Op::Put, AUTHORITY_KEY, move || {
            let archive = rival.open_archive_ha(&authority, &claim).unwrap();
            rival
                .append_checkpoint_ha(
                    &archive,
                    &LogSegment::from_bytes(&segment).unwrap(),
                    &|chain| Ok(bundle_bytes(&archive.manifest.manifest, chain)),
                    99,
                    &fake::open,
                    &authority,
                    &claim,
                )
                .unwrap();
        });
        let Err(HaArchiveError::Authority(AuthorityError::HeadMoved(observed))) = append(&env)
        else {
            panic!("a same-writer head CAS loser must explicitly rebase");
        };
        assert_eq!(observed, env.authority.read().unwrap());
        assert_eq!(
            observed.record.writer_id.as_ref(),
            Some(&env.claim.writer_id)
        );
        let current = env
            .sets
            .open_archive_ha(&env.authority, &env.claim)
            .unwrap();
        assert_eq!(current.checkpoints().len(), 1);
        assert_eq!(current.checkpoints()[0].checkpoint.captured_unix_ms, 99);
        assert!(env.archive.checkpoints().is_empty());
        let names = env.data.names();
        let error = append(&env).unwrap_err();
        assert!(matches!(
            error,
            HaArchiveError::Authority(AuthorityError::HeadMoved(_))
        ));
        let api = ApiError::from(error);
        assert_eq!(api.status, StatusCode::CONFLICT);
        assert_eq!(api.code, "writer_rebase_required");
        assert_eq!(env.data.names(), names);
    }

    #[test]
    fn index_check_rejects_epoch_regression_and_writer_mismatch() {
        let env = setup();
        append(&env).unwrap();
        let archive = env
            .sets
            .open_archive_ha(&env.authority, &env.claim)
            .unwrap();
        let mut database = Database::open(env.dir.path().join("source.vlite")).unwrap();
        database.put(3, &[0.0, 0.0, 1.0], b"three").unwrap();
        let segment = database
            .export_log(archive.next_base(), MAX_SEGMENT_BYTES)
            .unwrap();
        database.close().unwrap();
        env.sets
            .append_checkpoint_ha(
                &archive,
                &segment,
                &|chain| Ok(bundle_bytes(&archive.manifest.manifest, chain)),
                3,
                &fake::open,
                &env.authority,
                &env.claim,
            )
            .unwrap();
        let archive = env
            .sets
            .open_archive_ha(&env.authority, &env.claim)
            .unwrap();
        let index = archive.index.as_ref().unwrap();
        let check =
            |index: &HaIndex| index.check(&archive.set_id, &archive.manifest, &archive.head);
        check(index).unwrap();
        let mut bad = index.clone();
        bad.checkpoints[1].authority.epoch = 0;
        assert!(check(&bad).is_err());
        let mut bad = index.clone();
        bad.checkpoints[1].authority.writer_id = "f".repeat(32);
        assert!(check(&bad).is_err());
        let mut bad = index.clone();
        bad.authority.writer_id = "f".repeat(32);
        assert!(check(&bad).is_err());
        let mut bad = index.clone();
        bad.prior_head_sha256 = "f".repeat(64);
        assert!(check(&bad).is_err());
        let mut bad = index.clone();
        bad.prior_head.index_sha256 = Some("f".repeat(64));
        assert!(check(&bad).is_err());
        for field in [
            "epoch",
            "set_id",
            "manifest_sha256",
            "sequence",
            "chain_sha256",
        ] {
            let mut value = serde_json::to_value(index).unwrap();
            value["prior_head"][field] = match field {
                "epoch" | "sequence" => serde_json::json!(999),
                "set_id" => serde_json::json!("another-set"),
                _ => serde_json::json!("f".repeat(64)),
            };
            let mut bad: HaIndex = serde_json::from_value(value).unwrap();
            bad.prior_head_sha256 = ha_head_digest(&bad.prior_head).unwrap();
            assert!(check(&bad).is_err(), "{field}");
        }
        let mut bad = index.clone();
        bad.checkpoints[1].checkpoint.chain_sha256 = chain(
            &bad.checkpoints[0].checkpoint.chain_sha256,
            &bad.checkpoints[1].checkpoint.segment_sha256,
            bad.checkpoints[1].checkpoint.sequence,
        );
        assert!(check(&bad).is_err());
        for (field, value) in [
            ("scope_id", serde_json::json!("another-scope")),
            ("lineage", serde_json::json!("fedcba9876543210")),
        ] {
            let mut value_index = serde_json::to_value(index).unwrap();
            value_index["checkpoints"][1]["authority"][field] = value;
            let bad: HaIndex = serde_json::from_value(value_index).unwrap();
            assert!(check(&bad).is_err());
        }
        let mut bad = index.clone();
        bad.checkpoints[0].checkpoint.segment_bytes = MAX_SEGMENT_BYTES as u64 + 1;
        assert!(check(&bad).is_err());
        let mut bad = index.clone();
        bad.checkpoints[0].checkpoint.bundle_bytes = recovery_bundle::MAX_BYTES + 1;
        assert!(check(&bad).is_err());
    }

    #[test]
    fn ha_checkpoint_bundle_still_checked() {
        let env = setup();
        let original = env.data.names();
        for field in [
            "installation_id",
            "source_lineage",
            "database_sha256",
            "policy",
            "memory",
        ] {
            let bundle = |chain: &str| {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&bundle_bytes(&env.archive.manifest.manifest, chain))
                        .unwrap();
                match field {
                    "installation_id" => value[field] = serde_json::json!("other"),
                    "source_lineage" => value[field] = serde_json::json!("fedcba9876543210"),
                    "database_sha256" => value[field] = serde_json::json!("f".repeat(64)),
                    "policy" => value[field]["revision_unix_ms"] = serde_json::json!(77),
                    "memory" => {
                        value["schema"] = serde_json::json!(recovery_bundle::SCHEMA_V2);
                        value["memory_bindings"] = serde_json::json!({});
                    }
                    _ => unreachable!(),
                }
                Ok(serde_json::to_vec(&value).unwrap())
            };
            assert!(
                env.sets
                    .append_checkpoint_ha(
                        &env.archive,
                        &env.segment,
                        &bundle,
                        2,
                        &fake::open,
                        &env.authority,
                        &env.claim,
                    )
                    .is_err(),
                "{field}"
            );
        }
        assert!(
            env.sets
                .append_checkpoint_ha(
                    &env.archive,
                    &env.segment,
                    &|_| Ok(vec![b' '; recovery_bundle::MAX_BYTES as usize + 1]),
                    2,
                    &fake::open,
                    &env.authority,
                    &env.claim,
                )
                .is_err()
        );
        assert_eq!(env.data.names(), original);
        assert_eq!(env.authority.read().unwrap(), env.observed);
        append(&env).unwrap();
    }

    #[test]
    fn ha_open_uses_exact_authority_index_and_checks_hash() {
        let env = setup();
        append(&env).unwrap();
        let current = env
            .sets
            .open_archive_ha(&env.authority, &env.claim)
            .unwrap();
        env.data
            .set_raw(&env.sets.archive_key(&current.set_id), b"not an index");
        let reopened = env
            .sets
            .open_archive_ha(&env.authority, &env.claim)
            .unwrap();
        assert_eq!(reopened.head, current.head);
        assert_eq!(reopened.checkpoints(), current.checkpoints());
        let key = env
            .sets
            .object_key(&current.set_id, current.head.index_sha256.as_ref().unwrap());
        env.data.set_raw(&key, b"{}");
        assert!(
            env.sets
                .open_archive_ha(&env.authority, &env.claim)
                .is_err()
        );
        assert!(env.sets.resolve_ha_baseline(&env.authority).is_err());
    }

    #[test]
    fn ha_append_requires_serving_and_authorization() {
        let env = setup();
        let names = env.data.names();
        let denied = || Err(conflict("test_denied", "publication stopped"));
        assert!(
            env.sets
                .append_checkpoint_ha(
                    &env.archive,
                    &env.segment,
                    &|chain| Ok(bundle_bytes(&env.archive.manifest.manifest, chain)),
                    2,
                    &denied,
                    &env.authority,
                    &env.claim,
                )
                .is_err()
        );
        assert_eq!(env.data.names(), names);
        assert_eq!(env.authority.read().unwrap(), env.observed);
        let mut current = env.observed.record.clone();
        current.phase = Phase::Recovering;
        current.head = None;
        env.meta.set_raw(AUTHORITY_KEY, &encode(&current).unwrap());
        assert!(matches!(
            append(&env),
            Err(HaArchiveError::Authority(AuthorityError::Invalid))
        ));
        assert_eq!(env.data.names(), names);
    }

    #[test]
    fn ha_uploads_require_readback_and_final_authorization() {
        for skip in 0..3 {
            let env = setup();
            env.data.fault(Op::Get, "/objects/", Fault::Refuse, skip, 1);
            assert!(append(&env).is_err());
            assert_eq!(env.authority.read().unwrap(), env.observed);
            assert!(env.archive.checkpoints().is_empty());
        }
        let env = setup();
        let bytes = env.segment.to_bytes();
        let key = env.sets.object_key(&env.archive.set_id, &sha_hex(&bytes));
        env.data.set_raw(&key, &vec![b'?'; bytes.len()]);
        assert!(matches!(
            append(&env),
            Err(HaArchiveError::Api(error)) if error.code == "cloud_object_conflict"
        ));
        assert_eq!(env.authority.read().unwrap(), env.observed);

        let env = setup();
        let calls = std::cell::Cell::new(0);
        let authorize = || {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                Ok(())
            } else {
                Err(conflict("test_denied", "publication stopped"))
            }
        };
        assert!(
            env.sets
                .append_checkpoint_ha(
                    &env.archive,
                    &env.segment,
                    &|chain| Ok(bundle_bytes(&env.archive.manifest.manifest, chain)),
                    2,
                    &authorize,
                    &env.authority,
                    &env.claim,
                )
                .is_err()
        );
        assert_eq!(calls.get(), 2);
        assert_eq!(env.authority.read().unwrap(), env.observed);
        assert!(
            env.data
                .raw(&env.sets.archive_key(&env.archive.set_id))
                .is_none()
        );
    }
}
