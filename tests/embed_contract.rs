//! Wire contract for the embedding client: the request shape, both dialects, the bearer header.
//!
//! The listener answers exactly one request on loopback. No external network, no mock library.

use phylactery_mcp::config::EmbedApi;
use phylactery_mcp::embed::Embedder;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const DIM: usize = 768;

async fn serve_once(body: String) -> (String, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = socket.read(&mut chunk).await.expect("read");
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            let text = String::from_utf8_lossy(&buffer).to_string();
            if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                match content_length(head) {
                    Some(length) if rest.len() >= length => break,
                    None => break,
                    _ => {}
                }
            }
        }
        let request = String::from_utf8_lossy(&buffer).to_string();
        let reply = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(reply.as_bytes()).await.expect("write");
        socket.flush().await.expect("flush");
        request
    });
    (format!("http://{address}"), handle)
}

fn content_length(head: &str) -> Option<usize> {
    head.lines().find_map(|line| {
        line.to_ascii_lowercase()
            .strip_prefix("content-length:")
            .and_then(|value| value.trim().parse().ok())
    })
}

fn vector() -> Vec<f32> {
    (0..DIM).map(|index| index as f32 / 1000.0).collect()
}

#[tokio::test]
async fn ollama_dialect_posts_to_api_embed() {
    let body = serde_json::json!({ "embeddings": [vector()] }).to_string();
    let (base, server) = serve_once(body).await;
    let embedder = Embedder::from_parts(EmbedApi::Ollama, base, "nomic-embed-text-v2-moe", None)
        .expect("client");
    let vectors = embedder.embed(&["hello".to_string()]).await.expect("embed");
    assert_eq!(vectors.len(), 1);
    assert_eq!(vectors[0].len(), DIM);
    let request = server.await.expect("server");
    assert!(request.starts_with("POST /api/embed "), "{request}");
    assert!(request.contains("\"input\":[\"hello\"]"), "{request}");
    assert!(
        !request.to_ascii_lowercase().contains("authorization"),
        "{request}"
    );
}

#[tokio::test]
async fn openai_dialect_posts_to_v1_embeddings_with_bearer() {
    let body = serde_json::json!({ "data": [{ "embedding": vector() }] }).to_string();
    let (base, server) = serve_once(body).await;
    let embedder = Embedder::from_parts(
        EmbedApi::OpenAi,
        format!("{base}/v1"),
        "text-embedding-3-small",
        Some("sk-test".to_string()),
    )
    .expect("client");
    let vectors = embedder.embed(&["hello".to_string()]).await.expect("embed");
    assert_eq!(vectors[0].len(), DIM);
    let request = server.await.expect("server");
    assert!(request.starts_with("POST /v1/embeddings "), "{request}");
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer sk-test"),
        "{request}"
    );
}

#[tokio::test]
async fn a_wrong_dimension_is_refused_before_the_column_sees_it() {
    let body = serde_json::json!({ "embeddings": [vec![0.0f32; 1536]] }).to_string();
    let (base, server) = serve_once(body).await;
    let embedder = Embedder::from_parts(EmbedApi::Ollama, base, "text-embedding-3-small", None)
        .expect("client");
    let error = embedder
        .embed(&["hello".to_string()])
        .await
        .expect_err("must refuse a 1536d answer");
    assert!(error.to_string().contains("1536"), "{error}");
    let _ = server.await;
}
