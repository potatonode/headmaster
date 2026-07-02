use std::collections::HashSet;
use std::fs;
use std::path::Path;

use clap::{Parser, Subcommand};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write chart/README.md from values.schema.json and values.yaml
    Sync { chart_dir: String },
    /// Verify chart/README.md is up to date
    Check { chart_dir: String },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Sync { chart_dir } => sync(&chart_dir),
        Cmd::Check { chart_dir } => check(&chart_dir),
    }
}

fn sync(chart_dir: &str) {
    let chart_dir = Path::new(chart_dir);
    let readme = generate_from_dir(chart_dir);
    let readme_path = chart_dir.join("README.md");
    fs::write(&readme_path, readme)
        .unwrap_or_else(|e| panic!("write {}: {e}", readme_path.display()));
}

fn check(chart_dir: &str) {
    let chart_dir = Path::new(chart_dir);
    let expected = generate_from_dir(chart_dir);
    let readme_path = chart_dir.join("README.md");
    match fs::read_to_string(&readme_path) {
        Ok(got) if got == expected => {}
        Ok(_) => {
            eprintln!(
                "{} is out of date — run: task generate",
                readme_path.display()
            );
            std::process::exit(1);
        }
        Err(_) => {
            eprintln!("{} not found — run: task generate", readme_path.display());
            std::process::exit(1);
        }
    }
}

fn generate_from_dir(chart_dir: &Path) -> String {
    let schema_path = chart_dir.join("values.schema.json");
    let values_path = chart_dir.join("values.yaml");

    let schema_content = fs::read_to_string(&schema_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", schema_path.display()));
    let schema: serde_json::Value =
        serde_json::from_str(&schema_content).expect("values.schema.json is not valid JSON");

    let values_content = fs::read_to_string(&values_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", values_path.display()));
    let values: serde_json::Value =
        serde_yaml::from_str(&values_content).expect("values.yaml is not valid YAML");

    generate_chart_readme(&schema, &values)
}

fn generate_chart_readme(schema: &serde_json::Value, values: &serde_json::Value) -> String {
    let mut out = String::new();
    out.push_str("# Chart Values\n\n");
    out.push_str(
        "<!-- Generated from `chart/values.schema.json` by `task generate`. Do not edit by hand. -->\n\n",
    );

    out.push_str("## Operator\n\n");
    let top_required = required_fields(schema);
    let mut rows: Vec<[String; 5]> = Vec::new();
    if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
        for (key, prop) in props {
            if matches!(key.as_str(), "headscaleInstances" | "global") {
                continue;
            }
            collect_rows(
                &mut rows,
                key,
                prop,
                top_required.contains(key.as_str()),
                values.get(key),
            );
        }
    }
    out.push_str(&render_table(
        &["Value", "Type", "Default", "Required", "Description"],
        &rows,
    ));

    out.push_str("\n## HeadscaleInstance\n\n");
    out.push_str(
        "The following fields are set per-instance under `headscaleInstances.<name>`.\n\n",
    );
    let mut instance_rows: Vec<[String; 5]> = Vec::new();
    if let Some(instance_schema) =
        schema.pointer("/properties/headscaleInstances/additionalProperties")
    {
        let instance_required = required_fields(instance_schema);
        if let Some(props) = instance_schema
            .get("properties")
            .and_then(|p| p.as_object())
        {
            for (key, prop) in props {
                collect_rows(
                    &mut instance_rows,
                    key,
                    prop,
                    instance_required.contains(key.as_str()),
                    None,
                );
            }
        }
    }
    out.push_str(&render_table(
        &["Field", "Type", "Default", "Required", "Description"],
        &instance_rows,
    ));

    out
}

fn collect_rows(
    rows: &mut Vec<[String; 5]>,
    path: &str,
    prop: &serde_json::Value,
    required: bool,
    value: Option<&serde_json::Value>,
) {
    let raw_desc = prop
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("-");
    let description = raw_desc
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|");

    let default = value
        .map(format_default)
        .or_else(|| prop.get("default").map(format_default))
        .unwrap_or_else(|| "-".to_string());

    rows.push([
        format!("`{path}`"),
        format!("`{}`", schema_type_str(prop)),
        default,
        if required { "yes" } else { "no" }.to_string(),
        description,
    ]);

    if let Some(sub_props) = prop.get("properties").and_then(|p| p.as_object()) {
        let sub_required = required_fields(prop);
        for (sub_key, sub_prop) in sub_props {
            collect_rows(
                rows,
                &format!("{path}.{sub_key}"),
                sub_prop,
                sub_required.contains(sub_key.as_str()),
                value.and_then(|v| v.get(sub_key)),
            );
        }
    }
}

fn format_default(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "-".to_string(),
        serde_json::Value::Bool(b) => format!("`{b}`"),
        serde_json::Value::Number(n) => format!("`{n}`"),
        serde_json::Value::String(s) if s.is_empty() => r#"`""`"#.to_string(),
        serde_json::Value::String(s) => format!("`{s}`"),
        serde_json::Value::Array(a) if a.is_empty() => "`[]`".to_string(),
        serde_json::Value::Array(_) => "-".to_string(),
        serde_json::Value::Object(o) if o.is_empty() => "`{}`".to_string(),
        serde_json::Value::Object(_) => "-".to_string(),
    }
}

fn render_table(headers: &[&str], rows: &[[String; 5]]) -> String {
    let ncols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(ncols) {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let mut out = String::new();
    out.push('|');
    for (header, &w) in headers.iter().zip(&widths) {
        out.push_str(&format!(" {header:<w$} |"));
    }
    out.push('\n');

    out.push('|');
    for &w in &widths {
        out.push_str(&format!(" {} |", "-".repeat(w)));
    }
    out.push('\n');

    for row in rows {
        out.push('|');
        for (cell, &w) in row.iter().zip(&widths) {
            out.push_str(&format!(" {cell:<w$} |"));
        }
        out.push('\n');
    }

    out
}

fn required_fields(schema: &serde_json::Value) -> HashSet<&str> {
    schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default()
}

fn schema_type_str(prop: &serde_json::Value) -> String {
    if let Some(enum_vals) = prop.get("enum").and_then(|e| e.as_array()) {
        let vals: Vec<String> = enum_vals
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::to_string)
            .collect();
        return vals.join(" / ");
    }

    match prop.get("type").and_then(|t| t.as_str()) {
        Some("string") => "string".into(),
        Some("boolean") => "bool".into(),
        Some("integer") => "integer".into(),
        Some("number") => "number".into(),
        Some("array") => {
            if prop.pointer("/items/type").and_then(|t| t.as_str()) == Some("string") {
                "list(string)".into()
            } else {
                "list".into()
            }
        }
        Some("object") => "object".into(),
        _ => "any".into(),
    }
}
