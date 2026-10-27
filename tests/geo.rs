use tempfile::tempdir;
use vlite::{
    Database, DistanceMetric, FormatVersion, GeoSearchQuery, GeoSort, Metadata, MetadataValue,
};

fn geo(lat: f64, lon: f64) -> Metadata {
    Metadata::from([(
        "location".into(),
        MetadataValue::GeoPoint {
            latitude: lat,
            longitude: lon,
        },
    )])
}

#[test]
fn geo_radius_filter_and_geo_distance_ranking_are_deterministic() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("geo.vlite");
    let mut db = Database::create_with_format(&path, 1, FormatVersion::V5).unwrap();
    db.put_with_metadata(1, &[0.0], b"", &geo(0.0, 0.0))
        .unwrap();
    db.put_with_metadata(2, &[0.0], b"", &geo(0.0, 0.01))
        .unwrap();
    db.put_with_metadata(3, &[0.0], b"", &geo(0.0, 1.0))
        .unwrap();
    let nearby = db
        .search_geo(&GeoSearchQuery {
            vector: vec![0.0],
            k: 10,
            field: "location".into(),
            latitude: 0.0,
            longitude: 0.0,
            radius_meters: 2_000.0,
            conditions: vec![],
            metric: DistanceMetric::SquaredL2,
            sort: GeoSort::GeoDistance,
        })
        .unwrap();
    assert_eq!(nearby.iter().map(|hit| hit.id).collect::<Vec<_>>(), [1, 2]);
    assert!(nearby[1].geo_distance_meters > 1_000.0 && nearby[1].geo_distance_meters < 1_200.0);
    let vector_sorted = db
        .search_geo(&GeoSearchQuery {
            vector: vec![0.0],
            k: 1,
            field: "location".into(),
            latitude: 0.0,
            longitude: 0.0,
            radius_meters: 2_000.0,
            conditions: vec![],
            metric: DistanceMetric::SquaredL2,
            sort: GeoSort::VectorDistance,
        })
        .unwrap();
    assert_eq!(vector_sorted[0].id, 1);
    db.close().unwrap();
    let reopened = Database::open(path).unwrap();
    assert_eq!(
        reopened.get(2).unwrap().unwrap().metadata["location"],
        geo(0.0, 0.01)["location"]
    );
}

#[test]
fn geo_values_require_v5_and_invalid_coordinates_or_radius_are_rejected() {
    let directory = tempdir().unwrap();
    let mut old =
        Database::create_with_format(directory.path().join("geo-old.vlite"), 1, FormatVersion::V3)
            .unwrap();
    assert!(
        old.put_with_metadata(1, &[0.0], b"", &geo(0.0, 0.0))
            .is_err()
    );
    let mut db = Database::create_with_format(
        directory.path().join("geo-invalid.vlite"),
        1,
        FormatVersion::V5,
    )
    .unwrap();
    let invalid = Metadata::from([(
        "location".into(),
        MetadataValue::GeoPoint {
            latitude: 91.0,
            longitude: 0.0,
        },
    )]);
    assert!(db.put_with_metadata(1, &[0.0], b"", &invalid).is_err());
    assert!(
        db.search_geo(&GeoSearchQuery {
            vector: vec![0.0],
            k: 1,
            field: "location".into(),
            latitude: 0.0,
            longitude: 0.0,
            radius_meters: f64::NAN,
            conditions: vec![],
            metric: DistanceMetric::SquaredL2,
            sort: GeoSort::GeoDistance,
        })
        .is_err()
    );
}
