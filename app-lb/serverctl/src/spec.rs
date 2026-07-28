//! Working with deployment specs as JSON.
//!
//! The admin API's `PUT /deployments/:id` replaces the *whole* spec, so every
//! in-place change here is a read-modify-write: fetch the live spec, edit the
//! `Value`, put it back. Editing the `Value` (rather than a typed struct) is
//! what makes that safe — a field this CLI has never heard of survives the trip.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::path::Path;

/// One `--env` argument: `KEY=VALUE` to set, `KEY-` to remove (kubectl's syntax
/// for `set env`).
#[derive(Debug, PartialEq, Eq)]
pub enum EnvChange {
    Set(String, String),
    Remove(String),
}

pub fn parse_env(arg: &str) -> Result<EnvChange> {
    if let Some((key, value)) = arg.split_once('=') {
        if key.is_empty() {
            bail!("{arg:?} has an empty variable name");
        }
        return Ok(EnvChange::Set(key.to_string(), value.to_string()));
    }
    if let Some(key) = arg.strip_suffix('-')
        && !key.is_empty()
    {
        return Ok(EnvChange::Remove(key.to_string()));
    }
    bail!("{arg:?} is not KEY=VALUE (to set) or KEY- (to remove)")
}

/// One `--route` argument.
///
/// Either a comma-separated key list — `host=a.example.com,path=/api` — or a
/// bare value whose shape says what it is: a leading `/` is a path prefix, a
/// leading `*.` or `.` is a subdomain match, anything else an exact host.
pub fn parse_route(arg: &str) -> Result<Value> {
    let mut rule = Map::new();
    for part in arg.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (key, value) = match part.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None if part.starts_with('/') => ("path", part),
            None if part.starts_with("*.") => ("suffix", part.trim_start_matches("*.")),
            None if part.starts_with('.') => ("suffix", part),
            None => ("host", part),
        };
        if value.is_empty() {
            bail!("route {arg:?} has an empty value for {key:?}");
        }
        let field = match key {
            "host" => "host",
            "suffix" | "host-suffix" | "host_suffix" | "wildcard" => "host_suffix",
            "path" | "path-prefix" | "path_prefix" | "prefix" => "path_prefix",
            other => bail!(
                "unknown route key {other:?} in {arg:?} — expected host=, suffix= or path="
            ),
        };
        // `host_suffix` accepts a leading dot server-side and ignores it; strip
        // the `*.` form here so both spellings land on the same value.
        let value = if field == "host_suffix" {
            value.trim_start_matches("*.")
        } else {
            value
        };
        rule.insert(field.to_string(), Value::String(value.to_string()));
    }
    if rule.is_empty() {
        bail!("route {arg:?} is empty");
    }
    if rule.contains_key("host") && rule.contains_key("host_suffix") {
        bail!("route {arg:?} sets both host= and suffix=; a rule needs one or the other");
    }
    Ok(Value::Object(rule))
}

/// Build a route rule from the shorthand flags. All of them describe a *single*
/// rule together, which is what makes `--host x --path-prefix /api` mean "that
/// host under that path" rather than two independent rules.
pub fn route_from_parts(
    host: Option<&str>,
    host_suffix: Option<&str>,
    path_prefix: Option<&str>,
) -> Option<Value> {
    let mut rule = Map::new();
    if let Some(h) = host {
        rule.insert("host".into(), Value::String(h.to_string()));
    }
    if let Some(s) = host_suffix {
        rule.insert(
            "host_suffix".into(),
            Value::String(s.trim_start_matches("*.").to_string()),
        );
    }
    if let Some(p) = path_prefix {
        rule.insert("path_prefix".into(), Value::String(p.to_string()));
    }
    (!rule.is_empty()).then_some(Value::Object(rule))
}

/// Read specs from a file (or `-` for stdin).
///
/// Accepts JSON or YAML, a single document or many: a JSON array, a multi-doc
/// YAML stream, or one object. A `GET /deployments` response is unwrapped too,
/// so `serverctl get deployment web -o json | serverctl apply -f -` round-trips.
pub fn read_specs(path: &Path) -> Result<Vec<Value>> {
    let text = if path == Path::new("-") {
        std::io::read_to_string(std::io::stdin())
            .context("reading a spec from stdin")?
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?
    };
    if text.trim().is_empty() {
        bail!("{} is empty", display_path(path));
    }

    let docs: Vec<Value> = match serde_json::from_str::<Value>(&text) {
        Ok(v) => vec![v],
        // Not JSON: try YAML, which may hold several `---`-separated documents.
        Err(json_err) => {
            let mut docs = Vec::new();
            for doc in serde_yaml::Deserializer::from_str(&text) {
                let v = Value::deserialize(doc).map_err(|yaml_err| {
                    anyhow::anyhow!(
                        "{} is neither JSON ({json_err}) nor YAML ({yaml_err})",
                        display_path(path)
                    )
                })?;
                docs.push(v);
            }
            docs
        }
    };

    let mut specs = Vec::new();
    for doc in docs {
        match doc {
            Value::Array(items) => specs.extend(items.into_iter().map(unwrap_envelope)),
            Value::Null => {} // an empty YAML document, e.g. a trailing `---`
            other => specs.push(unwrap_envelope(other)),
        }
    }
    if specs.is_empty() {
        bail!("{} contained no deployment specs", display_path(path));
    }
    for spec in &specs {
        if !spec.is_object() {
            bail!("{} contains a {} where a deployment spec was expected", display_path(path), kind_of(spec));
        }
    }
    Ok(specs)
}

fn display_path(path: &Path) -> String {
    if path == Path::new("-") {
        "stdin".into()
    } else {
        path.display().to_string()
    }
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "object",
    }
}

/// A `GET /deployments` entry wraps the spec in status fields; a spec file does
/// not. Accept both by unwrapping the envelope when we see one.
fn unwrap_envelope(v: Value) -> Value {
    match &v {
        Value::Object(map) if map.contains_key("spec") && map.contains_key("kind") => {
            map.get("spec").cloned().unwrap_or(v)
        }
        _ => v,
    }
}

/// Pull the spec out of a `GET /deployments/:id` response.
pub fn spec_of(deployment: &Value) -> Result<Value> {
    deployment
        .get("spec")
        .cloned()
        .context("the server's deployment response had no `spec` field")
}

pub fn spec_id(spec: &Value) -> Option<&str> {
    spec.get("id")?.as_str()
}

pub fn is_static(spec: &Value) -> bool {
    spec.get("upstreams")
        .and_then(Value::as_array)
        .is_some_and(|u| !u.is_empty())
}

/// The `vm` block of a managed deployment, for editing.
pub fn vm_mut<'a>(spec: &'a mut Value, id: &str) -> Result<&'a mut Map<String, Value>> {
    if is_static(spec) {
        bail!(
            "deployment {id:?} is static (proxy_pass) and has no VM template — \
             change where it points with `serverctl set upstreams {id} <addr>...`"
        );
    }
    spec.get_mut("vm")
        .and_then(Value::as_object_mut)
        .with_context(|| format!("deployment {id:?} has no `vm` block to edit"))
}

/// Apply `KEY=VALUE` / `KEY-` changes to a managed deployment's `env_vars`.
pub fn apply_env(spec: &mut Value, id: &str, changes: &[EnvChange]) -> Result<Vec<String>> {
    let vm = vm_mut(spec, id)?;
    let env = vm
        .entry("env_vars")
        .or_insert_with(|| Value::Object(Map::new()));
    if env.is_null() {
        *env = Value::Object(Map::new());
    }
    let env = env
        .as_object_mut()
        .with_context(|| format!("deployment {id:?} has a non-object `vm.env_vars`"))?;

    let mut applied = Vec::new();
    for change in changes {
        match change {
            EnvChange::Set(k, v) => {
                env.insert(k.clone(), Value::String(v.clone()));
                applied.push(format!("{k}="));
            }
            EnvChange::Remove(k) => {
                if env.remove(k).is_none() {
                    // Not an error: `set env FOO-` is idempotent, like kubectl.
                    applied.push(format!("{k} (not set)"));
                } else {
                    applied.push(format!("{k} (removed)"));
                }
            }
        }
    }
    Ok(applied)
}

/// Merge only the fields that were given onto a partial scaling patch. The API
/// keeps every field it isn't sent, so an empty patch is a no-op rather than a
/// reset to defaults.
pub fn scaling_patch(entries: &[(&str, Option<u64>)]) -> Map<String, Value> {
    let mut patch = Map::new();
    for (key, value) in entries {
        if let Some(v) = value {
            patch.insert((*key).to_string(), Value::from(*v));
        }
    }
    patch
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn env_arguments_set_and_remove() {
        assert_eq!(
            parse_env("RUST_LOG=debug").unwrap(),
            EnvChange::Set("RUST_LOG".into(), "debug".into())
        );
        // A value containing '=' survives: only the first one splits.
        assert_eq!(
            parse_env("URL=postgres://u:p@h/db?a=b").unwrap(),
            EnvChange::Set("URL".into(), "postgres://u:p@h/db?a=b".into())
        );
        assert_eq!(parse_env("RUST_LOG-").unwrap(), EnvChange::Remove("RUST_LOG".into()));
        // An empty value is legitimate; an empty name is not.
        assert_eq!(parse_env("EMPTY=").unwrap(), EnvChange::Set("EMPTY".into(), String::new()));
        assert!(parse_env("=x").is_err());
        assert!(parse_env("bare").is_err());
    }

    #[test]
    fn routes_parse_from_keys_and_from_shape() {
        assert_eq!(parse_route("host=a.example.com").unwrap(), json!({"host": "a.example.com"}));
        assert_eq!(parse_route("a.example.com").unwrap(), json!({"host": "a.example.com"}));
        assert_eq!(parse_route("/api").unwrap(), json!({"path_prefix": "/api"}));
        assert_eq!(
            parse_route("*.apps.example.com").unwrap(),
            json!({"host_suffix": "apps.example.com"})
        );
        assert_eq!(
            parse_route("suffix=.apps.example.com,path=/api").unwrap(),
            json!({"host_suffix": ".apps.example.com", "path_prefix": "/api"})
        );
    }

    #[test]
    fn a_route_cannot_be_both_exact_and_wildcard() {
        assert!(parse_route("host=a.example.com,suffix=example.com").is_err());
        assert!(parse_route("nope=1").is_err());
    }

    #[test]
    fn shorthand_flags_make_one_combined_rule() {
        assert_eq!(
            route_from_parts(Some("a.example.com"), None, Some("/api")).unwrap(),
            json!({"host": "a.example.com", "path_prefix": "/api"})
        );
        assert!(route_from_parts(None, None, None).is_none());
    }

    #[test]
    fn a_get_response_unwraps_back_into_a_spec() {
        let envelope = json!({"spec": {"id": "web"}, "kind": "vm", "ready": 1});
        assert_eq!(unwrap_envelope(envelope), json!({"id": "web"}));
        // A bare spec is left alone.
        let bare = json!({"id": "web", "kind": "not-an-envelope-field"});
        assert_eq!(unwrap_envelope(bare.clone()), bare);
    }

    #[test]
    fn env_is_created_when_the_template_has_none() {
        let mut spec = json!({"id": "web", "vm": {"port": 8080}});
        apply_env(&mut spec, "web", &[EnvChange::Set("A".into(), "1".into())]).unwrap();
        assert_eq!(spec["vm"]["env_vars"], json!({"A": "1"}));
    }

    #[test]
    fn editing_the_vm_of_a_static_deployment_is_refused() {
        let mut spec = json!({"id": "proxy", "upstreams": ["10.0.0.9:8080"]});
        let err = vm_mut(&mut spec, "proxy").unwrap_err().to_string();
        assert!(err.contains("static"), "{err}");
    }

    #[test]
    fn a_scaling_patch_carries_only_what_was_asked_for() {
        let patch = scaling_patch(&[("min_replicas", Some(2)), ("max_replicas", None)]);
        assert_eq!(patch.len(), 1);
        assert_eq!(patch["min_replicas"], json!(2));
    }
}
