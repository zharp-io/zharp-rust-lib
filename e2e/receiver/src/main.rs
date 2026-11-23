use std::io::{Read, Write};

use axum::body::Bytes;
use axum::http::HeaderMap;
use axum::routing::post;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use prost::Message;

fn body(headers: &HeaderMap, bytes: Bytes) -> Vec<u8> {
    if headers.get("content-encoding").is_some_and(|v| v == "gzip") {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&bytes[..]).read_to_end(&mut out).unwrap();
        out
    } else {
        bytes.to_vec()
    }
}

fn write(kind: &str, headers: &HeaderMap, json: serde_json::Value) {
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let line = serde_json::json!({ "kind": kind, "authorization": auth, "body": json });
    // One write per export, one export at a time: concurrent requests never interleave lines.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap();
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open("/out/otlp.jsonl").unwrap();
    file.write_all(format!("{line}
").as_bytes()).unwrap();
}

#[tokio::main]
async fn main() {
    let app = axum::Router::new()
        .route("/api/v1/otlp/v1/traces", post(|headers: HeaderMap, bytes: Bytes| async move {
            let request = ExportTraceServiceRequest::decode(&body(&headers, bytes)[..]).unwrap();
            write("traces", &headers, serde_json::to_value(&request).unwrap());
            ""
        }))
        .route("/api/v1/otlp/v1/logs", post(|headers: HeaderMap, bytes: Bytes| async move {
            let request = ExportLogsServiceRequest::decode(&body(&headers, bytes)[..]).unwrap();
            write("logs", &headers, serde_json::to_value(&request).unwrap());
            ""
        }));
    let listener = tokio::net::TcpListener::bind("0.0.0.0:4318").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
