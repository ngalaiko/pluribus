mod listener;
mod native;
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(about = "Receive HTTP requests for a Pluribus plugin")]
struct Args {
    #[arg(long)]
    listen: SocketAddr,
    #[arg(long)]
    socket: PathBuf,
    #[arg(long)]
    runtime_uid: u32,
    /// Repeat ID:METHOD:PATH:CONSUMER for each route.
    #[arg(long, required = true, value_parser = route)]
    route: Vec<listener::Route>,
    #[arg(long, default_value_t = 1024 * 1024)]
    max_body_bytes: usize,
    #[arg(long, default_value_t = 256 * 1024 * 1024)]
    max_queue_bytes: usize,
    #[arg(long, default_value_t = 8)]
    response_timeout_seconds: u64,
}
fn route(value: &str) -> Result<listener::Route, String> {
    let parts: Vec<_> = value.splitn(4, ':').collect();
    if parts.len() != 4 || parts.iter().any(|s| s.is_empty()) {
        return Err("expected ID:METHOD:PATH:CONSUMER".into());
    }
    Ok(listener::Route {
        id: parts[0].into(),
        methods: parts[1].split(',').map(str::to_owned).collect(),
        path: parts[2].into(),
        consumer: parts[3].into(),
    })
}
#[tokio::main]
async fn main() {
    let args = Args::parse();
    if let Err(error) = listener::serve(listener::Config {
        listen: args.listen,
        socket: args.socket,
        runtime_uid: args.runtime_uid,
        routes: args.route,
        max_body_bytes: args.max_body_bytes,
        max_queue_bytes: args.max_queue_bytes,
        response_timeout_seconds: args.response_timeout_seconds,
    })
    .await
    {
        eprintln!("HTTP listener: {error}");
        std::process::exit(1);
    }
}
