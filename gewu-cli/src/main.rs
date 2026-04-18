#[tokio::main]
async fn main() -> anyhow::Result<()> {
    gewu_cli::run_with_plugins(|_registry| {}).await
}
