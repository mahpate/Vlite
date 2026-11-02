//! Private, in-memory import candidate for the logical import oracle domain.
//! Runs bound sorting scratch, not total memory: input, runs and output coexist.
//! No record lookup, schema admission, clock, filesystem or publication is implied.

use super::*;
use rayon::prelude::*;
use serde_json::Value;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ops::Range;

#[derive(Clone, Copy, Debug)]
enum Layout {
    Balanced,
    FrontHeavy,
    BackHeavy,
}

#[derive(Clone, Copy)]
struct Options {
    partitions: usize,
    run_size: usize,
    layout: Layout,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ImportError {
    kind: &'static str,
    ordinal: usize,
}

type ImportResult<T> = Result<T, ImportError>;

fn failure(kind: &'static str, ordinal: usize) -> ImportError {
    ImportError { kind, ordinal }
}

struct Image {
    graph: CompactGraph,
    bytes: Vec<u8>,
}

struct Node {
    source: SourceNode,
    attachments: Vec<(u16, u64)>,
}

enum Endpoint {
    Key(String),
    Ordinal(u64),
}

struct Edge {
    source: Endpoint,
    target: Endpoint,
    relationship: String,
    deadline: Option<u64>,
}

fn partitions(length: usize, options: Options) -> Vec<Range<usize>> {
    let workers = options.partitions;
    let cut = |index| length / workers * index + (length % workers) * index / workers;
    let mut sizes: Vec<_> = (0..workers)
        .map(|index| match options.layout {
            Layout::Balanced => cut(index + 1) - cut(index),
            _ if index == 0 => length.saturating_sub(workers - 1),
            _ => usize::from(index <= length.min(workers - 1)),
        })
        .collect();
    if matches!(options.layout, Layout::BackHeavy) {
        sizes.reverse();
    }
    let mut start = 0;
    sizes
        .into_iter()
        .map(|size| {
            let range = start..start + size;
            start += size;
            range
        })
        .collect()
}

fn encoding_ranges(length: usize, options: Options) -> Vec<Range<usize>> {
    partitions(length, options)
        .into_iter()
        .flat_map(|range| {
            let end = range.end;
            range
                .step_by(options.run_size)
                .map(move |start| start..start.saturating_add(options.run_size).min(end))
        })
        .collect()
}

fn fields(value: &Value, allowed: &[&str], ordinal: usize) -> ImportResult<()> {
    let object = value
        .as_object()
        .ok_or_else(|| failure("InvalidInput", ordinal))?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(failure("UnsupportedInput", ordinal));
    }
    Ok(())
}

fn text(value: &Value, ordinal: usize) -> ImportResult<String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| failure("InvalidInput", ordinal))
}

fn deadline(row: &Value, ordinal: usize) -> ImportResult<Option<u64>> {
    match row.get("deadline") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| failure("InvalidInput", ordinal)),
    }
}

fn decode_node(row: &Value, ordinal: usize) -> ImportResult<Node> {
    fields(row, &["key", "labels", "deadline", "attachments"], ordinal)?;
    let key = row.get("key").map(|key| text(key, ordinal)).transpose()?;
    let mut labels = row["labels"]
        .as_array()
        .ok_or_else(|| failure("InvalidInput", ordinal))?
        .iter()
        .map(|label| text(label, ordinal))
        .collect::<ImportResult<Vec<_>>>()?;
    labels.sort();
    if key.as_deref().is_some_and(|key| !valid_key(key))
        || labels.len() > 8
        || labels
            .iter()
            .any(|label| label.is_empty() || label.len() > 63)
        || labels.windows(2).any(|pair| pair[0] == pair[1])
    {
        return Err(failure("InvalidInput", ordinal));
    }
    let mut attachments = Vec::new();
    if let Some(items) = row.get("attachments") {
        for item in items
            .as_array()
            .ok_or_else(|| failure("InvalidInput", ordinal))?
        {
            fields(item, &["collection", "record"], ordinal)?;
            let collection = item["collection"]
                .as_u64()
                .and_then(|value| u16::try_from(value).ok())
                .ok_or_else(|| failure("InvalidInput", ordinal))?;
            let record = item["record"]
                .as_u64()
                .ok_or_else(|| failure("InvalidInput", ordinal))?;
            attachments.push((collection, record));
        }
    }
    Ok(Node {
        source: SourceNode {
            id: ordinal as u64,
            key,
            labels,
            revision: 1,
            deadline: deadline(row, ordinal)?,
            properties: Properties::new(),
            attachments: BTreeMap::new(),
        },
        attachments,
    })
}

fn endpoint(value: &Value, ordinal: usize) -> ImportResult<Endpoint> {
    fields(value, &["key", "ordinal"], ordinal)?;
    if value.as_object().unwrap().len() != 1 {
        return Err(failure("InvalidInput", ordinal));
    }
    if let Some(key) = value.get("key") {
        Ok(Endpoint::Key(text(key, ordinal)?))
    } else {
        value["ordinal"]
            .as_u64()
            .map(Endpoint::Ordinal)
            .ok_or_else(|| failure("InvalidInput", ordinal))
    }
}

fn decode_edge(row: &Value, ordinal: usize) -> ImportResult<Edge> {
    fields(
        row,
        &["source", "target", "relationship", "deadline"],
        ordinal,
    )?;
    let relationship = text(&row["relationship"], ordinal)?;
    if relationship.is_empty() || relationship.len() > 63 {
        return Err(failure("InvalidInput", ordinal));
    }
    Ok(Edge {
        source: endpoint(&row["source"], ordinal)?,
        target: endpoint(&row["target"], ordinal)?,
        relationship,
        deadline: deadline(row, ordinal)?,
    })
}

fn resolve(
    endpoint: &Endpoint,
    keys: &BTreeMap<String, u64>,
    count: usize,
    ordinal: usize,
) -> ImportResult<u64> {
    match endpoint {
        Endpoint::Key(key) => keys
            .get(key)
            .copied()
            .ok_or_else(|| failure("UnknownNodeKey", ordinal)),
        Endpoint::Ordinal(id) if *id < count as u64 => Ok(*id),
        Endpoint::Ordinal(_) => Err(failure("NodeNotFound", ordinal)),
    }
}

fn stage(
    nodes: &[Value],
    edges: &[Value],
    options: Options,
    pool: &rayon::ThreadPool,
) -> ImportResult<Image> {
    if options.partitions == 0
        || options.partitions > 256
        || options.run_size == 0
        || nodes.len() > u32::MAX as usize
        || edges.len() > u32::MAX as usize
        || nodes.len().checked_add(edges.len()).is_none()
    {
        return Err(failure("InvalidOptions", 0));
    }
    pool.install(|| {
        let decoded: Vec<_> = partitions(nodes.len(), options)
            .into_par_iter()
            .map(|range| {
                range
                    .map(|ordinal| decode_node(&nodes[ordinal], ordinal))
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut keys = BTreeMap::new();
        let mut owners = BTreeMap::new();
        let mut admitted = Vec::with_capacity(nodes.len());
        // Indexed partition collection preserves input order, including empty
        // partitions. Global admission must precede any edge error.
        for node in decoded.into_iter().flatten() {
            let mut node = node?;
            let ordinal = node.source.id as usize;
            if let Some(key) = &node.source.key
                && keys.insert(key.clone(), node.source.id).is_some()
            {
                return Err(failure("NodeKeyExists", ordinal));
            }
            for (collection, record) in node.attachments {
                if node.source.attachments.contains_key(&collection) {
                    return Err(failure("NodeAlreadyAttached", ordinal));
                }
                if owners
                    .insert((collection, record), node.source.id)
                    .is_some()
                {
                    return Err(failure("RecordAlreadyAttached", ordinal));
                }
                node.source.attachments.insert(collection, record);
            }
            admitted.push(node.source);
        }
        let decoded: Vec<_> = partitions(edges.len(), options)
            .into_par_iter()
            .map(|range| {
                range
                    .map(|id| {
                        let ordinal = nodes.len() + id;
                        let edge = decode_edge(&edges[id], ordinal)?;
                        Ok(SourceEdge {
                            id: id as u64,
                            source: resolve(&edge.source, &keys, nodes.len(), ordinal)?,
                            target: resolve(&edge.target, &keys, nodes.len(), ordinal)?,
                            relationship: edge.relationship,
                            key: None,
                            revision: 1,
                            deadline: edge.deadline,
                            properties: Properties::new(),
                            segments: None,
                        })
                    })
                    .collect::<Vec<ImportResult<_>>>()
            })
            .collect();
        let edges = decoded
            .into_iter()
            .flatten()
            .collect::<ImportResult<Vec<_>>>()?;
        let source = GraphSource {
            graph_id: 0,
            name: "import".into(),
            sequence: 1,
            next_node_id: nodes.len() as u64,
            next_edge_id: edges.len() as u64,
            temporal_enabled: false,
            schema: GraphSchema::default(),
            nodes: admitted,
            edges,
        };
        let labels: Vec<_> = source
            .nodes
            .par_iter()
            .flat_map_iter(|node| node.labels.iter().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let relationships: Vec<_> = source
            .edges
            .par_iter()
            .map(|edge| edge.relationship.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let edge_rows: Vec<_> = source
            .edges
            .par_iter()
            .map(|edge| EdgeRow {
                id: edge.id,
                source: edge.source as u32,
                target: edge.target as u32,
                relationship: relationships.binary_search(&edge.relationship).unwrap() as u32,
            })
            .collect();
        let (out, incoming) = rayon::join(
            || adjacency(&edge_rows, nodes.len(), options, false),
            || adjacency(&edge_rows, nodes.len(), options, true),
        );
        let mut label_postings = vec![Vec::new(); labels.len()];
        for node in &source.nodes {
            for label in &node.labels {
                label_postings[labels.binary_search(label).unwrap()].push(node.id as u32);
            }
        }
        let node_ordinals = source
            .nodes
            .iter()
            .map(|node| (node.id, node.id as u32))
            .collect();
        let graph = CompactGraph {
            source,
            labels,
            relationships,
            property_names: Vec::new(),
            label_postings,
            attachment_owners: owners,
            node_ordinals,
            edge_rows,
            out,
            incoming,
        };
        let bytes = encode_blocks(&graph, options).map_err(|_| failure("Capacity", 0))?;
        Ok(Image { graph, bytes })
    })
}

type Membership = (u32, u32, u64, u32);

fn adjacency(rows: &[EdgeRow], nodes: usize, options: Options, incoming: bool) -> Csr {
    let runs: Vec<Vec<Membership>> = partitions(rows.len(), options)
        .into_par_iter()
        .flat_map_iter(|range| {
            let mut runs = Vec::new();
            let mut buffer = Vec::new();
            for index in range {
                let row = &rows[index];
                buffer.push((
                    if incoming { row.target } else { row.source },
                    row.relationship,
                    row.id,
                    index as u32,
                ));
                if buffer.len() == options.run_size {
                    buffer.sort_unstable();
                    runs.push(std::mem::take(&mut buffer));
                }
            }
            if !buffer.is_empty() {
                buffer.sort_unstable();
                runs.push(buffer);
            }
            runs
        })
        .collect();
    let mut heap = BinaryHeap::new();
    for (run, values) in runs.iter().enumerate() {
        heap.push(Reverse((values[0], run, 0usize)));
    }
    let mut degrees = vec![0u32; nodes];
    let mut edges = Vec::with_capacity(rows.len());
    while let Some(Reverse((membership, run, index))) = heap.pop() {
        degrees[membership.0 as usize] += 1;
        edges.push(membership.3);
        if let Some(next) = runs[run].get(index + 1) {
            heap.push(Reverse((*next, run, index + 1)));
        }
    }
    let mut offsets = Vec::with_capacity(nodes + 1);
    offsets.push(0);
    for degree in degrees {
        offsets.push(offsets.last().unwrap() + degree);
    }
    Csr { offsets, edges }
}

// Each worker owns a complete byte block. Only checked concatenation mutates
// the final buffer; no worker can observe or publish a partial image.
fn encode_blocks(graph: &CompactGraph, options: Options) -> Result<Vec<u8>, CodecError> {
    let mut out = Vec::new();
    put_bytes(&mut out, MAGIC)?;
    put_u16(&mut out, graph.source.graph_id)?;
    put_string(&mut out, &graph.source.name)?;
    put_u64(&mut out, graph.source.sequence)?;
    put_u64(&mut out, graph.source.next_node_id)?;
    put_u64(&mut out, graph.source.next_edge_id)?;
    put_schema(&mut out, &graph.source.schema)?;
    put_strings(&mut out, &graph.labels)?;
    put_strings(&mut out, &graph.relationships)?;
    put_strings(&mut out, &graph.property_names)?;
    put_count(&mut out, graph.source.nodes.len())?;
    let blocks: Vec<Result<Vec<u8>, CodecError>> =
        encoding_ranges(graph.source.nodes.len(), options)
            .into_par_iter()
            .map(|range| {
                let mut bytes = Vec::new();
                for index in range {
                    let node = &graph.source.nodes[index];
                    put_u64(&mut bytes, node.id)?;
                    put_optional_string(&mut bytes, node.key.as_deref())?;
                    put_u64(&mut bytes, node.revision)?;
                    put_optional_u64(&mut bytes, node.deadline)?;
                    put_count(&mut bytes, node.labels.len())?;
                    for label in &node.labels {
                        put_count(&mut bytes, graph.labels.binary_search(label).unwrap())?;
                    }
                    put_properties(&mut bytes, &node.properties, &graph.property_names)?;
                }
                Ok(bytes)
            })
            .collect();
    for block in blocks {
        put_bytes(&mut out, &block?)?;
    }
    put_count(&mut out, graph.source.edges.len())?;
    let blocks: Vec<Result<Vec<u8>, CodecError>> =
        encoding_ranges(graph.source.edges.len(), options)
            .into_par_iter()
            .map(|range| {
                let mut bytes = Vec::new();
                for index in range {
                    let edge = &graph.source.edges[index];
                    let row = &graph.edge_rows[index];
                    put_u64(&mut bytes, edge.id)?;
                    put_u32(&mut bytes, row.source)?;
                    put_u32(&mut bytes, row.target)?;
                    put_u32(&mut bytes, row.relationship)?;
                    put_optional_string(&mut bytes, None)?;
                    put_u64(&mut bytes, edge.revision)?;
                    put_optional_u64(&mut bytes, edge.deadline)?;
                    put_properties(&mut bytes, &edge.properties, &graph.property_names)?;
                }
                Ok(bytes)
            })
            .collect();
    for block in blocks {
        put_bytes(&mut out, &block?)?;
    }
    put_csr(&mut out, &graph.out)?;
    put_csr(&mut out, &graph.incoming)?;
    put_count(&mut out, graph.label_postings.len())?;
    for (code, postings) in graph.label_postings.iter().enumerate() {
        put_count(&mut out, code)?;
        put_count(&mut out, postings.len())?;
        for ordinal in postings {
            put_u32(&mut out, *ordinal)?;
        }
    }
    put_count(&mut out, graph.attachment_owners.len())?;
    for node in &graph.source.nodes {
        for (collection, record) in &node.attachments {
            put_u32(&mut out, node.id as u32)?;
            put_u16(&mut out, *collection)?;
            put_u64(&mut out, *record)?;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
