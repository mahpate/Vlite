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
//! then replaces `archive.json` by ETag compare-and-swap. The index is the
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
