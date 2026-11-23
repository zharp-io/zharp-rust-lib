//! An Actix Web app with Zharp: `ZHARP_KEY=zpk_… cargo run --example actix --features actix`
#![allow(clippy::unnecessary_literal_unwrap)]

use actix_web::{App, HttpResponse, HttpServer, web};

async fn show(id: web::Path<u64>) -> Result<HttpResponse, actix_web::Error> {
    if *id == 0 {
        return Err(actix_web::error::ErrorInternalServerError(
            "the order store is down",
        ));
    }
    tracing::info!(id = *id, "order shown");
    Ok(HttpResponse::Ok().body(format!("order {id}")))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let _zharp = zharp::init!();
    HttpServer::new(|| {
        App::new()
            .wrap(zharp::actix::Middleware)
            .route("/orders/{id}", web::get().to(show))
            .route(
                "/panic",
                web::get().to(|| async {
                    let empty: Option<u32> = None;
                    HttpResponse::Ok().body(empty.unwrap().to_string())
                }),
            )
    })
    .bind(("0.0.0.0", 8080))?
    .run()
    .await
}
