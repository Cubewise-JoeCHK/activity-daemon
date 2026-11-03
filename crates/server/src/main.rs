//! Binary entry point for the central server. Configuration via
//! environment variables (kept dependency-free; a config file can arrive
//! later if the hub ever needs one).

use std::path::PathBuf;

use server::AppState;

#[tokio::main]
async fn main() {
    let addr = std::env::var("ACTIVITY_SERVER_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let db_path = std::env::var("ACTIVITY_SERVER_DB").unwrap_or_else(|_| "activity.db".to_string());
    let media_dir =
        std::env::var("ACTIVITY_SERVER_MEDIA_DIR").unwrap_or_else(|_| "media".to_string());

    let state = AppState::open(std::path::Path::new(&db_path), PathBuf::from(media_dir))
        .unwrap_or_else(|e| {
            eprintln!("failed to open central store: {e}");
            std::process::exit(1);
        });
    let app = server::build_router(state);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| {
            eprintln!("failed to bind {addr}: {e}");
            std::process::exit(1);
        });
    println!("activity-daemon server listening on {addr}");
    axum::serve(listener, app).await.expect("server error");
}
