#[tokio::test]
async fn api_serves_dashboard_and_healthcheck() {
    e2e::smoke_test().await.expect("e2e smoke test");
}
