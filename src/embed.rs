//! Embedding client.
//!
//! The server computes no vectors. It asks an external endpoint for them, in one of two dialects,
//! and refuses to write a row it could not embed: a memory without a vector does not enter the
//! circuit. A failure here is reported, never papered over.

use std::time::Duration;

use serde_json::{Value, json};

use crate::config::{Config, EMBED_DIM, EmbedApi};

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("embedding request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("embedding endpoint returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("embedding payload malformed: {0}")]
    Malformed(String),
    #[error("embedding endpoint returned {got} vectors for {expected} inputs")]
    Count { expected: usize, got: usize },
    #[error("embedding dimension {got} does not match the vector(768) columns")]
    Dim { got: usize },
}

pub struct Embedder {
    client: reqwest::Client,
    api: EmbedApi,
    url: String,
    model: String,
    key: Option<String>,
}

impl Embedder {
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        Self::from_parts(
            config.embed_api,
            config.embed_url.clone(),
            config.embed_model.clone(),
            config.embed_api_key.clone(),
        )
    }

    /// Build a client from the four things it needs, so the wire contract is testable
    /// without a `Config` and without a database.
    pub fn from_parts(
        api: EmbedApi,
        url: impl Into<String>,
        model: impl Into<String>,
        key: Option<String>,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            client,
            api,
            url: url.into(),
            model: model.into(),
            key,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn base_url(&self) -> &str {
        &self.url
    }

    pub fn api(&self) -> EmbedApi {
        self.api
    }

    /// The URL vectors are actually requested from. A base that already ends in `/v1` is not
    /// doubled: `https://api.example.com/v1` and `https://api.example.com` both work.
    pub fn endpoint(&self) -> String {
        endpoint_for(self.api, &self.url)
    }

    pub async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let mut request = self.client.post(self.endpoint()).json(&json!({
            "model": self.model,
            "input": inputs,
        }));
        if let Some(key) = &self.key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(EmbedError::Status {
                status: status.as_u16(),
                body: body.chars().take(400).collect(),
            });
        }
        parse_vectors(self.api, &body, inputs.len())
    }

    pub async fn embed_one(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let mut vectors = self.embed(&[text.to_string()]).await?;
        vectors
            .pop()
            .ok_or_else(|| EmbedError::Malformed("no vector returned".to_string()))
    }

    /// Cheap liveness probe: the same endpoint the real work uses.
    pub async fn probe(&self) -> Result<usize, EmbedError> {
        let vector = self.embed_one("probe").await?;
        Ok(vector.len())
    }
}

/// The request URL for a dialect and a base. Pure, so the shape is checkable without a network.
pub fn endpoint_for(api: EmbedApi, base: &str) -> String {
    match api {
        EmbedApi::Ollama => format!("{base}/api/embed"),
        EmbedApi::OpenAi => {
            if base.ends_with("/v1") {
                format!("{base}/embeddings")
            } else {
                format!("{base}/v1/embeddings")
            }
        }
    }
}

/// Parse either dialect into vectors, and refuse anything that is not `EMBED_DIM` wide.
///
/// Pure on purpose: the shape of a provider's answer is checkable without a network, and a wrong
/// dimension must fail here rather than at the `vector(768)` column.
pub fn parse_vectors(
    api: EmbedApi,
    body: &str,
    expected: usize,
) -> Result<Vec<Vec<f32>>, EmbedError> {
    let payload: Value =
        serde_json::from_str(body).map_err(|error| EmbedError::Malformed(error.to_string()))?;
    let raw = match api {
        EmbedApi::Ollama => payload.get("embeddings").and_then(Value::as_array).cloned(),
        EmbedApi::OpenAi => payload.get("data").and_then(Value::as_array).map(|items| {
            items
                .iter()
                .map(|item| item.get("embedding").cloned().unwrap_or(Value::Null))
                .collect::<Vec<_>>()
        }),
    };
    let raw = raw.ok_or_else(|| {
        EmbedError::Malformed(match api {
            EmbedApi::Ollama => "missing `embeddings` array".to_string(),
            EmbedApi::OpenAi => "missing `data[].embedding` array".to_string(),
        })
    })?;
    if raw.len() != expected {
        return Err(EmbedError::Count {
            expected,
            got: raw.len(),
        });
    }
    let mut out = Vec::with_capacity(raw.len());
    for vector in &raw {
        let values = vector
            .as_array()
            .ok_or_else(|| EmbedError::Malformed("vector is not an array".to_string()))?;
        if values.len() != EMBED_DIM {
            return Err(EmbedError::Dim { got: values.len() });
        }
        out.push(
            values
                .iter()
                .map(|value| value.as_f64().unwrap_or_default() as f32)
                .collect(),
        );
    }
    Ok(out)
}

/// pgvector text literal: `[0.1,0.2,...]`, cast in SQL as `$1::text::vector`.
pub fn vector_literal(values: &[f32]) -> String {
    let mut out = String::with_capacity(values.len() * 12 + 2);
    out.push('[');
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&format!("{value:.7}"));
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector() -> Value {
        Value::Array((0..EMBED_DIM).map(|i| json!(i as f64 / 1000.0)).collect())
    }

    #[test]
    fn literal_is_pgvector_shaped() {
        let literal = vector_literal(&[0.5, -1.0, 0.0]);
        assert!(literal.starts_with('['));
        assert!(literal.ends_with(']'));
        assert_eq!(literal.matches(',').count(), 2);
    }

    #[test]
    fn parses_ollama_shape() {
        let body = json!({ "embeddings": [vector(), vector()] }).to_string();
        let parsed = parse_vectors(EmbedApi::Ollama, &body, 2).expect("ollama shape");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].len(), EMBED_DIM);
    }

    #[test]
    fn parses_openai_shape() {
        let body = json!({
            "object": "list",
            "data": [
                { "object": "embedding", "index": 0, "embedding": vector() },
                { "object": "embedding", "index": 1, "embedding": vector() }
            ],
            "model": "text-embedding-3-small",
            "usage": { "prompt_tokens": 8, "total_tokens": 8 }
        })
        .to_string();
        let parsed = parse_vectors(EmbedApi::OpenAi, &body, 2).expect("openai shape");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].len(), EMBED_DIM);
    }

    #[test]
    fn rejects_wrong_dimension() {
        let body = json!({ "embeddings": [Value::Array(vec![json!(0.0); 1536])] }).to_string();
        match parse_vectors(EmbedApi::Ollama, &body, 1) {
            Err(EmbedError::Dim { got }) => assert_eq!(got, 1536),
            other => panic!("expected Dim, got {other:?}"),
        }
    }

    #[test]
    fn rejects_wrong_count() {
        let body = json!({ "embeddings": [vector()] }).to_string();
        match parse_vectors(EmbedApi::Ollama, &body, 3) {
            Err(EmbedError::Count { expected, got }) => {
                assert_eq!((expected, got), (3, 1));
            }
            other => panic!("expected Count, got {other:?}"),
        }
    }

    #[test]
    fn rejects_the_other_dialect() {
        let body = json!({ "data": [{ "embedding": vector() }] }).to_string();
        assert!(parse_vectors(EmbedApi::Ollama, &body, 1).is_err());
        let body = json!({ "embeddings": [vector()] }).to_string();
        assert!(parse_vectors(EmbedApi::OpenAi, &body, 1).is_err());
    }

    #[test]
    fn endpoint_is_not_doubled() {
        assert_eq!(
            endpoint_for(EmbedApi::OpenAi, "https://api.example.com"),
            "https://api.example.com/v1/embeddings"
        );
        assert_eq!(
            endpoint_for(EmbedApi::OpenAi, "https://api.example.com/v1"),
            "https://api.example.com/v1/embeddings"
        );
        assert_eq!(
            endpoint_for(EmbedApi::Ollama, "http://127.0.0.1:11434"),
            "http://127.0.0.1:11434/api/embed"
        );
    }
}
