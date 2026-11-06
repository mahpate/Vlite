//! KV wire conversion only; REST owns validation, pagination and mutation policy.
use super::*;
use crate::server::kv as rest;
use axum::extract::RawQuery;
use serde::de::DeserializeOwned;

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value).map_err(internal)
}

fn collection(name: String) -> Result<Path<String>, ApiError> {
    if !crate::format::valid_collection_name(&name) {
        return Err(unprocessable("invalid collection name"));
    }
    Ok(Path(name))
}

fn address(address: Option<proto::KvAddress>) -> Result<proto::KvAddress, ApiError> {
    address.ok_or_else(|| unprocessable("KV address is required"))
}

fn expectation(value: Option<proto::Expected>) -> Value {
    match value.and_then(|value| value.kind) {
        None | Some(proto::expected::Kind::Any(_)) => json!("any"),
        Some(proto::expected::Kind::Absent(_)) => json!("absent"),
        Some(proto::expected::Kind::Revision(revision)) => {
            json!({"revision":revision.to_string()})
        }
    }
}

#[derive(Deserialize)]
struct Entry {
    key: String,
    value_base64: String,
    version: String,
    expires_at_ms: Option<String>,
}

fn entry(value: Value) -> Result<proto::KvEntry, ApiError> {
    let value: Entry = decode(value)?;
    Ok(proto::KvEntry {
        key: value.key,
        value: BASE64_STANDARD
            .decode(value.value_base64)
            .map_err(internal)?,
        version: value.version.parse().map_err(internal)?,
        expires_at_ms: value
            .expires_at_ms
            .map(|value| value.parse())
            .transpose()
            .map_err(internal)?,
    })
}

#[tonic::async_trait]
impl proto::kv_server::Kv for Adapter {
    async fn get_kv_entry(
        &self,
        request: Request<proto::KvGetRequest>,
    ) -> Result<Response<proto::KvEntry>, Status> {
        self.call(
            "grpc:/vlite.v1.Kv/GetKvEntry",
            request,
            Need::Op(Op::Read, Reach::Collection),
            true,
            false,
            |service, role, _, request| async move {
                let address = address(request.address)?;
                let target = collection(address.collection)?;
                let input = decode(json!({"namespace":address.namespace,"key":address.key}))?;
                let Json(value) = rest::fetch(
                    State(service),
                    Extension(role),
                    Ok(target),
                    RawQuery(None),
                    Ok(Query(input)),
                )
                .await?;
                entry(value)
            },
        )
        .await
    }

    async fn put_kv_entry(
        &self,
        request: Request<proto::KvPutRequest>,
    ) -> Result<Response<proto::KvPutResponse>, Status> {
        self.call(
            "grpc:/vlite.v1.Kv/PutKvEntry",
            request,
            Need::Op(Op::Write, Reach::Collection),
            false,
            false,
            |service, role, request_id, request| async move {
                if request.value.len() > crate::kv::MAX_KV_VALUE_BYTES {
                    return Err(unprocessable("KV value exceeds 1047290 decoded bytes"));
                }
                let address = address(request.address)?;
                let target = collection(address.collection)?;
                let input = decode(json!({
                    "namespace":address.namespace,"key":address.key,
                    "value_base64":BASE64_STANDARD.encode(request.value),
                    "expected":expectation(request.expected),
                    "expires_at_ms":request.expires_at_ms.map(|value|value.to_string())
                }))?;
                let Json(value) = rest::save(
                    State(service),
                    Extension(role),
                    Extension(RequestId(request_id)),
                    None,
                    Ok(target),
                    RawQuery(None),
                    Ok(Query(rest::NoQuery {})),
                    Ok(Json(input)),
                )
                .await?;
                #[derive(Deserialize)]
                struct Put {
                    version: String,
                    expires_at_ms: Option<String>,
                }

                let value: Put = decode(value)?;
                Ok(proto::KvPutResponse {
                    version: value.version.parse().map_err(internal)?,
                    expires_at_ms: value
                        .expires_at_ms
                        .map(|value| value.parse())
                        .transpose()
                        .map_err(internal)?,
                })
            },
        )
        .await
    }

    async fn delete_kv_entry(
        &self,
        request: Request<proto::KvDeleteRequest>,
    ) -> Result<Response<proto::KvDeleteResponse>, Status> {
        self.call(
            "grpc:/vlite.v1.Kv/DeleteKvEntry",
            request,
            Need::Op(Op::Write, Reach::Collection),
            false,
            false,
            |service, role, request_id, request| async move {
                let address = address(request.address)?;
                let target = collection(address.collection)?;
                let expected = match request.expected.and_then(|value| value.kind) {
                    None | Some(proto::expected::Kind::Any(_)) => "any".into(),
                    Some(proto::expected::Kind::Absent(_)) => "absent".into(),
                    Some(proto::expected::Kind::Revision(revision)) => revision.to_string(),
                };
                let input = decode(json!({
                    "namespace":address.namespace,"key":address.key,"expected":expected
                }))?;
                let Json(value) = rest::remove(
                    State(service),
                    Extension(role),
                    Extension(RequestId(request_id)),
                    None,
                    Ok(target),
                    RawQuery(None),
                    Ok(Query(input)),
                )
                .await?;
                #[derive(Deserialize)]
                struct Delete {
                    deleted: bool,
                }
                let value: Delete = decode(value)?;
                Ok(proto::KvDeleteResponse {
                    deleted: value.deleted,
                })
            },
        )
        .await
    }

    async fn list_kv_entries(
        &self,
        request: Request<proto::KvListRequest>,
    ) -> Result<Response<proto::KvListResponse>, Status> {
        self.call(
            "grpc:/vlite.v1.Kv/ListKvEntries",
            request,
            Need::Op(Op::Read, Reach::Collection),
            true,
            false,
            |service, role, _, request| async move {
                let target = collection(request.collection)?;
                let input = decode(json!({
                    "namespace":request.namespace,"prefix":request.prefix,
                    "after":request.after,"limit":request.limit.map(|value|value.to_string())
                }))?;
                let Json(value) = rest::list(
                    State(service),
                    Extension(role),
                    Ok(target),
                    RawQuery(None),
                    Ok(Query(input)),
                )
                .await?;
                #[derive(Deserialize)]
                struct Page {
                    items: Vec<Value>,
                    next_after: Option<String>,
                }
                let value: Page = decode(value)?;
                Ok(proto::KvListResponse {
                    items: value
                        .items
                        .into_iter()
                        .map(entry)
                        .collect::<Result<_, _>>()?,
                    next_after: value.next_after,
                })
            },
        )
        .await
    }

    async fn get_kv_entries(
        &self,
        request: Request<proto::KvGetManyRequest>,
    ) -> Result<Response<proto::KvGetManyResponse>, Status> {
        self.call(
            "grpc:/vlite.v1.Kv/GetKvEntries",
            request,
            Need::Op(Op::Read, Reach::Collection),
            true,
            false,
            |service, role, _, request| async move {
                let target = collection(request.collection)?;
                rest::check_key_count(request.keys.len())?;
                let keys: Vec<Value> = request
                    .keys
                    .into_iter()
                    .map(|key| json!({"namespace":key.namespace,"key":key.key}))
                    .collect();
                let input = decode(json!({"keys":keys}))?;
                let Json(value) = rest::fetch_many(
                    State(service),
                    Extension(role),
                    Ok(target),
                    RawQuery(None),
                    Ok(Query(rest::NoQuery {})),
                    Ok(Json(input)),
                )
                .await?;
                #[derive(Deserialize)]
                struct Many {
                    items: Vec<Option<Value>>,
                    next_index: Option<u32>,
                }
                let value: Many = decode(value)?;
                Ok(proto::KvGetManyResponse {
                    items: value
                        .items
                        .into_iter()
                        .map(|item| {
                            Ok(proto::KvGetManyItem {
                                entry: item.map(entry).transpose()?,
                            })
                        })
                        .collect::<Result<_, ApiError>>()?,
                    next_index: value.next_index,
                })
            },
        )
        .await
    }

    async fn write_kv_batch(
        &self,
        request: Request<proto::KvBatchRequest>,
    ) -> Result<Response<proto::KvBatchResponse>, Status> {
        self.call(
            "grpc:/vlite.v1.Kv/WriteKvBatch",
            request,
            Need::Op(Op::Write, Reach::Collection),
            false,
            false,
            |service, role, request_id, request| async move {
                let target = collection(request.collection)?;
                rest::check_operation_count(request.operations.len())?;
                let operations = request
                    .operations
                    .into_iter()
                    .enumerate()
                    .map(|(index, operation)| {
                        batch_operation(operation)
                            .map_err(|error| rest::at("operations", index, error))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let input = decode(json!({"operations":operations}))?;
                let Json(value) = rest::batch(
                    State(service),
                    Extension(role),
                    Extension(RequestId(request_id)),
                    None,
                    Ok(target),
                    RawQuery(None),
                    Ok(Query(rest::NoQuery {})),
                    Ok(Json(input)),
                )
                .await?;
                #[derive(Deserialize)]
                struct Results {
                    results: Vec<Outcome>,
                }
                #[derive(Deserialize)]
                struct Outcome {
                    version: Option<String>,
                    deleted: Option<bool>,
                }
                let value: Results = decode(value)?;
                Ok(proto::KvBatchResponse {
                    results: value
                        .results
                        .into_iter()
                        .map(|outcome| {
                            let kind = match (outcome.version, outcome.deleted) {
                                (Some(version), None) => proto::kv_batch_result::Kind::Version(
                                    version.parse().map_err(internal)?,
                                ),
                                (None, Some(deleted)) => {
                                    proto::kv_batch_result::Kind::Deleted(deleted)
                                }
                                _ => return Err(internal("malformed KV batch result")),
                            };
                            Ok(proto::KvBatchResult { kind: Some(kind) })
                        })
                        .collect::<Result<_, ApiError>>()?,
                })
            },
        )
        .await
    }
}

/// One batch operation as the REST request carries it; REST checks it. Every
/// field of the message is named, so a field that a later release adds fails
/// the build here instead of being dropped.
fn batch_operation(operation: proto::KvBatchOperation) -> Result<Value, ApiError> {
    let proto::KvBatchOperation { kind } = operation;
    match kind {
        Some(proto::kv_batch_operation::Kind::Put(put)) => {
            let proto::KvBatchPut {
                namespace,
                key,
                value,
                expected,
            } = put;
            Ok(json!({
                "op":"put","namespace":namespace,"key":key,
                "value_base64":BASE64_STANDARD.encode(value),
                "expected":expectation(expected)
            }))
        }
        Some(proto::kv_batch_operation::Kind::Delete(delete)) => {
            let proto::KvBatchDelete {
                namespace,
                key,
                expected,
            } = delete;
            Ok(json!({
                "op":"delete","namespace":namespace,"key":key,
                "expected":expectation(expected)
            }))
        }
        None => Err(unprocessable("KV batch operation is required")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use proto::kv_server::Kv;
    use tower::ServiceExt;

    fn address() -> Option<proto::KvAddress> {
        Some(proto::KvAddress {
            collection: "default".into(),
            namespace: "n".into(),
            key: "k".into(),
        })
    }

    #[tokio::test]
    async fn grpc_kv_commit_indeterminate_is_unknown_for_put_delete_and_batch() {
        for kind in ["put", "delete", "batch"] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("commit-failure.vlite");
            let mut db = Database::create_with_format(&path, 1, crate::FormatVersion::V5).unwrap();
            db.kv_store("default")
                .unwrap()
                .put("n", "k", b"before", crate::kv::KvPut::default())
                .unwrap();
            db.file = crate::LockedFile::disk(std::fs::File::open(&path).unwrap());
            let service = Service::new(db);
            let adapter = Adapter {
                service: service.clone(),
            };
            let error = match kind {
                "delete" => adapter
                    .delete_kv_entry(Request::new(proto::KvDeleteRequest {
                        address: address(),
                        expected: None,
                    }))
                    .await
                    .unwrap_err(),
                "batch" => adapter
                    .write_kv_batch(Request::new(proto::KvBatchRequest {
                        collection: "default".into(),
                        operations: vec![proto::KvBatchOperation {
                            kind: Some(proto::kv_batch_operation::Kind::Put(proto::KvBatchPut {
                                namespace: "n".into(),
                                key: "k".into(),
                                value: b"after".to_vec(),
                                expected: None,
                            })),
                        }],
                    }))
                    .await
                    .unwrap_err(),
                _ => adapter
                    .put_kv_entry(Request::new(proto::KvPutRequest {
                        address: address(),
                        value: b"after".to_vec(),
                        ..Default::default()
                    }))
                    .await
                    .unwrap_err(),
            };
            assert_eq!(error.code(), tonic::Code::Unknown);
            assert_eq!(
                error.metadata().get(ERROR_CODE_TRAILER).unwrap(),
                "commit_indeterminate"
            );
            assert!(!error.metadata().get("x-request-id").unwrap().is_empty());
            assert!(error.metadata().get("retry-after").is_none());
            assert_eq!(service.metrics.commit_failures.load(Ordering::Relaxed), 1);
            assert!(service.close().await.is_err());
        }
    }

    #[tokio::test]
    async fn grpc_kv_shared_rate_limit_without_wall_clock_race() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new(
            Database::create_in_memory(
                1,
                crate::CreateOptions {
                    format: Some(crate::FormatVersion::V5),
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        let admin = "grpc_kv_rate_admin_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
        let app = configured_admin_router(
            service.clone(),
            ServerOptions {
                admin_token: Some(BearerToken::new(admin).unwrap()),
                admin_directory: Some(directory.path().into()),
                ..Default::default()
            },
        )
        .unwrap();
        let created = app.clone().oneshot(
            axum::http::Request::builder()
                .method("POST").uri("/v1/admin/credentials")
                .header("authorization", format!("Bearer {admin}"))
                .header("content-type", "application/json")
                .body(Body::from(json!({
                    "name":"rate", "collections":["default"], "operations":["read"],
                    "requests_per_second":1, "expires_unix_ms":crate::current_time_ms()+600_000
                }).to_string())).unwrap()
        ).await.unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let body = created.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        let token = body["token"].as_str().unwrap();
        let id = body["credential"]["id"].as_str().unwrap();
        let mut published = service.clone();
        published.admin = service.published_admin.lock().unwrap().clone().unwrap();
        let adapter = Adapter { service: published };
        let rest = || {
            app.clone().oneshot(
                axum::http::Request::builder()
                    .uri("/v1/collections/default/kv/entries?namespace=n")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
        };
        let grpc = || {
            let mut request = Request::new(proto::KvListRequest {
                collection: "default".into(),
                namespace: "n".into(),
                ..Default::default()
            });
            request
                .metadata_mut()
                .insert("authorization", format!("Bearer {token}").parse().unwrap());
            request
        };
        let freeze = |tokens| {
            // The limiter uses std::time::Instant, not Tokio's pausable clock.
            service.rate_limits.lock().unwrap().insert(
                id.into(),
                (tokens, Instant::now() + Duration::from_secs(3600)),
            );
        };
        freeze(1.0);
        assert_eq!(rest().await.unwrap().status(), StatusCode::OK);
        assert_eq!(service.rate_limits.lock().unwrap()[id].0, 0.0);
        freeze(0.0);
        let error = adapter.list_kv_entries(grpc()).await.unwrap_err();
        assert_eq!(error.code(), tonic::Code::ResourceExhausted);
        assert_eq!(
            error.metadata().get(ERROR_CODE_TRAILER).unwrap(),
            "rate_limited"
        );
        assert_eq!(error.metadata().get("retry-after").unwrap(), "1");
        freeze(1.0);
        assert!(
            adapter
                .list_kv_entries(grpc())
                .await
                .unwrap()
                .into_inner()
                .items
                .is_empty()
        );
        assert_eq!(service.rate_limits.lock().unwrap()[id].0, 0.0);
        freeze(0.0);
        let response = rest().await.unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get("retry-after").unwrap(), "1");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "rate_limited");
    }

    #[cfg(feature = "otel")]
    #[test]
    fn grpc_kv_telemetry_collection_for_every_request() {
        assert_eq!(
            request_collection(&proto::KvGetRequest { address: address() }),
            "default"
        );
        assert_eq!(
            request_collection(&proto::KvPutRequest {
                address: address(),
                ..Default::default()
            }),
            "default"
        );
        assert_eq!(
            request_collection(&proto::KvDeleteRequest {
                address: address(),
                expected: None
            }),
            "default"
        );
        assert_eq!(
            request_collection(&proto::KvListRequest {
                collection: "default".into(),
                ..Default::default()
            }),
            "default"
        );
        assert_eq!(
            request_collection(&proto::KvGetManyRequest {
                collection: "default".into(),
                ..Default::default()
            }),
            "default"
        );
        assert_eq!(
            request_collection(&proto::KvBatchRequest {
                collection: "default".into(),
                ..Default::default()
            }),
            "default"
        );
    }
}
