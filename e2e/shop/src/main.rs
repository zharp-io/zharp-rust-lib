mod cart;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Router, routing::get, routing::post};
use sqlx::PgPool;
use zharp::axum::Reported;

#[derive(Debug, thiserror::Error)]
enum AppError {
    #[error("database error")]
    Db(#[from] sqlx::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()).into_response()
    }
}

async fn order(Path(id): Path<i64>) -> String {
    tracing::info!(id, "showing the order");
    let client = zharp::reqwest::wrap(reqwest::Client::new());
    let _ = client.get("http://127.0.0.1:3000/health").send().await;
    format!("order {id}")
}

/// Signs up the same email twice: the second insert fails (unique violation), logged with an
/// error field, and the handler answers 500.
async fn signup(State(pool): State<PgPool>) -> (StatusCode, &'static str) {
    for _ in 0..2 {
        let inserted = sqlx::query("INSERT INTO users (email) VALUES ($1)").bind("ada@example.com").execute(&pool).await;
        if let Err(error) = inserted {
            tracing::error!(error = &error as &dyn std::error::Error, "could not sign up");
            return (StatusCode::INTERNAL_SERVER_ERROR, "no");
        }
    }
    (StatusCode::OK, "ok")
}

/// A query error turned into the handler's error with `?` through Reported.
async fn missing_table(State(pool): State<PgPool>) -> Result<String, Reported<AppError>> {
    let row: (i64,) = sqlx::query_as("SELECT count(*) FROM carts WHERE id = $1").bind(7i64).fetch_one(&pool).await?;
    Ok(row.0.to_string())
}

async fn panics() -> String {
    cart::total(&[Some(3), None]).to_string()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _zharp = zharp::init!();
    let pool = PgPool::connect(&std::env::var("DATABASE_URL")?).await?;
    sqlx::query("DROP TABLE IF EXISTS users").execute(&pool).await?;
    sqlx::query("CREATE TABLE users (id bigserial PRIMARY KEY, email text UNIQUE NOT NULL)").execute(&pool).await?;

    let app = Router::new()
        .route("/orders/{id}", get(order))
        .route("/signup", post(signup))
        .route("/carts", get(missing_table))
        .route("/panic", get(panics))
        .route("/health", get(|| async { "ok" }))
        .with_state(pool)
        .layer(zharp::axum::layer());
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
    axum::serve(listener, app).await?;
    Ok(())
}
