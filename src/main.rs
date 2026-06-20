#[tokio::main]
async fn main() {
    if let Err(e) = cog::run().await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
