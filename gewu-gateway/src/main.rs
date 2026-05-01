#[tokio::main]
async fn main() -> anyhow::Result<()> {
    gewu_gateway::run().await
}
