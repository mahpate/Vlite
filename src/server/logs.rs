use std::{
    collections::VecDeque,
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use serde_json::{Value, json};

pub(super) const DEFAULT_CAPACITY: usize = 2_000;

#[derive(Clone, Serialize)]
pub(super) struct Entry {
    pub seq: u64,
    pub ts_ms: u64,
    pub level: &'static str,
    pub target: String,
    pub message: String,
    pub fields: Value,
}

struct State {
    capacity: usize,
    next_seq: u64,
    dropped: u64,
    entries: VecDeque<Entry>,
}

pub(super) struct Buffer(Mutex<State>);

impl Buffer {
    fn new(capacity: usize) -> Self {
        Self(Mutex::new(State {
            capacity,
            next_seq: 1,
            dropped: 0,
            entries: VecDeque::with_capacity(capacity),
        }))
    }

    pub(super) fn set_capacity(&self, capacity: usize) {
        if let Ok(mut state) = self.0.lock() {
            state.capacity = capacity;
            while state.entries.len() > capacity {
                state.entries.pop_front();
                state.dropped += 1;
            }
        }
    }

    pub(super) fn record(&self, level: &'static str, target: &str, message: String, fields: Value) {
        let Ok(mut state) = self.0.lock() else { return };
        let seq = state.next_seq;
        state.next_seq = state.next_seq.saturating_add(1);
        if state.entries.len() == state.capacity {
            state.entries.pop_front();
            state.dropped = state.dropped.saturating_add(1);
        }
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| {
                duration.as_millis().min(u64::MAX as u128) as u64
            });
        state.entries.push_back(Entry {
            seq,
            ts_ms,
            level,
            target: target.to_owned(),
            message,
            fields: if fields.is_object() {
                fields
            } else {
                json!({})
            },
        });
    }

    pub(super) fn snapshot(&self) -> (Vec<Entry>, u64, usize, u64) {
        self.0.lock().map_or_else(
            |_| (Vec::new(), 0, DEFAULT_CAPACITY, 0),
            |state| {
                (
                    state.entries.iter().cloned().collect(),
                    state.dropped,
                    state.capacity,
                    state.next_seq.saturating_sub(1),
                )
            },
        )
    }

    pub(super) fn stats(&self) -> (u64, usize, u64) {
        self.0.lock().map_or((0, DEFAULT_CAPACITY, 0), |state| {
            (
                state.dropped,
                state.capacity,
                state.next_seq.saturating_sub(1),
            )
        })
    }
}

fn global() -> &'static Buffer {
    static BUFFER: OnceLock<Buffer> = OnceLock::new();
    BUFFER.get_or_init(|| Buffer::new(DEFAULT_CAPACITY))
}

pub(super) fn configure(capacity: usize) {
    global().set_capacity(capacity);
}

pub(super) fn buffer() -> &'static Buffer {
    global()
}

/// Print the exact legacy message and capture it as a structured in-memory log.
pub(super) fn stderr(level: &'static str, target: &str, message: impl Into<String>) {
    stderr_with_fields(level, target, message.into(), Value::Null);
}

pub(super) fn stderr_with_fields(
    level: &'static str,
    target: &str,
    message: String,
    extra_fields: Value,
) {
    eprintln!("{message}");
    let mut fields = serde_json::from_str::<Value>(&message)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if let Some(extra) = extra_fields.as_object() {
        fields.extend(extra.clone());
    }
    global().record(level, target, message, Value::Object(fields));
}

fn level_index(level: &str) -> Option<u8> {
    match level {
        "trace" => Some(0),
        "debug" => Some(1),
        "info" => Some(2),
        "warn" => Some(3),
        "error" => Some(4),
        _ => None,
    }
}

#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Query {
    since: Option<u64>,
    level: Option<String>,
    q: Option<String>,
    limit: Option<usize>,
}

pub(super) fn query(query: Query) -> Result<Value, (&'static str, String)> {
    let since = query.since;
    let limit = query.limit.unwrap_or(200);
    if !(1..=1000).contains(&limit) {
        return Err(("invalid_query", "limit must be 1..1000".to_owned()));
    }
    let min_level = match query.level.as_deref() {
        None => 0,
        Some(level) => level_index(level).ok_or_else(|| {
            (
                "invalid_query",
                "level must be trace, debug, info, warn or error".to_owned(),
            )
        })?,
    };
    let needle = query.q.as_deref().map(str::to_lowercase);
    let (entries, dropped, capacity, _) = global().snapshot();
    let mut matches = entries
        .into_iter()
        .filter(|entry| since.is_none_or(|seq| entry.seq > seq))
        .filter(|entry| level_index(entry.level).is_some_and(|level| level >= min_level))
        .filter(|entry| {
            needle.as_ref().is_none_or(|needle| {
                entry.message.to_lowercase().contains(needle)
                    || entry.target.to_lowercase().contains(needle)
            })
        })
        .collect::<Vec<_>>();
    if since.is_none() {
        let skip = matches.len().saturating_sub(limit);
        matches.drain(..skip);
    } else {
        matches.truncate(limit);
    }
    let next_seq = matches.last().map_or(since.unwrap_or(0), |entry| entry.seq);
    let entries = matches
        .iter()
        .map(|entry| serde_json::to_value(entry).expect("serializable log entry"))
        .collect::<Vec<_>>();
    Ok(json!({"entries":entries,"next_seq":next_seq,"dropped":dropped,"capacity":capacity}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_buffer_counts_overwritten_entries_and_keeps_monotonic_sequences() {
        let buffer = Buffer::new(2);
        for n in 0..3 {
            buffer.record("info", "test", n.to_string(), json!({"n": n}));
        }
        let (entries, dropped, capacity, last_seq) = buffer.snapshot();
        assert_eq!(
            entries.iter().map(|entry| entry.seq).collect::<Vec<_>>(),
            [2, 3]
        );
        assert_eq!(dropped, 1);
        assert_eq!(capacity, 2);
        assert_eq!(last_seq, 3);
    }
}
