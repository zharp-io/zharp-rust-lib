use actix_web::{App, HttpResponse, HttpServer, web};
use sqlx::MySqlPool;

/// Adds the same cart twice: MySQL's duplicate-key error, logged with an error field, then the
/// handler's own 500 error (which must not be reported again).
async fn add(pool: web::Data<MySqlPool>) -> Result<HttpResponse, actix_web::Error> {
    for _ in 0..2 {
        if let Err(error) = sqlx::query("INSERT INTO carts (owner) VALUES (?)").bind("ada@example.com").execute(pool.get_ref()).await {
            tracing::error!(error = &error as &dyn std::error::Error, "could not add the cart");
            return Err(actix_web::error::ErrorInternalServerError("the cart could not be added"));
        }
    }
    Ok(HttpResponse::Ok().finish())
}

async fn down() -> Result<HttpResponse, actix_web::Error> {
    Err(actix_web::error::ErrorServiceUnavailable("the cart store is down"))
}

async fn panics(path: web::Path<usize>) -> HttpResponse {
    let carts: Vec<u32> = Vec::new();
    HttpResponse::Ok().body(carts[*path].to_string())
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let _zharp = zharp::init!();
    let pool = MySqlPool::connect(&std::env::var("DATABASE_URL").unwrap()).await.unwrap();
    sqlx::query("DROP TABLE IF EXISTS carts").execute(&pool).await.unwrap();
    sqlx::query("CREATE TABLE carts (id BIGINT AUTO_INCREMENT PRIMARY KEY, owner VARCHAR(200) UNIQUE NOT NULL)").execute(&pool).await.unwrap();
    let pool = web::Data::new(pool);
    HttpServer::new(move || {
        App::new()
            .app_data(pool.clone())
            .wrap(zharp::actix::Middleware)
            .route("/carts", web::post().to(add))
            .route("/down", web::get().to(down))
            .route("/carts/{index}", web::get().to(panics))
    })
    .bind(("0.0.0.0", 8080))?
    .run()
    .await
}
