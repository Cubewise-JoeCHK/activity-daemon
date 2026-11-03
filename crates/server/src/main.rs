//! Binary entry point for the central server. Configuration via environment
//! variables (see docs/sync.md for the deployment runbook):
//!
//! | var             | default                 | meaning                       |
//! |-----------------|-------------------------|-------------------------------|
//! | `ACTIVITY_PORT` | `8080`                  | bind port on `0.0.0.0`        |
//! | `ACTIVITY_DB`   | `./data/activity.db`    | central SQLite store          |
//! | `ACTIVITY_DATA` | `./data`                | media root (`$DATA/media`)    |
//! | `ACTIVITY_TOKEN`| unset                   | require `X-Activity-Token`    |

use std::net::SocketAddr;
use std::path::PathBuf;

use server::{AppState, AuthConfig};

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn main() {
    let port: u16 = env_or("ACTIVITY_PORT", "8080").parse().unwrap_or_else(|_| {
        eprintln!("ACTIVITY_PORT is not a valid port number, using 8080");
        8080
    });
    let db_path = PathBuf::from(env_or("ACTIVITY_DB", "./data/activity.db"));
    let data_dir = PathBuf::from(env_or("ACTIVITY_DATA", "./data"));

    if let Some(parent) = db_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!(
                "failed to create database directory {}: {e}",
                parent.display()
            );
            std::process::exit(1);
        }
    }
    if let Err(e) = std::fs::create_dir_all(data_dir.join("media")) {
        eprintln!(
            "failed to create media root {}: {e}",
            data_dir.join("media").display()
        );
        std::process::exit(1);
    }

    let auth = AuthConfig::detect();
    println!("auth mode: {}", auth.describe());

    let state =
        AppState::open_with_auth(&db_path, data_dir.join("media"), auth).unwrap_or_else(|e| {
            eprintln!("failed to open central store at {}: {e}", db_path.display());
            std::process::exit(1);
        });
    let app = server::build_router(state);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async move {
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .unwrap_or_else(|e| {
                eprintln!("failed to bind {addr}: {e}");
                std::process::exit(1);
            });
        println!("activity-daemon central hub listening on http://{addr}");
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server error");
    });
}
