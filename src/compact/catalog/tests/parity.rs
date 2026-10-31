//! One image check for every reader (review 9ebcbeff, MEDIUM): full
//! replay, the change feed and log export each refuse a complete,
//! checksummed precision image, plaintext or authenticated, whose catalog
//! dimensions, import bindings or graph revision coverage contradict the
//! frames it restates, before they return any change or segment.

use super::*;

/// `image` (complete and plaintext, at `floor`) and then an op-17 create
/// at `floor + 1`, which a reader that accepts the image returns.
fn with_create(image: Vec<u8>, floor: u64) -> Vec<u8> {
    let mut log = image;
    log.extend(format::precise_create_frame(
        floor + 1,
        9,
        "later",
        2,
        &CollectionSettings::default(),
        F32,
    ));
    log
}

/// The plaintext `log` with every frame sealed under `key()`, as an
/// encrypted store writes it. The frame prefixes, and so the image end
/// digest, are unchanged.
fn encrypted(log: &[u8]) -> Vec<u8> {
    let (header, frames) = split(log);
    let cipher = Cipher::create(&key()).unwrap();
    let mut bytes = format::header_v5_flags(
        u32_at(&header, 12) as usize,
        u64_at(&header, 16),
        u16_at(&header, 24),
        u16_at(&header, 26) | FLAG_ENCRYPTED,
        Some(&cipher),
    );
    for frame in &frames {
        bytes.extend(cipher.seal_frame(frame).unwrap());
    }
    bytes
}

/// Outcomes of full replay, the change feed and log export of `log` from
/// `floor`. A reader that accepts it returns exactly the suffix create.
fn readers(log: &[u8], key: Option<&EncryptionKey>, floor: u64) -> [String; 3] {
    let cursor = FeedCursor {
        lineage: LINEAGE,
        sequence: floor,
    };
    let feed = crate::changes::read_changes(&mut Cursor::new(log), key, None, &cursor, 16)
        .map(|(batch, _)| assert_eq!(batch.changes.len(), 1));
    let export = logops::read_segment(&mut Cursor::new(log), key, None, floor, 1 << 20)
        .map(|(segment, _)| assert_eq!(segment.last_sequence, floor + 1));
    [outcome(load(log, key)), outcome(feed), outcome(export)]
}

/// The readers' outcomes for `image` followed by a create, plaintext and
/// then encrypted.
fn parity(image: Vec<u8>, floor: u64) -> [[String; 3]; 2] {
    let log = with_create(image, floor);
    [
        readers(&log, None, floor),
        readers(&encrypted(&log), Some(&key()), floor),
    ]
}

fn all(outcome: &str) -> [[String; 3]; 2] {
    std::array::from_fn(|_| std::array::from_fn(|_| outcome.to_owned()))
}

#[test]
fn readers_check_catalog_dimensions_alike() {
    let _gate = Activation::on();
    let image = |dimensions| {
        replaced(
            &initial(None),
            KIND_CATALOG,
            1,
            vec![catalog_frame(1, &entries(&[(0, F32, dimensions)]))],
        )
    };
    assert_eq!(parity(image(2), 1), all("ok"));
    // The header gives `default` two dimensions.
    assert_eq!(parity(image(3), 1), all("corrupt"));
    let (header, frames) = named_image();
    let catalog = |list: &[(u16, Precision, usize)]| {
        let mut frames = frames.clone();
        frames[3] = catalog_frame(2, &entries(list));
        parity(sealed(&header, &frames), 2)
    };
    assert_eq!(catalog(&[(0, F32, 2), (1, F16, 3)]), all("ok"));
    // `half` was restated with three.
    assert_eq!(catalog(&[(0, F32, 2), (1, F16, 4)]), all("corrupt"));
}

#[test]
fn readers_check_import_bindings_alike() {
    let _gate = Activation::on();
    let (header, frames) = named_image();
    let bound = |collection: u16, dropped: u8, state: u8, name: &str| {
        let section = binding_section(collection, dropped, state, name);
        let mut frames = frames.clone();
        frames[4] = block_frame(2, KIND_IMPORTS, 0, 0, 1, &[&section]);
        parity(sealed(&header, &frames), 2)
    };
    assert_eq!(bound(1, 0, 0, "half"), all("ok"));
    assert_eq!(bound(0, 0, 1, "default"), all("ok"));
    // A live binding names its live collection; a dropped one is an
    // assigned ID that no live collection holds.
    for (collection, dropped, state, name) in [
        (1, 0, 0, "other"),
        (5, 0, 0, "half"),
        (1, 1, 1, "half"),
        (2, 1, 1, "gone"),
        (0, 1, 1, "default"),
    ] {
        assert_eq!(
            bound(collection, dropped, state, name),
            all("corrupt"),
            "{collection} {dropped} {state} {name}"
        );
    }
}

#[test]
fn readers_check_graph_revision_coverage_alike() {
    let _gate = Activation::on();
    let mut db = Database::open_from_bytes(image_of(&initial(None)), None).unwrap();
    for name in ["g", "h"] {
        db.create_graph(name).unwrap();
        let mut graph = db.graph_mut(name).unwrap();
        let nodes: Vec<_> = ["a", "b", "c"]
            .map(|key| GraphOp::CreateNode {
                key: key.into(),
                labels: Vec::new(),
                properties: Metadata::new(),
            })
            .into();
        graph.write_batch(&nodes).unwrap();
        graph
            .create_edge(Some("e"), "a", "knows", "b", &Metadata::new())
            .unwrap();
    }
    let loaded = load(&db.to_bytes().unwrap(), None).unwrap();
    let floor = loaded.sequence;
    let (header, mut frames) = split(&image_of(&loaded));
    frames.pop();
    let runs: Vec<usize> = (0..frames.len())
        .filter(|&index| is_block(&frames[index], KIND_REVISIONS))
        .collect();
    assert_eq!(runs.len(), 2);
    let [first, last] = [runs[0], runs[1]];
    assert_eq!(frames[last - 1][4], 15);
    let owner = u16_at(&frames[last], 5);
    let parts = sections(&frames[last]);
    let (nodes, edges) =
        decode_revisions(&[&parts[0], &parts[1]], u32_at(&frames[last], 24), floor, 0).unwrap();
    assert_eq!((nodes.len(), edges.len()), (3, 1));
    let rerun = |owner: u16, nodes: Revs, edges: Revs| {
        let mut run = Vec::new();
        revision_frames(floor, owner, nodes, edges, |frame| {
            run.push(frame.to_vec());
            Ok(())
        })
        .unwrap();
        run
    };
    let with_run = |at: usize, run: Vec<Vec<u8>>| {
        let mut frames = frames.clone();
        frames.splice(at..=at, run);
        parity(sealed(&header, &frames), floor)
    };
    assert_eq!(with_run(last, rerun(owner, &nodes, &edges)), all("ok"));
    let mut shifted = nodes.clone();
    shifted[2].0 += 1;
    let mut extra = nodes.clone();
    extra.push((nodes[2].0 + 1, 1));
    for (name, run) in [
        ("a node missing", rerun(owner, &nodes[..2], &edges)),
        ("an edge missing", rerun(owner, &nodes, &[])),
        ("an extra node", rerun(owner, &extra, &edges)),
        ("another node", rerun(owner, &shifted, &edges)),
        ("another graph", rerun(owner + 1, &nodes, &edges)),
        ("no run", Vec::new()),
    ] {
        assert_eq!(with_run(last, run), all("corrupt"), "{name}");
    }
    // The first graph's snapshot without its run, which the second graph's
    // run still declares in the table.
    assert_eq!(with_run(first, Vec::new()), all("corrupt"));
}
