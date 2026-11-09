#[path = "../tests/fixtures/native_scenario.rs"]
mod scenario;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    scenario::run().await
}
