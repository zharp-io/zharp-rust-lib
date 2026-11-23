//! An Axum app with Zharp: `ZHARP_KEY=zpk_… cargo run --example axum --features axum,reqwest`
//!
//! - GET /orders/42      → 200, a span named `GET /orders/{id}`, an outgoing call in the same trace
//! - GET /orders/0       → 500, reported once as the request's error (with the stack of the `?`)
//! - GET /panic          → 500, the panic reported with its backtrace
#![allow(clippy::unnecessary_literal_unwrap)]

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Router, routing::get};
use zharp::axum::Reported;

#[derive(Debug, thiserror::Error)]
enum OrderError {
    #[error("order {0} not found")]
    NotFound(u64),
    #[error("could not read the order")]
    Io(#[from] std::io::Error),
}

impl IntoResponse for OrderError {
    fn into_response(self) -> Response {
        let status = match self {
            OrderError::NotFound(_) => StatusCode::NOT_FOUND,
            OrderError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, self.to_string()).into_response()
    }
}

fn read_order(id: u64) -> Result<String, std::io::Error> {
    if id == 0 {
        return Err(std::io::Error::other("disk on fire"));
    }
    Ok(format!("order {id}"))
}

async fn show(Path(id): Path<u64>) -> Result<String, Reported<OrderError>> {
    let order = read_order(id)?; // std::io::Error → OrderError → Reported, stack taken here
    if id > 1000 {
        return Err(OrderError::NotFound(id).into_reported());
    }
    tracing::info!(id, "order shown");
    let client = zharp::reqwest::wrap(reqwest::Client::new());
    let _ = client.get("http://127.0.0.1:3000/health").send().await;
    Ok(order)
}

trait IntoReported: Sized {
    fn into_reported(self) -> Reported<Self> {
        Reported::new(self)
    }
}
impl IntoReported for OrderError {}

#[tokio::main]
async fn main() {
    let _zharp = zharp::init!();

    let app = Router::new()
        .route("/orders/{id}", get(show))
        .route(
            "/panic",
            get(|| async {
                let empty: Option<u32> = None;
                empty.unwrap().to_string()
            }),
        )
        .route("/health", get(|| async { "ok" }))
        .layer(zharp::axum::layer());
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
