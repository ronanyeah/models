//! Regenerates `../src/xai.rs` from `../openapi.json`.
//!
//!     jq -f xai-schemas.jq openapi.json > xai-schemas.json   # prune to the roots
//!     cargo run -p gen-xai && rustfmt src/xai.rs
//!
//! Types come from typify; the endpoint traits below tie each path to the
//! request and response types the spec pairs it with, so a call site can't
//! post the wrong body to the wrong path.
//!
//! This is a separate crate so that typify stays out of the `models`
//! dependency graph, and so a regeneration that emits code which doesn't
//! compile can still be rerun.

use std::{
    collections::BTreeSet,
    error::Error,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;
use typify::{TypeSpace, TypeSpaceSettings};

const SPEC: &str = "openapi.json";
const SCHEMA: &str = "xai-schemas.json";
const OUT: &str = "src/xai.rs";

const TRAITS: &str = "\
#[doc = \"A POST operation: the request body type, its path, and what comes back.\"]
pub trait PostEndpoint: ::serde::Serialize {
    type Response: ::serde::de::DeserializeOwned;
    const PATH: &'static str;
}
#[doc = \"A GET operation taking no body: a marker type, its path and response.\"]
pub trait GetEndpoint {
    type Response: ::serde::de::DeserializeOwned;
    const PATH: &'static str;
}
";

/// The workspace root, so the paths above resolve wherever cargo is invoked
/// from.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("gen-xai sits one level below the workspace root")
        .to_path_buf()
}

/// `#/components/schemas/ModelResponse` -> `ModelResponse`.
fn ref_name(schema: &Value) -> Option<&str> {
    schema["$ref"].as_str()?.rsplit('/').next()
}

/// The JSON request body schema of an operation, if it has one.
fn request_type(op: &Value) -> Option<&str> {
    ref_name(&op["requestBody"]["content"]["application/json"]["schema"])
}

/// The 200-response schema of an operation, if it returns JSON.
fn response_type(op: &Value) -> Option<&str> {
    ref_name(&op["responses"]["200"]["content"]["application/json"]["schema"])
}

/// `/v1/models` -> `V1_MODELS`.
fn const_name(path: &str) -> String {
    path.trim_start_matches('/')
        .replace(['/', '-', '.'], "_")
        .to_uppercase()
}

/// `/v1/models` -> `GetModels`. The version segment is dropped for brevity; if
/// that ever collides, the generated code simply won't compile.
fn marker_name(path: &str) -> String {
    let rest: String = path
        .trim_start_matches('/')
        .split('/')
        .filter(|seg| !is_version(seg))
        .flat_map(|seg| seg.split(['-', '_', '.']))
        .map(capitalize)
        .collect();

    format!("Get{rest}")
}

fn is_version(seg: &str) -> bool {
    seg.starts_with('v') && seg.len() > 1 && seg[1..].bytes().all(|b| b.is_ascii_digit())
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Path constants plus one trait impl per operation whose types survived
/// pruning. Templated paths (`/v1/files/{file_id}`) are skipped: they need
/// formatting, not a constant.
fn endpoints(generated: &BTreeSet<String>) -> Result<String, Box<dyn Error>> {
    let spec: Value = serde_json::from_str(&fs::read_to_string(root().join(SPEC))?)?;

    let paths = spec["paths"]
        .as_object()
        .ok_or("openapi.json has no `paths` object")?;

    let mut consts = String::new();
    let mut impls = String::new();

    for (path, item) in paths.iter().filter(|(p, _)| !p.contains('{')) {
        writeln!(
            consts,
            "    pub const {}: &str = {path:?};",
            const_name(path)
        )?;

        let Some(operations) = item.as_object() else {
            continue;
        };

        for (method, op) in operations {
            let Some(response) = response_type(op).filter(|t| generated.contains(*t)) else {
                continue;
            };

            match method.as_str() {
                "post" => {
                    let Some(request) = request_type(op).filter(|t| generated.contains(*t)) else {
                        continue;
                    };

                    writeln!(
                        impls,
                        "impl PostEndpoint for {request} {{\n    \
                         type Response = {response};\n    \
                         const PATH: &'static str = {path:?};\n}}"
                    )?;
                }
                "get" => {
                    let marker = marker_name(path);

                    writeln!(
                        impls,
                        "#[doc = \"GET {path}\"]\npub struct {marker};\n\
                         impl GetEndpoint for {marker} {{\n    \
                         type Response = {response};\n    \
                         const PATH: &'static str = {path:?};\n}}"
                    )?;
                }
                _ => {}
            }
        }
    }

    Ok(format!(
        "#[doc = \"Endpoint paths, from the spec's `paths` object.\"]\n\
         pub mod paths {{\n{consts}}}\n\n{TRAITS}\n{impls}"
    ))
}

fn main() -> Result<(), Box<dyn Error>> {
    let raw = fs::read_to_string(root().join(SCHEMA))?;

    let generated: BTreeSet<String> = serde_json::from_str::<Value>(&raw)?["$defs"]
        .as_object()
        .ok_or("xai-schemas.json has no `$defs`")?
        .keys()
        .cloned()
        .collect();

    let mut type_space = TypeSpace::new(TypeSpaceSettings::default().with_struct_builder(true));
    type_space.add_root_schema(serde_json::from_str(&raw)?)?;

    let count = type_space.iter_types().count();
    let out = root().join(OUT);

    fs::write(
        &out,
        format!(
            "//! Wire types generated from `{SCHEMA}` by `cargo run -p gen-xai`.\n\
             //! Do not edit by hand.\n\
             #![allow(clippy::all)]\n\n{}\n\n{}",
            type_space.to_stream(),
            endpoints(&generated)?
        ),
    )?;

    eprintln!("wrote {count} types to {}", out.display());

    Ok(())
}
