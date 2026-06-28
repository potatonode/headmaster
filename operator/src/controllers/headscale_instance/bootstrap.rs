use k8s_openapi::api::core::v1::{Pod, Secret};
use k8s_openapi_ext::SecretExt;
use kube::api::{Api, AttachParams};
use tokio::io::AsyncReadExt;

use super::Error;
use crate::context::Context;
use crate::controllers::applier::ChildApplier;

/// Ensures a Secret `headscale-api-key-<instance>` exists with field `key` containing an
/// API key created via `headscale apikeys create` inside the headscale pod.
pub(super) async fn ensure_api_key(ctx: &Context, child: &ChildApplier<'_>) -> Result<(), Error> {
    let secret_name = format!("headscale-api-key-{}", child.instance);
    let secret_api = Api::<Secret>::namespaced(ctx.client.clone(), &child.namespace);

    if secret_api.get_opt(&secret_name).await?.is_some() {
        return Ok(());
    }

    let pod_name = format!("headscale-server-{}-0", child.instance);
    let pod_api = Api::<Pod>::namespaced(ctx.client.clone(), &child.namespace);
    let attach_params = AttachParams::default()
        .container("headscale")
        .stdout(true)
        .stderr(true)
        .stdin(false);

    let mut process = pod_api
        .exec(
            &pod_name,
            [
                "headscale",
                "apikeys",
                "create",
                "--expiration",
                "876000h",
                "--output",
                "json",
            ],
            &attach_params,
        )
        .await
        .map_err(Error::Kube)?;

    let mut stdout = process
        .stdout()
        .ok_or_else(|| Error::ExecFailed("exec produced no stdout handle".to_string()))?;
    let stderr = process.stderr();

    let mut output = String::new();
    let mut stderr_output = String::new();
    if let Some(mut stderr) = stderr {
        let (stdout_result, _) = tokio::join!(
            stdout.read_to_string(&mut output),
            stderr.read_to_string(&mut stderr_output)
        );
        stdout_result?;
    } else {
        stdout.read_to_string(&mut output).await?;
    }
    drop(stdout);
    process
        .join()
        .await
        .map_err(|e| Error::ExecFailed(e.to_string()))?;

    let api_key: String = serde_json::from_str(output.trim()).map_err(|e| {
        // Do not include `output` in the message — stdout from a key-creation
        // command may contain a partial secret.
        if !stderr_output.trim().is_empty() {
            tracing::warn!(stderr = stderr_output.trim(), "apikeys create stderr");
        }
        Error::ExecInvalidOutput(format!("bad apikeys create output: {e}"))
    })?;

    tracing::info!(
        name = child.instance,
        "HeadscaleInstance: bootstrapped API key"
    );

    child
        .apply(
            "headscale",
            Secret::new(&secret_name).string_data([("HEADSCALE_API_KEY", api_key)]),
        )
        .await?;
    Ok(())
}
