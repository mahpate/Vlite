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
pub use crate::property_graph::GraphChange;

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

pub(crate) fn read_changes<R: Read + Seek>(
    file: &mut R,
    key: Option<&EncryptionKey>,
    known: Option<&crate::crypto::Cipher>,
    cursor: &FeedCursor,
    max_events: usize,
) -> Result<ChangeBatch> {
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
    )
}

fn scan<R: Read + Seek>(
    file: &mut R,
    header: &LogHeader,
    lineage: u64,
    after: u64,
    max_events: usize,
) -> Result<ChangeBatch> {
    let mut collections: BTreeMap<u16, (String, usize)> = BTreeMap::new();
    collections.insert(0, ("default".to_string(), header.dimensions));
    let mut offset = header.start;
    let mut head = 0u64;
    let mut changes = Vec::new();
    let mut next = after;
    let mut anchor_ms: Option<u64> = None;
    let mut has_more = false;
    let mut graphs = crate::property_graph::feed::FeedGraphs::default();
    while let Some(frame) = format::next_frame(file, header, offset)? {
        offset += frame.disk.len() as u64;
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
            5 => {
                let (name, dimensions, settings) = format::decode_create(body, offset)?;
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
            crate::property_graph::engine::codec::OP_GRAPH => {
                events = graphs.frame(frame.sequence, frame.count, body, offset, &collections)?;
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
    if after > head {
        return Err(Error::CursorAhead {
            cursor: after,
            head,
        });
    }
    Ok(ChangeBatch {
        changes,
        next: FeedCursor {
            lineage,
            sequence: next,
        },
        has_more,
        head,
    })
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
    read_changes(&mut file, key, None, cursor, max_events)
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
