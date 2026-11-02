//! Private packed fields prototype. Never registered as a graph/disk codec.

use super::*;
use crate::property_graph::engine::PropertyProjection;

mod fields;

const MAGIC: &[u8; 8] = b"VLCOL001";
type Result<T> = std::result::Result<T, CodecError>;

fn corrupt<T>() -> Result<T> {
    Err(CodecError::Corrupt)
}

fn u32_of(n: usize) -> Result<u32> {
    u32::try_from(n).map_err(|_| CodecError::Corrupt)
}

fn bounded(n: usize) -> Result<()> {
    if n > MAX_BYTES {
        return corrupt();
    }
    Ok(())
}

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(n).ok_or(CodecError::Corrupt)?;
        self.0 = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?.try_into().map_err(|_| CodecError::Corrupt)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    fn count(&mut self) -> Result<usize> {
        Ok(u32::from_le_bytes(self.array()?) as usize)
    }

    fn block(&mut self) -> Result<&'a [u8]> {
        let n = self.count()?;
        self.take(n)
    }

    fn end(&self) -> Result<()> {
        if !self.0.is_empty() {
            return corrupt();
        }
        Ok(())
    }
}

fn block(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    bounded(out.len().saturating_add(4).saturating_add(bytes.len()))?;
    out.extend_from_slice(&u32_of(bytes.len())?.to_le_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

/// One contiguous payload and row offsets, with no row-owned allocations.
struct Bytes {
    offsets: Box<[u32]>,
    data: Box<[u8]>,
}

impl Bytes {
    fn build(rows: impl IntoIterator<Item = Vec<u8>>) -> Result<Self> {
        let mut offsets = vec![0];
        let mut data = Vec::new();
        for row in rows {
            bounded(
                data.len()
                    .saturating_add(row.len())
                    .saturating_add(4 * offsets.len()),
            )?;
            data.extend(row);
            offsets.push(u32_of(data.len())?);
        }
        Ok(Self {
            offsets: offsets.into_boxed_slice(),
            data: data.into_boxed_slice(),
        })
    }

    fn get(&self, row: usize) -> Result<&[u8]> {
        let start = *self.offsets.get(row).ok_or(CodecError::Corrupt)? as usize;
        let end = *self.offsets.get(row + 1).ok_or(CodecError::Corrupt)? as usize;
        self.data.get(start..end).ok_or(CodecError::Corrupt)
    }

    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.heap_bytes());
        for offset in &self.offsets {
            bytes.extend_from_slice(&offset.to_le_bytes());
        }
        bytes.extend_from_slice(&self.data);
        bytes
    }

    fn decode(bytes: &[u8], rows: usize) -> Result<Self> {
        let n = rows.checked_add(1).ok_or(CodecError::Corrupt)?;
        let header = n.checked_mul(4).ok_or(CodecError::Corrupt)?;
        let mut cursor = Cursor(bytes);
        let offsets = cursor.take(header)?;
        let offsets: Vec<_> = offsets
            .as_chunks::<4>()
            .0
            .iter()
            .map(|value| u32::from_le_bytes(*value))
            .collect();
        if offsets.first() != Some(&0)
            || offsets.last().copied().map(|n| n as usize) != Some(cursor.0.len())
            || offsets.windows(2).any(|pair| pair[0] > pair[1])
        {
            return corrupt();
        }
        Ok(Self {
            offsets: offsets.into_boxed_slice(),
            data: cursor.0.into(),
        })
    }

    fn heap_bytes(&self) -> usize {
        self.offsets.len() * 4 + self.data.len()
    }
}

enum Presence {
    Sparse(Box<[u32]>),
    Dense { bits: Box<[u64]>, ranks: Box<[u32]> },
}

impl Presence {
    fn build(rows: usize, ordinals: &[u32]) -> Self {
        let words = rows.div_ceil(64);
        if 4 * ordinals.len() <= 8 * words + 4 * (words + 1) {
            return Self::Sparse(ordinals.into());
        }
        let mut bits = vec![0u64; words];
        for &ordinal in ordinals {
            bits[ordinal as usize / 64] |= 1 << (ordinal % 64);
        }
        let mut ranks = vec![0u32];
        for word in &bits {
            ranks.push(ranks.last().unwrap() + word.count_ones());
        }
        Self::Dense {
            bits: bits.into_boxed_slice(),
            ranks: ranks.into_boxed_slice(),
        }
    }

    fn rank(&self, row: u32) -> Option<usize> {
        match self {
            Self::Sparse(rows) => rows.binary_search(&row).ok(),
            Self::Dense { bits, ranks } => {
                let at = row as usize / 64;
                let bit = row % 64;
                let word = *bits.get(at)?;
                (word & (1 << bit) != 0).then(|| {
                    ranks[at] as usize + (word & ((1u64 << bit) - 1)).count_ones() as usize
                })
            }
        }
    }

    fn encode(&self) -> (u8, Vec<u8>) {
        let mut out = Vec::new();
        match self {
            Self::Sparse(rows) => {
                for row in rows {
                    out.extend_from_slice(&row.to_le_bytes());
                }
                (0, out)
            }
            Self::Dense { bits, ranks } => {
                for word in bits {
                    out.extend_from_slice(&word.to_le_bytes());
                }
                for rank in ranks {
                    out.extend_from_slice(&rank.to_le_bytes());
                }
                (1, out)
            }
        }
    }

    fn decode(mode: u8, bytes: &[u8], rows: usize, count: usize) -> Result<Self> {
        let mut cursor = Cursor(bytes);
        let ordinals = match mode {
            0 => {
                if count > bytes.len() / 4 {
                    return corrupt();
                }
                (0..count)
                    .map(|_| cursor.count().and_then(u32_of))
                    .collect::<Result<Vec<_>>>()?
            }
            1 => {
                let words = rows.div_ceil(64);
                if bytes.len() != 8 * words + 4 * (words + 1) {
                    return corrupt();
                }
                let mut ordinals = Vec::new();
                let mut ranks = vec![0];
                for at in 0..words {
                    let word = u64::from_le_bytes(cursor.array()?);
                    for bit in 0..64 {
                        if word & (1 << bit) != 0 {
                            ordinals.push(u32_of(at * 64 + bit)?);
                        }
                    }
                    ranks.push(u32_of(ordinals.len())?);
                }
                for rank in ranks {
                    if cursor.count()? != rank as usize {
                        return corrupt();
                    }
                }
                ordinals
            }
            _ => return corrupt(),
        };
        cursor.end()?;
        if ordinals.len() != count
            || ordinals.last().is_some_and(|&last| last as usize >= rows)
            || ordinals.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return corrupt();
        }
        // Canonical choice also verifies dense padding bits and anchor values.
        let result = Self::build(rows, &ordinals);
        if result.encode() != (mode, bytes.to_vec()) {
            return corrupt();
        }
        Ok(result)
    }

    fn heap_bytes(&self) -> usize {
        match self {
            Self::Sparse(rows) => rows.len() * 4,
            Self::Dense { bits, ranks } => bits.len() * 8 + ranks.len() * 4,
        }
    }
}

fn zigzag(value: i64) -> u64 {
    ((value as u64) << 1) ^ ((value >> 63) as u64)
}

fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

fn width(values: &[u64]) -> u8 {
    (64 - values.iter().copied().max().unwrap_or(0).leading_zeros()) as u8
}

fn pack(values: &[u64], width: u8) -> Vec<u8> {
    let mut bytes = vec![0; (values.len() * width as usize).div_ceil(8)];
    for (at, &value) in values.iter().enumerate() {
        for bit in 0..width as usize {
            if value & (1u64 << bit) != 0 {
                let position = at * width as usize + bit;
                bytes[position / 8] |= 1 << (position % 8);
            }
        }
    }
    bytes
}

fn unpack(bytes: &[u8], width: u8, at: usize) -> Result<u64> {
    if width > 64 {
        return corrupt();
    }
    let mut value = 0;
    for bit in 0..width as usize {
        let position = at
            .checked_mul(width as usize)
            .and_then(|n| n.checked_add(bit))
            .ok_or(CodecError::Corrupt)?;
        if bytes.get(position / 8).ok_or(CodecError::Corrupt)? & (1 << (position % 8)) != 0 {
            value |= 1u64 << bit;
        }
    }
    Ok(value)
}

fn raw(value: &PropertyValue) -> Vec<u8> {
    let mut out = Vec::new();
    match value {
        PropertyValue::String(value) => {
            out.push(1);
            out.extend_from_slice(&(value.len() as u32).to_le_bytes());
            out.extend_from_slice(value.as_bytes());
        }
        PropertyValue::Integer(value) => {
            out.push(2);
            out.extend_from_slice(&value.to_le_bytes());
        }
        PropertyValue::Float(value) => {
            out.push(3);
            out.extend_from_slice(&value.to_bits().to_le_bytes());
        }
        PropertyValue::Boolean(value) => out.extend_from_slice(&[4, u8::from(*value)]),
        PropertyValue::GeoPoint {
            latitude,
            longitude,
        } => {
            out.push(5);
            out.extend_from_slice(&latitude.to_bits().to_le_bytes());
            out.extend_from_slice(&longitude.to_bits().to_le_bytes());
        }
    }
    out
}

fn raw_value(bytes: &[u8]) -> Result<PropertyValue> {
    let mut cursor = Cursor(bytes);
    let result = match cursor.byte()? {
        1 => PropertyValue::String(
            std::str::from_utf8(cursor.block()?)
                .map_err(|_| CodecError::Corrupt)?
                .to_owned(),
        ),
        2 => PropertyValue::Integer(i64::from_le_bytes(cursor.array()?)),
        3 => PropertyValue::Float(f64::from_bits(u64::from_le_bytes(cursor.array()?))),
        4 => PropertyValue::Boolean(match cursor.byte()? {
            0 => false,
            1 => true,
            _ => return corrupt(),
        }),
        5 => PropertyValue::GeoPoint {
            latitude: f64::from_bits(u64::from_le_bytes(cursor.array()?)),
            longitude: f64::from_bits(u64::from_le_bytes(cursor.array()?)),
        },
        _ => return corrupt(),
    };
    cursor.end()?;
    properties::validate_value(&result).map_err(|_| CodecError::Corrupt)?;
    Ok(result)
}

struct Values {
    kind: u8,
    width: u8,
    data: Box<[u8]>,
    // Parsed once at admission; no first-read rebuilding or per-row ownership.
    indexed: Option<Bytes>,
}

impl Values {
    fn build(values: &[&PropertyValue]) -> Result<Self> {
        let raw_rows = Bytes::build(values.iter().map(|value| raw(value)))?;
        let mut result = Self {
            kind: 0,
            width: 0,
            data: raw_rows.encode().into_boxed_slice(),
            indexed: Some(raw_rows),
        };
        // data contains the canonical bytes; indexed keeps only offsets at runtime.
        result.discard_index_payload();
        let first = values.first().ok_or(CodecError::Corrupt)?;
        let candidate = match first {
            PropertyValue::Integer(_)
                if values
                    .iter()
                    .all(|v| matches!(v, PropertyValue::Integer(_))) =>
            {
                let values: Vec<_> = values
                    .iter()
                    .map(|v| match v {
                        PropertyValue::Integer(n) => zigzag(*n),
                        _ => unreachable!(),
                    })
                    .collect();
                let width = width(&values);
                Some((1, width, pack(&values, width)))
            }
            PropertyValue::Float(_)
                if values.iter().all(|v| matches!(v, PropertyValue::Float(_))) =>
            {
                Some((
                    2,
                    64,
                    values
                        .iter()
                        .flat_map(|v| match v {
                            PropertyValue::Float(n) => n.to_bits().to_le_bytes(),
                            _ => unreachable!(),
                        })
                        .collect(),
                ))
            }
            PropertyValue::Boolean(_)
                if values
                    .iter()
                    .all(|v| matches!(v, PropertyValue::Boolean(_))) =>
            {
                let values: Vec<_> = values
                    .iter()
                    .map(|v| match v {
                        PropertyValue::Boolean(n) => u64::from(*n),
                        _ => unreachable!(),
                    })
                    .collect();
                Some((3, 1, pack(&values, 1)))
            }
            PropertyValue::GeoPoint { .. }
                if values
                    .iter()
                    .all(|v| matches!(v, PropertyValue::GeoPoint { .. })) =>
            {
                Some((
                    4,
                    0,
                    values
                        .iter()
                        .flat_map(|v| match v {
                            PropertyValue::GeoPoint {
                                latitude,
                                longitude,
                            } => {
                                let mut bytes = [0; 16];
                                bytes[..8].copy_from_slice(&latitude.to_bits().to_le_bytes());
                                bytes[8..].copy_from_slice(&longitude.to_bits().to_le_bytes());
                                bytes
                            }
                            _ => unreachable!(),
                        })
                        .collect(),
                ))
            }
            PropertyValue::String(_)
                if values.iter().all(|v| matches!(v, PropertyValue::String(_))) =>
            {
                let unique: BTreeSet<_> = values
                    .iter()
                    .map(|v| match v {
                        PropertyValue::String(s) => s.as_str(),
                        _ => unreachable!(),
                    })
                    .collect();
                let strings: Vec<_> = unique.into_iter().collect();
                let codes: Vec<_> = values
                    .iter()
                    .map(|v| match v {
                        PropertyValue::String(s) => {
                            strings.binary_search(&s.as_str()).unwrap() as u64
                        }
                        _ => unreachable!(),
                    })
                    .collect();
                let width = width(&codes);
                let dictionary = Bytes::build(strings.iter().map(|s| s.as_bytes().to_vec()))?;
                let mut data = u32_of(strings.len())?.to_le_bytes().to_vec();
                block(&mut data, &dictionary.encode())?;
                data.extend(pack(&codes, width));
                Some((5, width, data))
            }
            _ => None,
        };
        if let Some((kind, width, data)) = candidate
            && data.len() < result.data.len()
        {
            result = Self::decode(kind, width, &data, values.len())?;
        }
        Ok(result)
    }

    fn discard_index_payload(&mut self) {
        if let Some(indexed) = &mut self.indexed {
            indexed.data = Box::new([]);
        }
    }

    fn decode(kind: u8, width: u8, data: &[u8], count: usize) -> Result<Self> {
        let indexed = match kind {
            0 if width == 0 => Some(Bytes::decode(data, count)?),
            1 | 3 if width <= 64 && (kind != 3 || width == 1) => {
                if data.len() != (count * width as usize).div_ceil(8) {
                    return corrupt();
                }
                None
            }
            2 if width == 64 && data.len() == count * 8 => None,
            4 if width == 0 && data.len() == count * 16 => None,
            5 if width <= 32 => {
                let mut cursor = Cursor(data);
                let n = cursor.count()?;
                let dict = cursor.block()?;
                if n == 0 || n > count || cursor.0.len() != (count * width as usize).div_ceil(8) {
                    return corrupt();
                }
                let dictionary = Bytes::decode(dict, n)?;
                for at in 0..n {
                    let text = dictionary.get(at)?;
                    if text.len() > 4096
                        || std::str::from_utf8(text).is_err()
                        || at > 0 && dictionary.get(at - 1)? >= text
                    {
                        return corrupt();
                    }
                }
                Some(dictionary)
            }
            _ => return corrupt(),
        };
        let mut result = Self {
            kind,
            width,
            data: data.into(),
            indexed,
        };
        result.discard_index_payload();
        Ok(result)
    }

    fn get(&self, at: usize) -> Result<PropertyValue> {
        let result = match self.kind {
            0 => {
                let index = self.indexed.as_ref().ok_or(CodecError::Corrupt)?;
                let header = index.offsets.len() * 4;
                let start = *index.offsets.get(at).ok_or(CodecError::Corrupt)? as usize;
                let end = *index.offsets.get(at + 1).ok_or(CodecError::Corrupt)? as usize;
                raw_value(
                    self.data
                        .get(header + start..header + end)
                        .ok_or(CodecError::Corrupt)?,
                )?
            }
            1 => PropertyValue::Integer(unzigzag(unpack(&self.data, self.width, at)?)),
            2 => {
                let bytes = self
                    .data
                    .get(at * 8..at * 8 + 8)
                    .ok_or(CodecError::Corrupt)?;
                PropertyValue::Float(f64::from_bits(u64::from_le_bytes(
                    bytes.try_into().unwrap(),
                )))
            }
            3 => PropertyValue::Boolean(unpack(&self.data, 1, at)? != 0),
            4 => {
                let bytes = self
                    .data
                    .get(at * 16..at * 16 + 16)
                    .ok_or(CodecError::Corrupt)?;
                PropertyValue::GeoPoint {
                    latitude: f64::from_bits(u64::from_le_bytes(bytes[..8].try_into().unwrap())),
                    longitude: f64::from_bits(u64::from_le_bytes(bytes[8..].try_into().unwrap())),
                }
            }
            5 => {
                let mut cursor = Cursor(&self.data);
                cursor.count()?;
                let dict = cursor.block()?;
                let index = self.indexed.as_ref().ok_or(CodecError::Corrupt)?;
                let code = unpack(cursor.0, self.width, at)? as usize;
                let header = index.offsets.len() * 4;
                let start = *index.offsets.get(code).ok_or(CodecError::Corrupt)? as usize;
                let end = *index.offsets.get(code + 1).ok_or(CodecError::Corrupt)? as usize;
                PropertyValue::String(
                    std::str::from_utf8(
                        dict.get(header + start..header + end)
                            .ok_or(CodecError::Corrupt)?,
                    )
                    .map_err(|_| CodecError::Corrupt)?
                    .to_owned(),
                )
            }
            _ => return corrupt(),
        };
        properties::validate_value(&result).map_err(|_| CodecError::Corrupt)?;
        Ok(result)
    }

    fn heap_bytes(&self) -> usize {
        self.data.len()
            + self
                .indexed
                .as_ref()
                .map_or(0, |index| index.offsets.len() * 4)
    }
}

struct Column {
    name: String,
    presence: Presence,
    count: usize,
    values: Values,
}

struct Table {
    ids: Box<[u64]>,
    columns: Box<[Column]>,
}

impl Table {
    fn build(rows: &[(u64, &Properties)]) -> Result<Self> {
        if rows.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
            return corrupt();
        }
        let mut logical_bytes = rows
            .len()
            .checked_mul(std::mem::size_of::<Properties>())
            .ok_or(CodecError::Corrupt)?;
        bounded(logical_bytes)?;
        let mut fields: BTreeMap<&str, Vec<(u32, &PropertyValue)>> = BTreeMap::new();
        for (at, (_, values)) in rows.iter().enumerate() {
            logical_bytes = logical_bytes
                .checked_add(properties::validate(values).map_err(|_| CodecError::Corrupt)?)
                .ok_or(CodecError::Corrupt)?;
            bounded(logical_bytes)?;
            for (name, value) in *values {
                fields.entry(name).or_default().push((u32_of(at)?, value));
            }
        }
        let columns = fields
            .into_iter()
            .map(|(name, rows_present)| {
                let ordinals: Vec<_> = rows_present.iter().map(|&(row, _)| row).collect();
                let values: Vec<_> = rows_present.iter().map(|&(_, value)| value).collect();
                Ok(Column {
                    name: name.into(),
                    presence: Presence::build(rows.len(), &ordinals),
                    count: values.len(),
                    values: Values::build(&values)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let result = Self {
            ids: rows
                .iter()
                .map(|&(id, _)| id)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            columns: columns.into_boxed_slice(),
        };
        bounded(result.encode()?.len())?;
        Ok(result)
    }

    fn project(&self, id: u64, projection: &PropertyProjection) -> Result<Option<Properties>> {
        let Ok(at) = self.ids.binary_search(&id) else {
            return Ok(None);
        };
        let mut result = Properties::new();
        let mut add = |column: &Column| -> Result<()> {
            if let Some(rank) = column.presence.rank(u32_of(at)?) {
                result.insert(column.name.clone(), column.values.get(rank)?);
            }
            Ok(())
        };
        match projection {
            PropertyProjection::None => (),
            PropertyProjection::All => {
                for column in &self.columns {
                    add(column)?;
                }
            }
            PropertyProjection::Only(names) => {
                for name in names {
                    if let Ok(at) = self
                        .columns
                        .binary_search_by(|column| column.name.cmp(name))
                    {
                        add(&self.columns[at])?;
                    }
                }
            }
        }
        Ok(Some(result))
    }

    fn encode(&self) -> Result<Vec<u8>> {
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&u32_of(self.ids.len())?.to_le_bytes());
        out.extend_from_slice(&u32_of(self.columns.len())?.to_le_bytes());
        for id in &self.ids {
            out.extend_from_slice(&id.to_le_bytes());
        }
        for column in &self.columns {
            out.extend_from_slice(&(column.name.len() as u16).to_le_bytes());
            out.extend_from_slice(column.name.as_bytes());
            let (mode, presence) = column.presence.encode();
            out.push(mode);
            out.extend_from_slice(&u32_of(column.count)?.to_le_bytes());
            block(&mut out, &presence)?;
            out.extend_from_slice(&[column.values.kind, column.values.width]);
            block(&mut out, &column.values.data)?;
        }
        bounded(out.len())?;
        Ok(out)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        bounded(bytes.len())?;
        let mut cursor = Cursor(bytes);
        if cursor.take(8)? != MAGIC {
            return corrupt();
        }
        let rows = cursor.count()?;
        let columns = cursor.count()?;
        bounded(
            rows.checked_mul(std::mem::size_of::<Properties>())
                .ok_or(CodecError::Corrupt)?,
        )?;
        if rows > cursor.0.len() / 8 {
            return corrupt();
        }
        let ids = (0..rows)
            .map(|_| cursor.array().map(u64::from_le_bytes))
            .collect::<Result<Vec<_>>>()?;
        if ids.windows(2).any(|pair| pair[0] >= pair[1]) || columns > cursor.0.len() / 20 {
            return corrupt();
        }
        let mut result = Vec::new();
        let mut decoded: Vec<_> = (0..rows).map(|_| Properties::new()).collect();
        let mut logical_bytes = rows * std::mem::size_of::<Properties>();
        for _ in 0..columns {
            let len = u16::from_le_bytes(cursor.array()?) as usize;
            let name = std::str::from_utf8(cursor.take(len)?).map_err(|_| CodecError::Corrupt)?;
            properties::validate_field_name(name).map_err(|_| CodecError::Corrupt)?;
            if result
                .last()
                .is_some_and(|previous: &Column| previous.name.as_str() >= name)
            {
                return corrupt();
            }
            let mode = cursor.byte()?;
            let count = cursor.count()?;
            if count == 0 || count > rows {
                return corrupt();
            }
            let presence = Presence::decode(mode, cursor.block()?, rows, count)?;
            let kind = cursor.byte()?;
            let width = cursor.byte()?;
            let values = Values::decode(kind, width, cursor.block()?, count)?;
            // Validate present entries only, never rows * global-name-count.
            let ordinals: Vec<u32> = match &presence {
                Presence::Sparse(rows) => rows.to_vec(),
                Presence::Dense { bits, .. } => bits
                    .iter()
                    .enumerate()
                    .flat_map(|(at, word)| {
                        (0..64).filter_map(move |bit| {
                            (word & (1 << bit) != 0).then_some((at * 64 + bit) as u32)
                        })
                    })
                    .collect(),
            };
            for (rank, row) in ordinals.into_iter().enumerate() {
                let value = values.get(rank)?;
                logical_bytes = logical_bytes
                    .checked_add(name.len() + raw(&value).len() + 2)
                    .ok_or(CodecError::Corrupt)?;
                bounded(logical_bytes)?;
                let target = &mut decoded[row as usize];
                target.insert(name.into(), value);
                properties::validate(target).map_err(|_| CodecError::Corrupt)?;
            }
            result.push(Column {
                name: name.into(),
                presence,
                count,
                values,
            });
        }
        cursor.end()?;
        let table = Self {
            ids: ids.into_boxed_slice(),
            columns: result.into_boxed_slice(),
        };
        let rows: Vec<_> = table.ids.iter().copied().zip(decoded.iter()).collect();
        // Reject non-minimal widths, unused dictionary entries, padding, and
        // noncanonical raw/typed choices through an independent construction pass.
        if Self::build(&rows)?.encode()? != bytes {
            return corrupt();
        }
        Ok(table)
    }

    fn heap_bytes(&self) -> usize {
        self.ids.len() * 8
            + self.columns.len() * std::mem::size_of::<Column>()
            + self
                .columns
                .iter()
                .map(|column| {
                    column.name.len() + column.presence.heap_bytes() + column.values.heap_bytes()
                })
                .sum::<usize>()
    }
}
