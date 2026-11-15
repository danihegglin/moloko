//! `strategy.matrix` expansion, following GitHub's include/exclude rules.

use anyhow::{Result, bail};
use serde_json::{Map, Value};

pub const MAX_COMBINATIONS: usize = 256;

/// Expand an (already interpolated) matrix into its combinations.
/// An absent or empty matrix yields one empty combination.
pub fn expand(matrix: Option<&Value>) -> Result<Vec<Map<String, Value>>> {
    let m = match matrix {
        None | Some(Value::Null) => return Ok(vec![Map::new()]),
        Some(Value::Object(m)) => m,
        Some(other) => bail!("strategy.matrix must be a map, got {other}"),
    };
    let mut combos: Vec<Map<String, Value>> = vec![Map::new()];
    let mut has_base = false;
    for (key, values) in m {
        if key == "include" || key == "exclude" {
            continue;
        }
        let values = match values {
            Value::Array(a) => a,
            other => bail!("matrix.{key} must be a list, got {other}"),
        };
        if values.is_empty() {
            bail!("matrix.{key} is empty");
        }
        has_base = true;
        combos = combos
            .into_iter()
            .flat_map(|c| {
                values.iter().map(move |v| {
                    let mut c = c.clone();
                    c.insert(key.clone(), v.clone());
                    c
                })
            })
            .collect();
        if combos.len() > MAX_COMBINATIONS {
            bail!("matrix has more than {MAX_COMBINATIONS} combinations");
        }
    }
    if !has_base {
        combos.clear();
    }

    for ex in objects(m.get("exclude"), "exclude")? {
        combos.retain(|c| !ex.iter().all(|(k, v)| c.get(k).is_some_and(|cv| partial_eq(cv, v))));
    }

    // Each include is merged into every combination whose original values it doesn't change;
    // if it fits none, it becomes a combination of its own.
    let originals: Vec<Map<String, Value>> = combos.clone();
    for inc in objects(m.get("include"), "include")? {
        let mut added = false;
        for (i, orig) in originals.iter().enumerate() {
            let fits = inc
                .iter()
                .all(|(k, v)| orig.get(k).is_none_or(|ov| ov == v));
            if fits {
                for (k, v) in inc {
                    combos[i].insert(k.clone(), v.clone());
                }
                added = true;
            }
        }
        if !added {
            combos.push(inc.clone());
        }
    }
    if combos.is_empty() {
        bail!("matrix has no combinations");
    }
    if combos.len() > MAX_COMBINATIONS {
        bail!("matrix has more than {MAX_COMBINATIONS} combinations");
    }
    Ok(combos)
}

fn objects<'a>(v: Option<&'a Value>, what: &str) -> Result<Vec<&'a Map<String, Value>>> {
    match v {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| x.as_object().ok_or_else(|| anyhow::anyhow!("matrix.{what} entries must be maps")))
            .collect(),
        Some(other) => bail!("matrix.{what} must be a list, got {other}"),
    }
}

/// An exclude entry matches when its (possibly nested) values are contained in the combination's.
fn partial_eq(actual: &Value, pattern: &Value) -> bool {
    match (actual, pattern) {
        (Value::Object(a), Value::Object(p)) => p.iter().all(|(k, pv)| a.get(k).is_some_and(|av| partial_eq(av, pv))),
        _ => actual == pattern,
    }
}

/// `build (ubuntu, 18)`: the job id plus the combination's values, like GitHub.
pub fn instance_name(job: &str, combo: &Map<String, Value>) -> String {
    if combo.is_empty() {
        return job.to_string();
    }
    let vals: Vec<String> = combo
        .values()
        .map(|v| match v {
            Value::Object(_) | Value::Array(_) => serde_json::to_string(v).unwrap_or_default(),
            other => super::expr::to_string(other),
        })
        .collect();
    format!("{job} ({})", vals.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn names(m: Value) -> Vec<String> {
        expand(Some(&m)).unwrap().iter().map(|c| serde_json::to_string(c).unwrap()).collect()
    }

    #[test]
    fn cartesian_with_exclude_and_include() {
        let m = json!({
            "os": ["ubuntu", "mac"],
            "node": [18, 20],
            "exclude": [{"os": "mac", "node": 18}],
            "include": [{"os": "ubuntu", "experimental": true}, {"os": "windows", "node": 20}]
        });
        assert_eq!(
            names(m),
            [
                r#"{"os":"ubuntu","node":18,"experimental":true}"#,
                r#"{"os":"ubuntu","node":20,"experimental":true}"#,
                r#"{"os":"mac","node":20}"#,
                r#"{"os":"windows","node":20}"#,
            ]
        );
    }

    #[test]
    fn include_only_and_empty() {
        assert_eq!(names(json!({"include": [{"a": 1}, {"a": 2}]})), [r#"{"a":1}"#, r#"{"a":2}"#]);
        assert_eq!(expand(None).unwrap(), vec![Map::new()]);
        assert!(expand(Some(&json!({"a": []}))).is_err());
        assert!(expand(Some(&json!({"a": [1], "exclude": [{"a": 1}]}))).is_err());
        let big: Vec<i32> = (0..20).collect();
        assert!(expand(Some(&json!({"a": big, "b": big}))).is_err());
    }

    #[test]
    fn names_like_github() {
        let c = expand(Some(&json!({"os": ["ubuntu"], "node": [18]}))).unwrap();
        assert_eq!(instance_name("build", &c[0]), "build (ubuntu, 18)");
        assert_eq!(instance_name("build", &Map::new()), "build");
    }
}
