#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let namespace = std::env::args()
        .nth(1)
        .ok_or("usage: wait-namespace-ready <namespace>")?;
    eprintln!("waiting for namespace {namespace} to be ready...");
    let kube = integration_tests::kube_client().await?;
    integration_tests::wait_for_namespace_ready(&kube, &namespace).await?;
    eprintln!("namespace {namespace} is ready");
    Ok(())
}
