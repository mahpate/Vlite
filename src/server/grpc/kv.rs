//! KV wire conversion only; REST owns validation, pagination and mutation policy.
use super::*;
use crate::server::kv as rest;
use serde::de::DeserializeOwned;

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value).map_err(internal)
}

fn collection(name: String) -> Result<Path<String>, ApiError> {
    if name.is_empty() {
        return Err(unprocessable("KV requires a nonempty collection"));
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
                    Ok(target),
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
                    Ok(target),
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
}
