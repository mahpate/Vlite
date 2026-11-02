//! Change feed: committed changes after a durable cursor.
//!
//! The feed is derived from the database log itself, so it is exactly the set
//! of durable commits. Cursors are `(lineage, sequence)` pairs: a compaction,
//! restore, upgrade or re-encryption starts a new lineage, after which older
//! cursors fail with `Error::CursorCompacted` rather than silently skipping
//! history. Feeds need a lineage (format v4 or newer). Graph-enabled files
//! report graph commits as `GraphCreated`, `GraphDropped` and `GraphBatch`
//! events next to vector events (v0.6.0).

use crate::crypto::EncryptionKey;
use crate::format::{self, LogHeader, OP_TIME_ANCHOR};
use crate::{CollectionSettings, Error, Metadata, Record, Result};
use std::collections::BTreeMap;
use std::io::{Read, Seek};
use std::path::Path;

/// Re-exported from [`crate::property_graph`], where it is defined.
pub use crate::property_graph::{GraphChange, GraphRemovalReason, GraphTtl, TemporalChange};

// Gap 3: the TTL feed reader that numbers commits lands next.
#[cfg_attr(not(test), allow(dead_code))]
mod ttl_chunk;

/// Default and maximum events returned by one batch.
pub const DEFAULT_FEED_BATCH: usize = 1000;
pub const MAX_FEED_BATCH: usize = 10_000;

/// A position in the feed: everything up to and including `sequence` of the
/// given `lineage` has been consumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FeedCursor {
    /// Zero means "the beginning of whatever lineage the database has now";
    /// it is only valid together with `sequence == 0`.
    pub lineage: u64,
    pub sequence: u64,
}

impl FeedCursor {
    /// Reads every committed change in the current history.
    pub const fn beginning() -> Self {
        Self {
            lineage: 0,
            sequence: 0,
        }
    }

    /// Opaque, checksummed text form for storing a cursor elsewhere.
    pub fn to_token(&self) -> String {
        let body = format!("vlc1.{:016x}.{:016x}", self.lineage, self.sequence);
        format!("{body}.{:08x}", format::crc32(body.as_bytes()))
    }

    pub fn from_token(token: &str) -> Result<Self> {
        let invalid = Error::InvalidCursor("malformed cursor token");
        let (body, crc) = token
            .rsplit_once('.')
            .ok_or(Error::InvalidCursor("malformed cursor token"))?;
        let crc = u32::from_str_radix(crc, 16)
            .map_err(|_| Error::InvalidCursor("malformed cursor token"))?;
        if format::crc32(body.as_bytes()) != crc {
            return Err(Error::InvalidCursor("cursor token checksum mismatch"));
        }
        let mut parts = body.split('.');
        if parts.next() != Some("vlc1") {
            return Err(invalid);
        }
        let lineage = parts
            .next()
            .and_then(|value| u64::from_str_radix(value, 16).ok())
            .ok_or(Error::InvalidCursor("malformed cursor token"))?;
        let sequence = parts
            .next()
            .and_then(|value| u64::from_str_radix(value, 16).ok())
            .ok_or(Error::InvalidCursor("malformed cursor token"))?;
        if parts.next().is_some() || (lineage == 0 && sequence != 0) {
            return Err(Error::InvalidCursor("malformed cursor token"));
        }
        Ok(Self { lineage, sequence })
    }

    /// Durably stores the cursor (write to a sibling file, fsync, rename).
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        use std::io::Write;
        let path = path.as_ref();
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(".tmp");
        let temporary = std::path::PathBuf::from(temporary);
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(self.to_token().as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            // Directory fsync is best effort; it is unsupported on some platforms.
            if let Ok(directory) = std::fs::File::open(parent) {
                let _ = directory.sync_all();
            }
        }
        Ok(())
    }

    /// Loads a saved cursor; `Ok(None)` when none has been saved yet.
    pub fn load(path: impl AsRef<Path>) -> Result<Option<Self>> {
        match std::fs::read_to_string(path.as_ref()) {
            Ok(text) => Self::from_token(text.trim()).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

/// What happened in one committed operation.
#[derive(Clone, Debug, PartialEq)]
pub enum ChangeKind {
    /// A record was inserted or replaced. `expires_at_ms` is set for records
    /// written with a time to live; expiry itself is not logged, consumers
    /// derive it from this timestamp.
    Put {
        id: u64,
        record: Record,
        expires_at_ms: Option<u64>,
    },
    /// A delete commit. Deleting a missing ID is still reported.
    Delete {
        id: u64,
    },
    /// Metadata of an existing record was replaced.
    MetadataUpdate {
        id: u64,
        metadata: Metadata,
    },
    CollectionCreated {
        dimensions: usize,
        settings: CollectionSettings,
    },
    CollectionDropped,
    SettingsUpdated {
        settings: CollectionSettings,
    },
    /// Named/sparse vector data of the collection was replaced.
    VectorsUpdated,
    /// A graph was created (graph-enabled files). Graph events have an empty
    /// `Change::collection`; the graph name is here.
    GraphCreated {
        graph: String,
    },
    /// A graph was dropped.
    GraphDropped {
        graph: String,
    },
    /// The node, edge, attachment and schema changes of one graph commit, in
    /// log order. `revision` is the commit sequence, which is also the new
    /// revision of every node and edge written here. A commit that mixes
    /// graph and vector operations yields several events sharing `sequence`:
    /// vector operations are `Put`/`Delete` events at their position and the
    /// graph operations between them form one `GraphBatch` each.
    GraphBatch {
        graph: String,
        revision: u64,
        operations: Vec<GraphChange>,
    },
}

/// One event. A batch commit produces several events sharing `sequence`,
/// numbered by `ordinal`.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub sequence: u64,
    pub ordinal: u32,
    pub collection: String,
    /// Present for files created with commit timestamps.
    pub committed_at_ms: Option<u64>,
    pub kind: ChangeKind,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChangeBatch {
    pub changes: Vec<Change>,
    /// Pass this to the next call; equal to the input when nothing was new.
    pub next: FeedCursor,
    /// More committed changes are available right now.
    pub has_more: bool,
    /// Last committed sequence at read time.
    pub head: u64,
}

/// Gap 3 (unreleased): the exact set of optional graph feed events a reader
/// selects. A cursor is bound to one set; a different set is never accepted
/// implicitly (convert with [`GraphFeedCursor::at_commit`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GraphEventSet {
    /// The legacy feed: no TTL events or annotations; v1 cursor tokens.
    Empty,
    /// Graph TTL events: logged deadline changes and expiry reclaims, with a
    /// [`GraphTtl`] annotation per node or edge operation; v2 cursor tokens.
    Ttl,
}

impl GraphEventSet {
    /// The set's spelling inside a v2 token, `None` for the v1 `Empty` set.
    const fn token_name(self) -> Option<&'static str> {
        match self {
            Self::Empty => None,
            Self::Ttl => Some("ttl"),
        }
    }
}

/// Gap 3 (unreleased): a graph feed position bound to a [`GraphEventSet`].
///
/// Besides a commit boundary, a TTL cursor can stand inside one commit, after
/// event `ordinal` of `position().sequence`, because a large expiry reclaim
/// is delivered in several chunks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GraphFeedCursor {
    position: FeedCursor,
    ordinal: Option<u32>,
    graph_events: GraphEventSet,
}

/// Marks a commit boundary inside a v2 token.
const BOUNDARY_ORDINAL: u32 = u32::MAX;

impl GraphFeedCursor {
    /// Reads every committed change in the current history with
    /// `graph_events`.
    pub const fn beginning(graph_events: GraphEventSet) -> Self {
        Self::at_commit(FeedCursor::beginning(), graph_events)
    }

    /// The boundary after commit `position`, read with `graph_events`. This
    /// is the explicit way to start a set from a legacy cursor or to switch
    /// a boundary cursor to another set.
    pub const fn at_commit(position: FeedCursor, graph_events: GraphEventSet) -> Self {
        Self {
            position,
            ordinal: None,
            graph_events,
        }
    }

    /// The commit this cursor is in or after. Inside a commit (`ordinal()`
    /// is `Some`) this drops the remaining events' position: do not use it
    /// as a legacy resume cursor, or the commit's unread events are skipped.
    pub const fn position(&self) -> FeedCursor {
        self.position
    }

    /// `None` when all of `position().sequence` has been consumed;
    /// `Some(n)` when its events through ordinal `n` have been consumed.
    pub const fn ordinal(&self) -> Option<u32> {
        self.ordinal
    }

    /// The event set this cursor is bound to.
    pub const fn graph_events(&self) -> GraphEventSet {
        self.graph_events
    }

    /// Parses a token from [`to_token`](Self::to_token): a `vlc1` token is
    /// an `Empty` commit boundary (parsed as [`FeedCursor::from_token`]), a
    /// `vlc2` token carries its set and ordinal. A malformed, altered or
    /// impossible token is `Error::InvalidCursor`.
    pub fn from_token(token: &str) -> Result<Self> {
        if !token.starts_with("vlc2.") {
            let position = FeedCursor::from_token(token)?;
            return Ok(Self::at_commit(position, GraphEventSet::Empty));
        }
        let malformed = || Error::InvalidCursor("malformed cursor token");
        let (body, crc) = token.rsplit_once('.').ok_or_else(malformed)?;
        let parts: Vec<&str> = body.split('.').collect();
        let [_, lineage, sequence, ordinal, set] = parts[..] else {
            return Err(malformed());
        };
        let lineage = lower_hex(lineage, 16).ok_or_else(malformed)?;
        let sequence = lower_hex(sequence, 16).ok_or_else(malformed)?;
        let ordinal = lower_hex(ordinal, 8).ok_or_else(malformed)?;
        let crc = lower_hex(crc, 8).ok_or_else(malformed)?;
        if u64::from(format::crc32(body.as_bytes())) != crc {
            return Err(Error::InvalidCursor("cursor token checksum mismatch"));
        }
        let graph_events = match set {
            "ttl" => GraphEventSet::Ttl,
            _ => return Err(Error::InvalidCursor("unsupported cursor event set")),
        };
        let ordinal = u32::try_from(ordinal).map_err(|_| malformed())?;
        let ordinal = (ordinal != BOUNDARY_ORDINAL).then_some(ordinal);
        if (lineage == 0 && (sequence != 0 || ordinal.is_some()))
            || (ordinal.is_some() && sequence == 0)
        {
            return Err(Error::InvalidCursor("impossible cursor position"));
        }
        Ok(Self {
            position: FeedCursor { lineage, sequence },
            ordinal,
            graph_events,
        })
    }

    /// Opaque, checksummed text form. An `Empty` cursor is exactly its
    /// position's v1 token; other sets use
    /// `vlc2.<lineage>.<sequence>.<ordinal>.<set>.<crc>`.
    pub fn to_token(&self) -> String {
        let Some(set) = self.graph_events.token_name() else {
            return self.position.to_token();
        };
        let body = format!(
            "vlc2.{:016x}.{:016x}.{:08x}.{set}",
            self.position.lineage,
            self.position.sequence,
            self.ordinal.unwrap_or(BOUNDARY_ORDINAL)
        );
        format!("{body}.{:08x}", format::crc32(body.as_bytes()))
    }
}

/// Exactly `width` lower-case hex digits.
fn lower_hex(text: &str, width: usize) -> Option<u64> {
    if text.len() != width
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    u64::from_str_radix(text, 16).ok()
}

/// Gap 3 (unreleased): one event of a graph feed with its annotations.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct GraphFeedChange {
    /// The event, as the legacy feed reports it (TTL deadline changes and
    /// reclaim removals appear as ordinary graph patches and deletes).
    pub change: Change,
    /// For a [`GraphEventSet::Ttl`] graph batch, one entry per operation of
    /// `change` at the same index: `Some` for node and edge operations,
    /// `None` for attachments and schema. Empty for every other event and
    /// for the `Empty` set.
    pub ttl: Vec<Option<GraphTtl>>,
}

/// Gap 3 (unreleased): a page of graph feed events.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct GraphFeedBatch {
    pub changes: Vec<GraphFeedChange>,
    /// Pass this to the next call; it may stand inside a commit.
    pub next: GraphFeedCursor,
    /// Another event exists after `next` in the scanned log.
    pub has_more: bool,
    /// Last committed sequence at read time.
    pub head: u64,
}

/// Gap 3 (unreleased): why a graph feed read failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum GraphFeedError {
    /// The read failed as the legacy feed does (lineage, ahead, corruption,
    /// unsupported or invalid cursor).
    Engine(Error),
    /// The cursor is bound to a different event set than requested.
    CursorModeMismatch {
        cursor: GraphEventSet,
        requested: GraphEventSet,
    },
    /// The cursor is before the history floor, or inside the commit at the
    /// floor, which a checkpoint no longer restates event by event.
    CursorBeforeHistory { cursor: u64, floor: u64 },
    /// One event's canonical size exceeds the per-event limit; nothing was
    /// delivered and the cursor does not advance.
    ResponseTooLarge { bytes: usize, limit: usize },
}

impl std::fmt::Display for GraphFeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Engine(error) => error.fmt(f),
            Self::CursorModeMismatch { cursor, requested } => write!(
                f,
                "cursor_mode_mismatch: cursor graph_events {cursor:?} differ from requested {requested:?}"
            ),
            Self::CursorBeforeHistory { cursor, floor } => write!(
                f,
                "cursor sequence {cursor} is before the history floor {floor}: a checkpoint restated earlier changes; take a snapshot and resume at {floor} or later"
            ),
            Self::ResponseTooLarge { bytes, limit } => write!(
                f,
                "response_too_large: canonical event of {bytes} bytes exceeds {limit} bytes"
            ),
        }
    }
}

impl std::error::Error for GraphFeedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Engine(error) => Some(error),
            _ => None,
        }
    }
}

impl From<Error> for GraphFeedError {
    fn from(error: Error) -> Self {
        Self::Engine(error)
    }
}

/// Reads a batch, with the derived blocks a compact image skipped (reported
/// by independent readers only; a handle's open already reported its own).
pub(crate) fn read_changes<R: Read + Seek>(
    file: &mut R,
    key: Option<&EncryptionKey>,
    known: Option<&crate::crypto::Cipher>,
    cursor: &FeedCursor,
    max_events: usize,
) -> Result<(ChangeBatch, crate::compact::Skips)> {
    read_changes_with(file, key, known, cursor, max_events, false)
}

/// [`read_changes`], reporting temporal graph entries if `temporal` (Gap 4)
/// and refusing them otherwise.
pub(crate) fn read_changes_with<R: Read + Seek>(
    file: &mut R,
    key: Option<&EncryptionKey>,
    known: Option<&crate::crypto::Cipher>,
    cursor: &FeedCursor,
    max_events: usize,
    temporal: bool,
) -> Result<(ChangeBatch, crate::compact::Skips)> {
    let header = format::read_log_header_with(file, key, known)?;
    let lineage = header.lineage.ok_or(Error::ChangeFeedRequiresUpgrade)?;
    if cursor.lineage != 0 && cursor.lineage != lineage {
        return Err(Error::CursorCompacted {
            cursor_lineage: cursor.lineage,
            current_lineage: lineage,
        });
    }
    if cursor.lineage == 0 && cursor.sequence != 0 {
        return Err(Error::InvalidCursor("sequence requires a lineage"));
    }
    scan(
        file,
        &header,
        lineage,
        cursor.sequence,
        max_events.clamp(1, MAX_FEED_BATCH),
        temporal,
    )
}

fn scan<R: Read + Seek>(
    file: &mut R,
    header: &LogHeader,
    lineage: u64,
    after: u64,
    max_events: usize,
    temporal: bool,
) -> Result<(ChangeBatch, crate::compact::Skips)> {
    let mut collections: BTreeMap<u16, (String, usize)> = BTreeMap::new();
    collections.insert(0, ("default".to_string(), header.dimensions));
    let mut offset = header.start;
    let mut head = 0u64;
    let mut changes = Vec::new();
    let mut next = after;
    let mut anchor_ms: Option<u64> = None;
    let mut has_more = false;
    let mut graphs = crate::property_graph::feed::FeedGraphs::new(header.flags, temporal);
    let mut check = crate::compact::ImageCheck::new(
        header.version,
        header.flags,
        header.dimensions,
        header.collection_floor,
    );
    while let Some(frame) = format::next_checked_frame(file, header, offset, check.as_mut())? {
        let at = offset;
        offset += frame.disk.len() as u64;
        // A compact image's blocks, and in a precision image the frames
        // they restate context for, are checked as full replay checks them.
        #[cfg_attr(not(test), allow(unused_variables))]
        let supplement = if let Some(check) = &mut check {
            check.frame(&frame, at)?
        } else {
            None
        };
        if frame.operation == OP_TIME_ANCHOR {
            anchor_ms = frame
                .plain
                .get(format::PREFIX_SIZE..format::PREFIX_SIZE + 8)
                .map(|bytes| u64::from_le_bytes(bytes.try_into().expect("8 bytes")));
            continue;
        }
        if frame.operation == format::OP_ADVANCE {
            // Frames up to the checkpoint restate history; changes before it
            // are no longer in the log.
            if after < frame.sequence {
                return Err(Error::CursorBeforeHistory {
                    cursor: after,
                    floor: frame.sequence,
                });
            }
            head = frame.sequence;
            continue;
        }
        head = head.max(frame.sequence);
        let committed_at = anchor_ms.take();
        let body = frame.body();
        let name_of = |collections: &BTreeMap<u16, (String, usize)>| {
            collections
                .get(&frame.collection)
                .map(|entry| entry.0.clone())
                .unwrap_or_default()
        };
        let dimensions = collections
            .get(&frame.collection)
            .map(|entry| entry.1)
            .unwrap_or(0);
        let mut events = Vec::new();
        let corrupt = || Error::Corrupt {
            offset,
            reason: "undecodable frame in change feed",
        };
        match frame.operation {
            1 => {
                let vector = format::decode_vector(&body[..frame.count as usize * 4], offset)?;
                events.push((
                    name_of(&collections),
                    ChangeKind::Put {
                        id: frame.id,
                        record: Record {
                            vector,
                            payload: body[frame.count as usize * 4..].to_vec(),
                            metadata: Metadata::new(),
                        },
                        expires_at_ms: None,
                    },
                ));
            }
            2 => events.push((name_of(&collections), ChangeKind::Delete { id: frame.id })),
            3 => {
                let (operations, _marker) = format::decode_batch(
                    body,
                    frame.count as usize,
                    dimensions,
                    offset,
                    header.version,
                )?;
                let name = name_of(&collections);
                for operation in operations {
                    let kind = match operation {
                        crate::Mutation::Put {
                            id,
                            vector,
                            payload,
                        } => ChangeKind::Put {
                            id,
                            record: Record {
                                vector,
                                payload,
                                metadata: Metadata::new(),
                            },
                            expires_at_ms: None,
                        },
                        crate::Mutation::PutWithMetadata {
                            id,
                            vector,
                            payload,
                            metadata,
                        } => ChangeKind::Put {
                            id,
                            record: Record {
                                vector,
                                payload,
                                metadata,
                            },
                            expires_at_ms: None,
                        },
                        crate::Mutation::Delete { id } => ChangeKind::Delete { id },
                    };
                    events.push((name.clone(), kind));
                }
            }
            4 => {
                let record =
                    format::decode_extended(body, dimensions, offset, header.version >= 5)?;
                events.push((
                    name_of(&collections),
                    ChangeKind::Put {
                        id: frame.id,
                        record,
                        expires_at_ms: None,
                    },
                ));
            }
            9 => {
                if body.len() < 9 {
                    return Err(corrupt());
                }
                let expires = (body[0] == 1)
                    .then(|| u64::from_le_bytes(body[1..9].try_into().expect("8 bytes")));
                let record = format::decode_extended(&body[9..], dimensions, offset, true)?;
                events.push((
                    name_of(&collections),
                    ChangeKind::Put {
                        id: frame.id,
                        record,
                        expires_at_ms: expires,
                    },
                ));
            }
            8 => {
                let metadata = crate::metadata::decode(body).map_err(|_| corrupt())?;
                events.push((
                    name_of(&collections),
                    ChangeKind::MetadataUpdate {
                        id: frame.id,
                        metadata,
                    },
                ));
            }
            10 => events.push((name_of(&collections), ChangeKind::VectorsUpdated)),
            5 | format::OP_COLLECTION_PRECISE => {
                let (name, dimensions, settings) = if frame.operation == 5 {
                    format::decode_create(body, offset)?
                } else {
                    let (_, name, dimensions, settings) = format::decode_precise(body, offset)?;
                    (name, dimensions, settings)
                };
                collections.insert(frame.id as u16, (name.clone(), dimensions));
                events.push((
                    name,
                    ChangeKind::CollectionCreated {
                        dimensions,
                        settings,
                    },
                ));
            }
            6 => {
                let name = collections
                    .remove(&(frame.id as u16))
                    .map(|entry| entry.0)
                    .unwrap_or_default();
                events.push((name, ChangeKind::CollectionDropped));
            }
            7 => {
                let settings = format::decode_settings(body).map_err(|_| corrupt())?;
                let name = collections
                    .get(&(frame.id as u16))
                    .map(|entry| entry.0.clone())
                    .unwrap_or_default();
                events.push((name, ChangeKind::SettingsUpdated { settings }));
            }
            crate::property_graph::engine::codec::OP_GRAPH | format::OP_GRAPH_SNAPSHOT => {
                // Snapshot bodies of a checkpoint image track graph keys for
                // later frames; like all restated frames, they are at or
                // before every valid cursor.
                events = graphs.frame(&frame, offset, &collections)?;
            }
            // Record blocks restate records at the checkpoint: no change is
            // after the cursor, and no later frame needs their contents.
            format::OP_RECORD_BLOCK => {}
            // Compact blocks restate the checkpoint too; they were checked
            // above, and only exist behind FLAG_COMPACT.
            format::OP_COMPACT_BLOCK if check.is_some() => {
                #[cfg(test)]
                if let Some(crate::compact::Supplement::GraphSnapshot(bodies)) = supplement {
                    for body in bodies {
                        let encoded =
                            crate::property_graph::engine::encode_body(&body).map_err(|_| {
                                Error::Corrupt {
                                    offset: at,
                                    reason: "invalid compact graph snapshot",
                                }
                            })?;
                        let count =
                            u32::try_from(body.entries.len()).map_err(|_| Error::Corrupt {
                                offset: at,
                                reason: "invalid compact graph snapshot",
                            })?;
                        let plain = format::graph_snapshot_frame(frame.sequence, count, &encoded);
                        let snapshot = format::RawFrame {
                            operation: format::OP_GRAPH_SNAPSHOT,
                            collection: body.graph_id.0,
                            sequence: frame.sequence,
                            id: 0,
                            count,
                            disk: plain.clone(),
                            plain,
                        };
                        events.extend(graphs.frame(&snapshot, at, &collections)?);
                    }
                }
            }
            _ => return Err(corrupt()),
        }
        if frame.sequence <= after {
            continue;
        }
        if !changes.is_empty() && changes.len() + events.len() > max_events {
            has_more = true;
            break;
        }
        for (ordinal, (collection, kind)) in events.into_iter().enumerate() {
            changes.push(Change {
                sequence: frame.sequence,
                ordinal: ordinal as u32,
                collection,
                committed_at_ms: committed_at,
                kind,
            });
        }
        next = frame.sequence;
    }
    // The scan stopped at the end of the log, a frame that does not fit, or
    // a full batch (only ever after the image).
    if let Some(check) = &check {
        check.end(offset)?;
    }
    if after > head {
        return Err(Error::CursorAhead {
            cursor: after,
            head,
        });
    }
    let batch = ChangeBatch {
        changes,
        next: FeedCursor {
            lineage,
            sequence: next,
        },
        has_more,
        head,
    };
    let skipped = check
        .map(crate::compact::ImageCheck::into_skips)
        .unwrap_or_default();
    Ok((batch, skipped))
}

/// Reads changes from a closed (or copied) log file without opening a
/// `Database`. Pass `key` for encrypted logs.
pub fn read_log_changes(
    path: impl AsRef<Path>,
    key: Option<&EncryptionKey>,
    cursor: &FeedCursor,
    max_events: usize,
) -> Result<ChangeBatch> {
    let mut file = std::fs::File::open(path.as_ref())?;
    let (batch, skipped) = read_changes(&mut file, key, None, cursor, max_events)?;
    crate::compact::report(&skipped, "change_feed");
    Ok(batch)
}

#[cfg(feature = "json")]
impl GraphChange {
    /// One `graph_batch` operation in the REST/webhook/C ABI JSON shape:
    /// `op` names it, and u64 IDs and records are decimal strings.
    #[doc(hidden)]
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            GraphChange::NodeUpserted {
                id,
                key,
                labels,
                properties,
                created,
            } => json!({"op": "node_upserted", "id": id.to_string(), "key": key,
                "labels": labels, "properties": properties, "created": created}),
            GraphChange::NodePatched {
                id,
                key,
                add_labels,
                remove_labels,
                set,
                unset,
            } => json!({"op": "node_patched", "id": id.to_string(), "key": key,
                "add_labels": add_labels, "remove_labels": remove_labels, "set": set,
                "unset": unset}),
            GraphChange::NodeDeleted { id, key } => {
                json!({"op": "node_deleted", "id": id.to_string(), "key": key})
            }
            GraphChange::EdgeUpserted {
                id,
                key,
                source,
                source_key,
                relationship,
                destination,
                destination_key,
                properties,
                created,
            } => json!({"op": "edge_upserted", "id": id.to_string(), "key": key,
                "source": source.to_string(), "source_key": source_key,
                "relationship": relationship, "destination": destination.to_string(),
                "destination_key": destination_key, "properties": properties,
                "created": created}),
            GraphChange::EdgePatched {
                id,
                key,
                set,
                unset,
            } => {
                json!({"op": "edge_patched", "id": id.to_string(), "key": key, "set": set,
                    "unset": unset})
            }
            GraphChange::EdgeDeleted { id, key } => {
                json!({"op": "edge_deleted", "id": id.to_string(), "key": key})
            }
            GraphChange::Attached {
                node,
                node_key,
                collection,
                record,
            } => json!({"op": "attached", "node": node.to_string(), "node_key": node_key,
                "collection": collection, "record": record.to_string()}),
            GraphChange::Detached {
                node,
                node_key,
                collection,
                record,
            } => json!({"op": "detached", "node": node.to_string(), "node_key": node_key,
                "collection": collection, "record": record.to_string()}),
            GraphChange::SchemaUpdated { schema } => {
                json!({"op": "schema_updated", "schema": schema.to_json()})
            }
            // Gap 4: the served feeds (`changes_after`) refuse temporal
            // entries, so no server reaches this; there is no JSON shape yet.
            GraphChange::Temporal(_) => json!({"op": "unsupported_temporal"}),
        }
    }
}

#[cfg(feature = "json")]
impl ChangeKind {
    /// The type-specific fields of a graph event in the REST/webhook/C ABI
    /// JSON shape (`type`, `graph`, and for `graph_batch` the decimal-string
    /// `revision` and `operations`); `None` for vector events.
    #[doc(hidden)]
    pub fn graph_json(&self) -> Option<serde_json::Value> {
        use serde_json::json;
        Some(match self {
            ChangeKind::GraphCreated { graph } => json!({"type": "graph_created", "graph": graph}),
            ChangeKind::GraphDropped { graph } => json!({"type": "graph_dropped", "graph": graph}),
            ChangeKind::GraphBatch {
                graph,
                revision,
                operations,
            } => json!({"type": "graph_batch", "graph": graph, "revision": revision.to_string(),
                "operations": operations.iter().map(GraphChange::to_json).collect::<Vec<_>>()}),
            _ => return None,
        })
    }
}
