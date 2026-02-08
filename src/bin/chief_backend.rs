#[path = "backend/api/mod.rs"]
mod api;
#[path = "backend/app.rs"]
mod app;

#[tokio::main]
async fn main() {
    if let Err(err) = app::run().await {
        eprintln!("backend error: {err:#}");
        std::process::exit(1);
    }
}
