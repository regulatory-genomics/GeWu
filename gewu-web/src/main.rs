#[tokio::main]
async fn main() -> anyhow::Result<()> {
    gewu_web::run_with_plugins(|_registry| {}).await
}
