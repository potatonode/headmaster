use std::collections::HashSet;
use std::fs;
use std::path::Path;

use clap::{Parser, Subcommand};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print all CRD manifests to stdout
    Dump,
    /// Write CRD YAMLs and values schema to <chart-dir>
    Sync { chart_dir: String },
    /// Verify <chart-dir> CRD YAMLs and values schema are up to date
    Check { chart_dir: String },
}

fn main() {
    let cli = Cli::parse();
    let crds = operator::crds();
    match cli.command {
        Cmd::Dump => dump(&crds),
        Cmd::Sync { chart_dir } => sync(&crds, &chart_dir),
        Cmd::Check { chart_dir } => check(&crds, &chart_dir),
    }
}

fn dump(crds: &[CustomResourceDefinition]) {
    for crd in crds {
        print!("---\n{}", serde_saphyr::to_string(crd).unwrap());
    }
}

fn sync(crds: &[CustomResourceDefinition], chart_dir: &str) {
    let chart_dir = Path::new(chart_dir);
    sync_crds(crds, &chart_dir.join("crds"));
    sync_values_schema(crds, &chart_dir.join("values.schema.json"));
}

fn check(crds: &[CustomResourceDefinition], chart_dir: &str) {
    let chart_dir = Path::new(chart_dir);
    let mut ok = true;
    ok &= check_crds(crds, &chart_dir.join("crds"));
    ok &= check_values_schema(crds, &chart_dir.join("values.schema.json"));
    if !ok {
        std::process::exit(1);
    }
}

fn sync_crds(crds: &[CustomResourceDefinition], dir: &Path) {
    let expected: HashSet<String> = crds.iter().map(crd_filename).collect();
    for crd in crds {
        let path = dir.join(crd_filename(crd));
        fs::write(&path, serde_saphyr::to_string(crd).unwrap())
            .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    }
    let stale: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "yaml"))
        .filter(|e| !expected.contains(e.file_name().to_string_lossy().as_ref()))
        .collect();
    for entry in stale {
        let path = entry.path();
        fs::remove_file(&path).unwrap_or_else(|e| panic!("remove {}: {e}", path.display()));
    }
}

fn check_crds(crds: &[CustomResourceDefinition], dir: &Path) -> bool {
    let expected: HashSet<String> = crds.iter().map(crd_filename).collect();
    let mut ok = true;
    for crd in crds {
        let path = dir.join(crd_filename(crd));
        let want = serde_saphyr::to_string(crd).unwrap();
        match fs::read_to_string(&path) {
            Ok(got) if got == want => {}
            Ok(_) => {
                eprintln!("{} is out of date — run: task generate", path.display());
                ok = false;
            }
            Err(_) => {
                eprintln!("{} not found — run: task generate", path.display());
                ok = false;
            }
        }
    }
    let extra: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "yaml"))
        .filter(|e| !expected.contains(e.file_name().to_string_lossy().as_ref()))
        .collect();
    for entry in extra {
        eprintln!(
            "{} is not a known CRD — run: task generate",
            entry.path().display()
        );
        ok = false;
    }
    ok
}

fn sync_values_schema(crds: &[CustomResourceDefinition], schema_path: &Path) {
    let content = fs::read_to_string(schema_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", schema_path.display()));
    let updated = apply_crd_schemas(crds, &content);
    fs::write(schema_path, updated)
        .unwrap_or_else(|e| panic!("write {}: {e}", schema_path.display()));
}

fn check_values_schema(crds: &[CustomResourceDefinition], schema_path: &Path) -> bool {
    let content = fs::read_to_string(schema_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", schema_path.display()));
    if content == apply_crd_schemas(crds, &content) {
        true
    } else {
        eprintln!(
            "{} is out of date — run: task generate",
            schema_path.display()
        );
        false
    }
}

// For each CRD whose kind maps to a key in values.schema.json's top-level
// properties, replaces that property's `additionalProperties` with the CRD's
// spec schema. Keys are derived by lowercasing the first char of the kind and
// appending "s" (e.g. HeadscaleInstance -> headscaleInstances).
fn apply_crd_schemas(crds: &[CustomResourceDefinition], content: &str) -> String {
    let mut schema: serde_json::Value =
        serde_json::from_str(content).expect("values.schema.json is not valid JSON");

    for crd in crds {
        let key = values_key_for_kind(&crd.spec.names.kind);
        if let Some(spec_schema) = extract_spec_schema(crd)
            && let Some(entry) =
                schema.pointer_mut(&format!("/properties/{key}/additionalProperties"))
        {
            *entry = spec_schema;
            simplify_k8s_fields(entry);
        }
    }

    let mut out = serde_json::to_string_pretty(&schema).expect("serialize values schema");
    out.push('\n');
    out
}

// Strip verbose sub-properties from k8s types that users don't need to know about.
// ResourceRequirements expands into claims/limits/requests with Quantity docs; just expose it
// as an opaque object like the top-level resources field.
fn simplify_k8s_fields(spec_schema: &mut serde_json::Value) {
    if let Some(resources) = spec_schema.pointer_mut("/properties/resources")
        && let Some(obj) = resources.as_object_mut()
    {
        obj.remove("properties");
        obj.insert(
            "x-kubernetes-preserve-unknown-fields".to_string(),
            true.into(),
        );
    }
}

fn extract_spec_schema(crd: &CustomResourceDefinition) -> Option<serde_json::Value> {
    let spec = crd
        .spec
        .versions
        .first()?
        .schema
        .as_ref()?
        .open_api_v3_schema
        .as_ref()?
        .properties
        .as_ref()?
        .get("spec")?;
    serde_json::to_value(spec).ok()
}

// HeadscaleInstance -> headscaleInstances
fn values_key_for_kind(kind: &str) -> String {
    let mut chars = kind.chars();
    match chars.next() {
        Some(first) => format!("{}{}s", first.to_lowercase(), chars.as_str()),
        None => String::new(),
    }
}

fn crd_filename(crd: &CustomResourceDefinition) -> String {
    format!("{}.yaml", crd.spec.names.plural)
}
