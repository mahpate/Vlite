use super::*;

#[test]
fn collector_preserves_multiple_answers_order_duplicates_and_ports() {
    let answers = vec![
        "127.0.0.2:4002".parse().unwrap(),
        "127.0.0.1:4001".parse().unwrap(),
        "127.0.0.2:4002".parse().unwrap(),
        "[::1]:4003".parse().unwrap(),
    ];
    assert_eq!(
        collect_resolved_addresses(Ok(answers.clone().into_iter())).unwrap(),
        answers
    );
}

#[test]
fn collector_retains_disallowed_second_answer_for_validation() {
    let answers = [
        "127.0.0.1:4001".parse().unwrap(),
        "192.0.2.1:4002".parse().unwrap(),
    ];
    let collected = collect_resolved_addresses(Ok(answers.into_iter())).unwrap();
    assert_eq!(
        check_addresses(&collected, &["127.0.0.1".parse().unwrap()]),
        Err(ProviderError::Unavailable)
    );
}

#[test]
fn collector_keeps_64_accepted_and_65_rejected_answers() {
    let address = "127.0.0.1:4001".parse().unwrap();
    for count in [64, 65] {
        let answers = vec![address; count];
        let collected = collect_resolved_addresses(Ok(answers.clone().into_iter())).unwrap();
        assert_eq!(collected, answers, "collector must retain {count} answers");
        let checked = check_addresses(&collected, &["127.0.0.1".parse().unwrap()]);
        if count == 64 {
            assert_eq!(checked, Ok(answers));
        } else {
            assert_eq!(checked, Err(ProviderError::Unavailable));
        }
    }
}

#[test]
fn collector_consumes_at_most_65_items_from_a_longer_finite_iterator() {
    let consumed = AtomicUsize::new(0);
    let answers = (0..100).map(|index| {
        consumed.fetch_add(1, Ordering::SeqCst);
        SocketAddr::from(([127, 0, 0, 1], 4000 + index))
    });
    let collected = collect_resolved_addresses(Ok(answers)).unwrap();
    assert_eq!(consumed.load(Ordering::SeqCst), 65);
    assert_eq!(collected.len(), 65);
    assert_eq!(collected.first().unwrap().port(), 4000);
    assert_eq!(collected.last().unwrap().port(), 4064);
}

#[test]
fn collector_maps_io_errors_to_sanitized_unavailable() {
    for kind in [
        std::io::ErrorKind::NotFound,
        std::io::ErrorKind::PermissionDenied,
        std::io::ErrorKind::TimedOut,
        std::io::ErrorKind::Other,
    ] {
        let error = std::io::Error::new(kind, "fake-private-dns-detail");
        let result = collect_resolved_addresses(Err::<std::vec::IntoIter<SocketAddr>, _>(error));
        assert_eq!(result, Err(ProviderError::Unavailable));
        assert_eq!(result.unwrap_err().to_string(), "provider_unavailable");
    }
}
