use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::Api;

use super::client;

#[tokio::test]
async fn all_crds_are_registered() {
    let api: Api<CustomResourceDefinition> = Api::all(client().await);
    let registered = api
        .list(&Default::default())
        .await
        .expect("failed to list CRDs");
    let registered_names: std::collections::HashSet<&str> = registered
        .items
        .iter()
        .filter_map(|c| c.metadata.name.as_deref())
        .collect();

    for crd in operator::crds() {
        let name = crd.metadata.name.clone().unwrap();
        assert!(
            registered_names.contains(name.as_str()),
            "CRD {name} not registered in envtest"
        );
    }
}
