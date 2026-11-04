#![cfg(feature = "grpc")]
//! Frozen pre-provider wire shapes for the three additively extended messages.
use prost::Message;
use vlite::server::grpc::proto;

#[derive(Clone, PartialEq, Message)]
struct OldSearch {
    #[prost(string, tag = "1")]
    collection: String,
    #[prost(float, repeated, tag = "2")]
    vector: Vec<f32>,
    #[prost(uint32, tag = "3")]
    k: u32,
    #[prost(int32, tag = "15")]
    rerank_mode: i32,
    #[prost(double, optional, tag = "16")]
    mmr_lambda: Option<f64>,
    #[prost(uint32, optional, tag = "17")]
    rerank_candidates: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
struct OldHit {
    #[prost(uint64, tag = "1")]
    id: u64,
    #[prost(double, tag = "2")]
    distance: f64,
}

#[derive(Clone, PartialEq, Message)]
struct OldResponse {
    #[prost(message, repeated, tag = "1")]
    hits: Vec<OldHit>,
    #[prost(message, repeated, tag = "2")]
    hybrid_hits: Vec<proto::HybridHit>,
}

#[test]
fn old_wire_bytes_decode_unchanged_and_new_optional_fields_are_absent() {
    let old = OldSearch {
        collection: "docs".into(),
        vector: vec![0.1, 0.2, 0.3],
        k: 1,
        rerank_mode: 2,
        mmr_lambda: Some(0.5),
        rerank_candidates: Some(50),
    };
    let bytes = old.encode_to_vec();
    let new = proto::SearchRequest::decode(bytes.as_slice()).unwrap();
    assert_eq!(new.collection, old.collection);
    assert_eq!(new.vector, old.vector);
    assert_eq!(new.k, 1);
    assert_eq!(new.rerank_mode, proto::RerankMode::Mmr as i32);
    assert_eq!(new.mmr_lambda, Some(0.5));
    assert_eq!(new.rerank_candidates, Some(50));
    assert_eq!(new.rerank_provider, None);
    assert_eq!(new.encode_to_vec(), bytes);
    let old = OldResponse {
        hits: vec![OldHit {
            id: u64::MAX,
            distance: 0.4,
        }],
        hybrid_hits: vec![],
    };
    let bytes = old.encode_to_vec();
    let new = proto::SearchResponse::decode(bytes.as_slice()).unwrap();
    assert_eq!(new.hits[0].id, u64::MAX);
    assert_eq!(new.hits[0].distance, 0.4);
    assert_eq!(new.hits[0].rerank_score, None);
    assert_eq!(new.ranking, None);
    assert_eq!(new.encode_to_vec(), bytes);
}

#[test]
fn old_reader_ignores_ranking_and_preserves_original_distance() {
    let new = proto::SearchResponse {
        hits: vec![proto::Hit {
            id: 7,
            distance: 0.4,
            rerank_score: Some(0.8),
        }],
        ranking: Some(proto::Ranking {
            provider: "rank-a".into(),
            order: "score_desc_id_asc".into(),
        }),
        ..Default::default()
    };
    let old = OldResponse::decode(new.encode_to_vec().as_slice()).unwrap();
    assert_eq!(
        old.hits,
        vec![OldHit {
            id: 7,
            distance: 0.4
        }]
    );
    assert!(old.hybrid_hits.is_empty());
}

#[test]
fn additive_tags_and_enum_discriminants_are_exact() {
    assert_eq!(
        (
            proto::RerankMode::None as i32,
            proto::RerankMode::Exact as i32,
            proto::RerankMode::Mmr as i32,
            proto::RerankMode::Custom as i32
        ),
        (0, 1, 2, 3)
    );
    let request = proto::SearchRequest {
        rerank_provider: Some("a".into()),
        ..Default::default()
    };
    assert_eq!(request.encode_to_vec(), vec![0x92, 0x01, 1, b'a']);
    let ranking = proto::Ranking {
        provider: "a".into(),
        order: "b".into(),
    };
    assert_eq!(ranking.encode_to_vec(), vec![0x0a, 1, b'a', 0x12, 1, b'b']);
    let hit = proto::Hit {
        rerank_score: Some(0.0),
        ..Default::default()
    };
    assert_eq!(hit.encode_to_vec(), vec![0x19, 0, 0, 0, 0, 0, 0, 0, 0]);
    let response = proto::SearchResponse {
        ranking: Some(proto::Ranking::default()),
        ..Default::default()
    };
    assert_eq!(response.encode_to_vec(), vec![0x1a, 0]);
    let embed = proto::EmbedTextRequest {
        collection: "a".into(),
        text: "b".into(),
        provider: "c".into(),
    };
    assert_eq!(
        embed.encode_to_vec(),
        vec![0x0a, 1, b'a', 0x12, 1, b'b', 0x1a, 1, b'c']
    );
    let embed = proto::EmbedTextResponse {
        vector: vec![1.0],
        dimensions: 1,
        provider: "c".into(),
    };
    assert_eq!(
        embed.encode_to_vec(),
        vec![0x0a, 4, 0, 0, 0x80, 0x3f, 0x10, 1, 0x1a, 1, b'c']
    );
}
