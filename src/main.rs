//! Castor CLI binary entry point.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    castor::cli::run().await
}
