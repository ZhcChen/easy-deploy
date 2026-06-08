#[tokio::main]
async fn main() -> anyhow::Result<()> {
    e2e::smoke_test().await
}
