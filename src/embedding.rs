//! Pluggable embedding providers. No model runtime or network is used by default.
use crate::{Error, Result};
use std::{collections::BTreeMap, sync::Arc};
pub trait Embedder: Send + Sync {
    fn embed_bytes(&self, input: &[u8]) -> Result<Vec<f32>>;
    fn embed_text(&self, input: &str) -> Result<Vec<f32>> {
        self.embed_bytes(input.as_bytes())
    }
}
#[derive(Default)]
pub struct EmbedderRegistry {
    providers: BTreeMap<String, Arc<dyn Embedder>>,
}
impl EmbedderRegistry {
    pub fn register(&mut self, name: impl Into<String>, provider: Arc<dyn Embedder>) -> Result<()> {
        let name = name.into();
        if name.is_empty() || name.len() > 256 {
            return Err(Error::InvalidQuery("invalid embedder name"));
        }
        self.providers.insert(name, provider);
        Ok(())
    }
    pub fn get(&self, name: &str) -> Option<Arc<dyn Embedder>> {
        self.providers.get(name).cloned()
    }
}
/// Stable FNV-1a byte n-gram hashing, normalized to unit length. Not a semantic model.
pub struct HashEmbedder {
    dimensions: usize,
}
impl HashEmbedder {
    pub fn new(dimensions: usize) -> Result<Self> {
        crate::validate_dimensions(dimensions)?;
        Ok(Self { dimensions })
    }
}
impl Embedder for HashEmbedder {
    fn embed_bytes(&self, input: &[u8]) -> Result<Vec<f32>> {
        let mut out = vec![0.0f32; self.dimensions];
        for window in input.windows(3).chain(input.chunks(1)) {
            let mut hash = 0xcbf29ce484222325u64;
            for b in window {
                hash = (hash ^ u64::from(*b)).wrapping_mul(0x100000001b3);
            }
            out[(hash % self.dimensions as u64) as usize] +=
                if hash >> 63 == 0 { 1.0 } else { -1.0 };
        }
        let norm = crate::vector_norm(&out);
        if norm > 0.0 {
            for x in &mut out {
                *x = (f64::from(*x) / norm) as f32;
            }
        }
        Ok(out)
    }
}
#[cfg(feature = "http-embedder")]
pub struct HttpEmbedder {
    client: reqwest::blocking::Client,
    url: String,
    model: String,
    key: Option<String>,
}
#[cfg(feature = "http-embedder")]
impl HttpEmbedder {
    /// Endpoint must include `/embeddings`. Secrets are never included in errors.
    pub fn new(url: &str, model: &str, key: Option<String>) -> Result<Self> {
        let parsed =
            reqwest::Url::parse(url).map_err(|_| Error::InvalidQuery("invalid embedding URL"))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(Error::InvalidQuery("invalid embedding URL"));
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error::InvalidQuery("embedding client initialization failed"))?;
        Ok(Self {
            client,
            url: url.into(),
            model: model.into(),
            key,
        })
    }
    /// Reads VLITE_EMBEDDING_URL, VLITE_EMBEDDING_MODEL and optional VLITE_EMBEDDING_KEY.
    pub fn from_env() -> Result<Self> {
        let url = std::env::var("VLITE_EMBEDDING_URL")
            .map_err(|_| Error::InvalidQuery("missing embedding URL"))?;
        let model = std::env::var("VLITE_EMBEDDING_MODEL")
            .map_err(|_| Error::InvalidQuery("missing embedding model"))?;
        Self::new(&url, &model, std::env::var("VLITE_EMBEDDING_KEY").ok())
    }
}
#[cfg(feature = "http-embedder")]
impl Embedder for HttpEmbedder {
    fn embed_bytes(&self, input: &[u8]) -> Result<Vec<f32>> {
        use std::io::Read;
        let input = std::str::from_utf8(input)
            .map_err(|_| Error::InvalidQuery("HTTP embeddings require UTF-8 input"))?;
        let mut request = self
            .client
            .post(&self.url)
            .json(&serde_json::json!({"model":self.model,"input":input}));
        if let Some(key) = &self.key {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .and_then(|r| r.error_for_status())
            .map_err(|_| Error::InvalidQuery("embedding request failed"))?;
        let mut bytes = Vec::new();
        response
            .take(4_194_305)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::InvalidQuery("embedding response read failed"))?;
        if bytes.len() > 4_194_304 {
            return Err(Error::InvalidQuery("embedding response too large"));
        }
        let body: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| Error::InvalidQuery("invalid embedding response"))?;
        let v: Vec<f32> = serde_json::from_value(body["data"][0]["embedding"].clone())
            .map_err(|_| Error::InvalidQuery("invalid embedding response"))?;
        crate::validate_dimensions(v.len())?;
        crate::validate_vector(v.len(), &v)?;
        Ok(v)
    }
}
