//! Decide whether a workflow's `on:` matches a push or a manual run.

use super::glob;
use serde_json::{Map, Value};

pub enum Event<'a> {
    /// `changed` is None when the changed files can't be determined (then path filters pass).
    Push {
        git_ref: &'a str,
        changed: Option<&'a [String]>,
    },
    /// A manual run from the dashboard or API: `workflow_dispatch`.
    Manual,
}

impl Event<'_> {
    pub fn name(&self) -> &'static str {
        match self {
            Event::Push { .. } => "push",
            Event::Manual => "workflow_dispatch",
        }
    }
}

/// The configuration of `event` under `on:`, if the workflow listens to it.
/// `on: push`, `on: [push]` and `on: {push: null}` all yield `Some(Null)`.
pub fn event_config<'a>(on: &'a Value, event: &str) -> Option<&'a Value> {
    static NULL: Value = Value::Null;
    match on {
        Value::String(s) if s == event => Some(&NULL),
        Value::Array(a) if a.iter().any(|v| v.as_str() == Some(event)) => Some(&NULL),
        Value::Object(m) => m.get(event),
        _ => None,
    }
}

pub fn matches(on: &Value, ev: &Event) -> bool {
    let Some(cfg) = event_config(on, ev.name()) else {
        return false;
    };
    match ev {
        Event::Manual => true,
        Event::Push { git_ref, changed } => {
            let cfg = cfg.as_object();
            ref_matches(cfg, git_ref) && paths_match(cfg, *changed)
        }
    }
}

/// Default values of `workflow_dispatch` inputs, as the `inputs` context of a manual run.
pub fn dispatch_inputs(on: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(Value::Object(inputs)) =
        event_config(on, "workflow_dispatch").and_then(|c| c.get("inputs"))
    {
        for (k, spec) in inputs {
            let v = spec.get("default").cloned().unwrap_or_else(|| {
                // Booleans default to false, everything else to "".
                if spec.get("type").and_then(Value::as_str) == Some("boolean") {
                    Value::Bool(false)
                } else {
                    Value::String(String::new())
                }
            });
            out.insert(k.clone(), v);
        }
    }
    out
}

/// The `inputs` context of a manual run: the workflow's defaults overlaid with `given`,
/// coerced to each input's type. Fails on missing required inputs, unknown inputs and
/// values that aren't one of a `choice` input's options.
pub fn resolve_inputs(on: &Value, given: &Map<String, Value>) -> anyhow::Result<Map<String, Value>> {
    let specs = event_config(on, "workflow_dispatch")
        .and_then(|c| c.get("inputs"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for k in given.keys() {
        if !specs.contains_key(k) {
            anyhow::bail!("unexpected input `{k}`");
        }
    }
    let mut out = dispatch_inputs(on);
    for (k, spec) in &specs {
        let ty = spec.get("type").and_then(Value::as_str).unwrap_or("string");
        let raw = given.get(k).cloned().unwrap_or_else(|| out[k].clone());
        let text = super::expr::to_string(&raw);
        let value = match ty {
            "boolean" => match text.as_str() {
                "true" => Value::Bool(true),
                "false" | "" => Value::Bool(false),
                other => anyhow::bail!("input `{k}` must be true or false, got {other:?}"),
            },
            "number" if !text.is_empty() => text
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .ok_or_else(|| anyhow::anyhow!("input `{k}` must be a number, got {text:?}"))?,
            _ => Value::String(text.clone()),
        };
        if ty != "boolean" && text.is_empty() && spec.get("required").is_some_and(super::expr::truthy) {
            anyhow::bail!("input `{k}` is required");
        }
        if ty == "choice"
            && !text.is_empty()
            && let Some(Value::Array(options)) = spec.get("options")
            && !options.iter().any(|o| super::expr::to_string(o) == text)
        {
            anyhow::bail!("input `{k}` must be one of the workflow's options, got {text:?}");
        }
        out.insert(k.clone(), value);
    }
    Ok(out)
}

fn list(cfg: Option<&Map<String, Value>>, key: &str) -> Option<Vec<String>> {
    match cfg?.get(key)? {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Array(a) => Some(a.iter().map(super::expr::to_string).collect()),
        _ => None,
    }
}

fn ref_matches(cfg: Option<&Map<String, Value>>, git_ref: &str) -> bool {
    let (name, include, ignore, other_include, other_ignore) =
        if let Some(b) = git_ref.strip_prefix("refs/heads/") {
            (b, "branches", "branches-ignore", "tags", "tags-ignore")
        } else if let Some(t) = git_ref.strip_prefix("refs/tags/") {
            (t, "tags", "tags-ignore", "branches", "branches-ignore")
        } else {
            return true;
        };
    match (list(cfg, include), list(cfg, ignore)) {
        (Some(p), _) => glob::included(&p, name),
        (None, Some(p)) => !p.iter().any(|p| glob::matches(p, name)),
        // Filters only for the other kind of ref: this kind doesn't run.
        (None, None) => list(cfg, other_include).is_none() && list(cfg, other_ignore).is_none(),
    }
}

fn paths_match(cfg: Option<&Map<String, Value>>, changed: Option<&[String]>) -> bool {
    let Some(changed) = changed else {
        return true;
    };
    match (list(cfg, "paths"), list(cfg, "paths-ignore")) {
        (Some(p), _) => changed.iter().any(|f| glob::included(&p, f)),
        (None, Some(p)) => !changed.iter().all(|f| p.iter().any(|p| glob::matches(p, f))),
        (None, None) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gha::workflow::yaml_to_json;

    fn push(on: &str, git_ref: &str, changed: Option<&[&str]>) -> bool {
        let on = yaml_to_json(on).unwrap();
        let changed: Option<Vec<String>> = changed.map(|c| c.iter().map(|s| s.to_string()).collect());
        matches(
            &on,
            &Event::Push {
                git_ref,
                changed: changed.as_deref(),
            },
        )
    }

    #[test]
    fn event_shapes() {
        assert!(push("push", "refs/heads/x", None));
        assert!(push("[pull_request, push]", "refs/heads/x", None));
        assert!(push("push:", "refs/heads/x", None));
        assert!(!push("pull_request", "refs/heads/x", None));
        let manual = yaml_to_json("{push: {}, workflow_dispatch: {inputs: {env: {default: prod}, dry: {type: boolean}}}}").unwrap();
        assert!(matches(&manual, &Event::Manual));
        assert!(!matches(&yaml_to_json("push").unwrap(), &Event::Manual));
        let inputs = dispatch_inputs(&manual);
        assert_eq!(inputs["env"], "prod");
        assert_eq!(inputs["dry"], false);
    }

    #[test]
    fn dispatch_input_resolution() {
        let on = yaml_to_json(
            "workflow_dispatch:\n  inputs:\n    version: {required: true}\n    dry: {type: boolean, default: true}\n    env: {type: choice, options: [dev, prod], default: dev}\n    n: {type: number}\n",
        )
        .unwrap();
        let given = |v: serde_json::Value| v.as_object().unwrap().clone();
        let e = resolve_inputs(&on, &Map::new()).unwrap_err();
        assert!(e.to_string().contains("`version` is required"), "{e}");
        let r = resolve_inputs(&on, &given(serde_json::json!({"version": "0.3.0", "dry": "false", "n": "2"}))).unwrap();
        assert_eq!(r["version"], "0.3.0");
        assert_eq!(r["dry"], false);
        assert_eq!(r["env"], "dev");
        assert_eq!(r["n"], 2.0);
        assert!(resolve_inputs(&on, &given(serde_json::json!({"version": "1", "env": "qa"}))).is_err());
        assert!(resolve_inputs(&on, &given(serde_json::json!({"version": "1", "nope": "x"}))).is_err());
        assert!(resolve_inputs(&on, &given(serde_json::json!({"version": "1", "dry": "maybe"}))).is_err());
    }

    #[test]
    fn branch_and_tag_filters() {
        let on = "push: {branches: [main, 'releases/**']}";
        assert!(push(on, "refs/heads/main", None));
        assert!(push(on, "refs/heads/releases/1.0", None));
        assert!(!push(on, "refs/heads/feature", None));
        // Only branch filters: tag pushes don't run.
        assert!(!push(on, "refs/tags/v1", None));
        let tags = "push: {tags: ['v*']}";
        assert!(push(tags, "refs/tags/v1.2", None));
        assert!(!push(tags, "refs/heads/main", None));
        let ignore = "push: {branches-ignore: ['dependabot/**']}";
        assert!(push(ignore, "refs/heads/main", None));
        assert!(!push(ignore, "refs/heads/dependabot/npm/x", None));
        assert!(push("push: {branches: ['*', '!wip']}", "refs/heads/main", None));
        assert!(!push("push: {branches: ['*', '!wip']}", "refs/heads/wip", None));
    }

    #[test]
    fn path_filters() {
        let on = "push: {paths: ['src/**', '!src/docs/**']}";
        assert!(push(on, "refs/heads/main", Some(&["src/a.rs"])));
        assert!(!push(on, "refs/heads/main", Some(&["README.md"])));
        assert!(!push(on, "refs/heads/main", Some(&["src/docs/x.md"])));
        assert!(push(on, "refs/heads/main", None));
        let ignore = "push: {paths-ignore: ['**.md']}";
        assert!(!push(ignore, "refs/heads/main", Some(&["README.md", "docs/a.md"])));
        assert!(push(ignore, "refs/heads/main", Some(&["README.md", "main.go"])));
        // Branch and path filters must both match.
        assert!(!push("push: {branches: [main], paths: ['src/**']}", "refs/heads/dev", Some(&["src/a"])));
    }
}
