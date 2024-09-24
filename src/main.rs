use std::{future::IntoFuture, net::Ipv4Addr, process, sync::Arc};

use axum::{
    extract::{Path, Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    routing::method_routing::get,
    Router,
};
use rand::Rng;

struct HurlinState {
    key: [u8; 16],
}

#[axum::debug_middleware]
async fn fuzz_assert(
    Path((fuzz_key, _)): Path<(String, String)>,
    State(state): State<Arc<HurlinState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if fuzz_key.as_bytes() == state.key {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

#[axum::debug_handler]
async fn imports(
    Path((_, params)): Path<(String, String)>,
    State(state): State<Arc<HurlinState>>,
) -> impl IntoResponse {
    println!("params: {params:?}");
}

#[tokio::main]
async fn main() {
    let fuzz_raw: [u8; 8] = rand::thread_rng().gen();
    let mut fuzz_key = [0; 16];

    hex::encode_to_slice(fuzz_raw, &mut fuzz_key).unwrap();

    println!("{}", core::str::from_utf8(&fuzz_key).unwrap());

    let state = Arc::new(HurlinState { key: fuzz_key });

    let app = Router::new()
        .route("/import/:fuzz_key/*params", get(imports))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            fuzz_assert,
        ))
        .with_state(state);

    let port = tokio::net::TcpListener::bind((Ipv4Addr::from_bits(0), 0)).await.unwrap();

    println!("{}", port.local_addr().unwrap().port());

    let server = tokio::spawn(axum::serve(port, app).into_future());

    process::Command::new("fish").status();

    println!("exiting hurlin");

    server.abort();
}
