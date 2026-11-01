//! Gap 4: time-aware graph adapter.
//!
//! The temporal capability is a required header flag
//! ([`crate::format::FLAG_TEMPORAL`]) set atomically with the first temporal
//! write. Preactivation, like the P3a precision gate
//! (`crate::compact::precision_supported`): release builds do not support it
//! yet, so they refuse temporal files with `UnsupportedFeature(FLAG_TEMPORAL)`
//! and reject temporal graph operations the same way. Only this crate's tests
//! switch support on, per thread, with an [`Activation`], until temporal reads
//! (Gap 4 slice C) qualify every path.

use crate::format::{self, FLAG_TEMPORAL};
use crate::{Database, Error, Expected, Metadata, Result};

use super::convert::{metadata_from_properties, precondition, properties_from_metadata};
use super::engine::temporal::Segment;
use super::engine::{GraphState, TemporalMutation};
use super::{EdgeRef, selector};

/// Gap 4 (unreleased): one validity segment of a temporal edge. The edge
/// holds `properties` for validity times `valid_from <= t < valid_to`, in
/// milliseconds since the Unix epoch; `None` leaves that side unbounded.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TemporalSegment {
    pub valid_from: Option<i64>,
    pub valid_to: Option<i64>,
    pub properties: Metadata,
}

/// Gap 4 (unreleased): a temporal-edge operation of a graph batch
/// ([`GraphOp::Temporal`](super::GraphOp::Temporal)).
///
/// A temporal edge has one ID, key, type and pair of endpoints, and an
/// ordered series of at most 256 non-overlapping segments (touching is
/// allowed) instead of properties. Segments are addressed by their exact
/// current bounds, and every change to an existing series needs
/// `Expected::Revision` with the edge's revision, which covers the whole
/// series; within one batch it is the revision the edge had before the
/// batch, so closing a segment and appending its successor is one atomic,
/// revision-checked supersession. Invalid input is `InvalidQuery`. The
/// first temporal write enables the file's temporal capability, which
/// builds without it refuse to open; directory stores cannot enable it
/// (`UnsupportedFeature`), only inherit it by conversion. Not yet released:
/// every build refuses these operations with `UnsupportedFeature(64)`
/// until temporal reads exist.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum TemporalOp {
    /// Creates a temporal edge with this series (possibly empty). With a
    /// key that is live, an identical temporal edge (same endpoints, type
    /// and bit-identical series) is returned with `created = false`; any
    /// other edge is `GraphKeyConflict`.
    CreateEdge {
        key: Option<String>,
        source: String,
        relationship: String,
        destination: String,
        segments: Vec<TemporalSegment>,
    },
    /// Makes an ordinary edge temporal, keeping its ID, key and endpoints:
    /// its properties become one all-time segment.
    PromoteEdge { edge: EdgeRef, expected: Expected },
    /// Inserts a segment at its ordered position (backfill is allowed).
    AppendSegment {
        edge: EdgeRef,
        segment: TemporalSegment,
        expected: Expected,
    },
    /// Replaces the segment with exactly these bounds; the replacement may
    /// have other bounds.
    ReplaceSegment {
        edge: EdgeRef,
        valid_from: Option<i64>,
        valid_to: Option<i64>,
        segment: TemporalSegment,
        expected: Expected,
    },
    /// Ends the segment with exactly these bounds at `at`, which must lie
    /// strictly inside it; `at` equal to its finite end changes nothing.
    CloseSegment {
        edge: EdgeRef,
        valid_from: Option<i64>,
        valid_to: Option<i64>,
        at: i64,
        expected: Expected,
    },
    /// Removes the segment with exactly these bounds. The edge stays, with
    /// a possibly empty series, until it is deleted or expires.
    RemoveSegment {
        edge: EdgeRef,
        valid_from: Option<i64>,
        valid_to: Option<i64>,
        expected: Expected,
    },
}

pub(super) fn segment_in(segment: &TemporalSegment) -> Result<Segment> {
    Ok(Segment {
        valid_from: segment.valid_from,
        valid_to: segment.valid_to,
        properties: properties_from_metadata(&segment.properties)?,
    })
}

pub(super) fn segment_out(segment: &Segment) -> TemporalSegment {
    TemporalSegment {
        valid_from: segment.valid_from,
        valid_to: segment.valid_to,
        properties: metadata_from_properties(&segment.properties),
    }
}

/// Converts one temporal operation (`UnsupportedFeature` where this build
/// does not support the capability).
pub(super) fn mutation(op: &TemporalOp) -> Result<TemporalMutation> {
    if !supported() {
        return Err(Error::UnsupportedFeature(FLAG_TEMPORAL));
    }
    Ok(match op {
        TemporalOp::CreateEdge {
            key,
            source,
            relationship,
            destination,
            segments,
        } => TemporalMutation::CreateEdge {
            key: key.clone(),
            source: source.clone(),
            relationship: relationship.clone(),
            destination: destination.clone(),
            segments: segments.iter().map(segment_in).collect::<Result<_>>()?,
        },
        TemporalOp::PromoteEdge { edge, expected } => TemporalMutation::Promote {
            edge: selector(edge),
            expected: precondition(*expected),
        },
        TemporalOp::AppendSegment {
            edge,
            segment,
            expected,
        } => TemporalMutation::AppendSegment {
            edge: selector(edge),
            segment: segment_in(segment)?,
            expected: precondition(*expected),
        },
        TemporalOp::ReplaceSegment {
            edge,
            valid_from,
            valid_to,
            segment,
            expected,
        } => TemporalMutation::ReplaceSegment {
            edge: selector(edge),
            valid_from: *valid_from,
            valid_to: *valid_to,
            segment: segment_in(segment)?,
            expected: precondition(*expected),
        },
        TemporalOp::CloseSegment {
            edge,
            valid_from,
            valid_to,
            at,
            expected,
        } => TemporalMutation::CloseSegment {
            edge: selector(edge),
            valid_from: *valid_from,
            valid_to: *valid_to,
            at: *at,
            expected: precondition(*expected),
        },
        TemporalOp::RemoveSegment {
            edge,
            valid_from,
            valid_to,
            expected,
        } => TemporalMutation::RemoveSegment {
            edge: selector(edge),
            valid_from: *valid_from,
            valid_to: *valid_to,
            expected: precondition(*expected),
        },
    })
}

/// Gives a graph state loaded from a file with header `flags` the file's
/// temporal capability; a state holding temporal edges without it is
/// corrupt (`offset` is where its graph data starts).
pub(crate) fn adopt(state: &mut GraphState, flags: u16, offset: u64) -> Result<()> {
    if flags & FLAG_TEMPORAL != 0 {
        state.set_temporal(true);
    } else if state.is_temporal() {
        return Err(format::corrupt(
            offset,
            "temporal entry without the temporal capability",
        ));
    }
    Ok(())
}

impl Database {
    /// Whether a batch may enable the temporal capability (see
    /// [`Database::enable_temporal`]): a directory store's segments all
    /// repeat the header flags, so it only inherits the capability by
    /// conversion from a file that has it.
    pub(super) fn check_temporal_enable(&self) -> Result<()> {
        if self.file.is_chain() {
            return Err(Error::UnsupportedFeature(FLAG_TEMPORAL));
        }
        Ok(())
    }

    /// Durably sets `FLAG_TEMPORAL` in the header before the first temporal
    /// frame is appended, so no file holds temporal entries without it. The
    /// header is rewritten in one write of its 32 bytes (flags and checksum
    /// share a sector) and synced; a crash before the frame leaves a capable
    /// file without temporal edges. A failure poisons the handle like a
    /// failed append.
    pub(super) fn enable_temporal(&mut self) -> Result<()> {
        use std::io::{Read, Seek, SeekFrom, Write};
        self.check_temporal_enable()?;
        let flags = self.flags | FLAG_TEMPORAL;
        let written = (|| -> std::io::Result<()> {
            #[cfg(test)]
            if FAIL_HEADER.with(|fail| fail.replace(false)) {
                return Err(std::io::Error::other("injected header write failure"));
            }
            let mut header = [0u8; format::HEADER_SIZE as usize];
            self.file.seek(SeekFrom::Start(0))?;
            self.file.read_exact(&mut header)?;
            if header[26..28] != self.flags.to_le_bytes()
                || format::crc32(&header[..28]).to_le_bytes() != header[28..32]
            {
                return Err(std::io::Error::other("database header changed"));
            }
            header[26..28].copy_from_slice(&flags.to_le_bytes());
            let checksum = format::crc32(&header[..28]);
            header[28..32].copy_from_slice(&checksum.to_le_bytes());
            self.file.seek(SeekFrom::Start(0))?;
            self.file.write_all(&header)?;
            self.file.sync_all()
        })();
        if let Err(error) = written {
            self.poisoned = true;
            return Err(Error::CommitIndeterminate(error));
        }
        self.flags = flags;
        Ok(())
    }

    /// Gap 4 (unreleased): [`Database::changes_after`] that also reports
    /// temporal-edge changes as
    /// [`GraphChange::Temporal`](crate::changes::GraphChange::Temporal), in
    /// commit order. `changes_after`, `read_log_changes` and every server
    /// feed refuse a log with temporal changes (`UnsupportedFeature(64)`)
    /// rather than omit them. Not yet released: every build refuses it with
    /// `UnsupportedFeature(64)` until temporal reads exist.
    pub fn temporal_changes_after(
        &self,
        cursor: &crate::changes::FeedCursor,
        max_events: usize,
    ) -> Result<crate::changes::ChangeBatch> {
        if !supported() {
            return Err(Error::UnsupportedFeature(FLAG_TEMPORAL));
        }
        self.lineage.ok_or(Error::ChangeFeedRequiresUpgrade)?;
        // As `changes_after`: never publish commits a crash could lose.
        self.sync()?;
        let mut file = &self.file;
        crate::changes::read_changes_with(
            &mut file,
            None,
            self.cipher.as_ref(),
            cursor,
            max_events,
            true,
        )
        .map(|(batch, _)| batch)
    }
}

/// Whether this build reads and writes temporal graph files (Gap 4).
pub(crate) fn supported() -> bool {
    #[cfg(test)]
    {
        TEMPORAL.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        false
    }
}

#[cfg(test)]
thread_local! {
    static TEMPORAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Fails the next header rewrite of [`Database::enable_temporal`].
    pub(crate) static FAIL_HEADER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test-only switch of [`supported`] on this thread, restored when dropped.
#[cfg(test)]
pub(crate) struct Activation(bool);

#[cfg(test)]
impl Activation {
    pub(crate) fn on() -> Self {
        Self::set(true)
    }

    pub(crate) fn set(supported: bool) -> Self {
        Self(TEMPORAL.with(|cell| cell.replace(supported)))
    }
}

#[cfg(test)]
impl Drop for Activation {
    fn drop(&mut self) {
        TEMPORAL.with(|cell| cell.set(self.0));
    }
}

#[cfg(test)]
mod tests;
