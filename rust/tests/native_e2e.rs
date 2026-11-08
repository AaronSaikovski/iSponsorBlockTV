#[path = "fixtures/native_scenario.rs"]
mod scenario;

#[tokio::test]
async fn native_application_loopback() {
    scenario::run().await.unwrap();
}
