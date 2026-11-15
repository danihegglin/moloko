//! GitHub Actions workflow files (`.github/workflows/*.yml`) and action metadata (`action.yml`).
//!
//! Fields that GitHub allows in several shapes (a string or a list, a map or an expression)
//! are kept as JSON values and resolved when the job runs, because most of them can contain
//! `${{ }}` expressions that depend on runtime contexts.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value};

pub const WORKFLOWS_DIR: &str = ".github/workflows";

/// Parse YAML into ordered JSON, applying `<<` merge keys.
pub fn yaml_to_json(src: &str) -> Result<Value> {
    let mut y: serde_yaml::Value = serde_yaml::from_str(src)?;
    y.apply_merge()?;
    Ok(serde_json::to_value(y)?)
}

#[derive(Debug, Clone, Deserialize)]
pub struct Workflow {
    pub name: Option<String>,
    #[serde(default)]
    pub on: Value,
    #[serde(default)]
    pub env: Map<String, Value>,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub jobs: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Defaults {
    #[serde(default)]
    pub run: RunDefaults,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct RunDefaults {
    pub shell: Option<String>,
    pub working_directory: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Job {
    #[serde(default, deserialize_with = "one_or_many")]
    pub needs: Vec<String>,
    pub runs_on: Option<Value>,
    #[serde(rename = "if")]
    pub cond: Option<Value>,
    /// A map, or an expression that evaluates to one.
    pub env: Option<Value>,
    #[serde(default)]
    pub defaults: Defaults,
    pub strategy: Option<Strategy>,
    /// An image name, or `{image, env, options, ...}`.
    pub container: Option<Value>,
    #[serde(default)]
    pub services: Map<String, Value>,
    #[serde(default)]
    pub outputs: Map<String, Value>,
    #[serde(default)]
    pub steps: Vec<Step>,
    pub timeout_minutes: Option<Value>,
    pub continue_on_error: Option<Value>,
    /// Reusable workflow call; not supported yet.
    pub uses: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Strategy {
    pub matrix: Option<Value>,
    pub fail_fast: Option<Value>,
    pub max_parallel: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Step {
    pub id: Option<String>,
    pub name: Option<String>,
    #[serde(rename = "if")]
    pub cond: Option<Value>,
    pub uses: Option<String>,
    pub run: Option<Value>,
    pub shell: Option<String>,
    #[serde(default)]
    pub with: Map<String, Value>,
    #[serde(default)]
    pub env: Map<String, Value>,
    pub working_directory: Option<String>,
    pub continue_on_error: Option<Value>,
    pub timeout_minutes: Option<Value>,
}

impl Step {
    pub fn run_script(&self) -> Option<String> {
        self.run.as_ref().map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
    }
}

/// Accept `needs: build` as well as `needs: [build, test]`.
fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Option::<OneOrMany>::deserialize(d)? {
        None => vec![],
        Some(OneOrMany::One(s)) => vec![s],
        Some(OneOrMany::Many(v)) => v,
    })
}

impl Workflow {
    pub fn parse(src: &str) -> Result<Self> {
        let v = yaml_to_json(src).context("invalid YAML")?;
        let wf: Workflow = serde_json::from_value(v).context("invalid workflow")?;
        if wf.on.is_null() {
            bail!("workflow has no `on:` trigger");
        }
        if wf.jobs.is_empty() {
            bail!("workflow has no jobs");
        }
        for id in wf.jobs.keys() {
            let job = wf.job(id)?;
            for need in &job.needs {
                if !wf.jobs.contains_key(need) {
                    bail!("job {id:?} needs unknown job {need:?}");
                }
            }
            if job.uses.is_some() {
                continue;
            }
            if job.steps.is_empty() {
                bail!("job {id:?} has no steps");
            }
            for (i, s) in job.steps.iter().enumerate() {
                match (&s.uses, &s.run) {
                    (Some(_), None) | (None, Some(_)) => {}
                    _ => bail!("job {id:?} step {}: needs exactly one of `uses` or `run`", i + 1),
                }
            }
        }
        wf.check_cycles()?;
        Ok(wf)
    }

    pub fn job(&self, id: &str) -> Result<Job> {
        serde_json::from_value(self.jobs[id].clone()).with_context(|| format!("invalid job {id:?}"))
    }

    fn check_cycles(&self) -> Result<()> {
        fn visit<'a>(
            wf: &'a Workflow,
            id: &'a str,
            stack: &mut Vec<&'a str>,
            done: &mut std::collections::HashSet<&'a str>,
        ) -> Result<()> {
            if done.contains(id) {
                return Ok(());
            }
            if stack.contains(&id) {
                bail!("job dependency cycle: {} -> {id}", stack.join(" -> "));
            }
            stack.push(id);
            let needs = wf.jobs[id]
                .get("needs")
                .map(|n| match n {
                    Value::String(s) => vec![s.as_str()],
                    Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
                    _ => vec![],
                })
                .unwrap_or_default();
            for n in needs {
                if let Some((k, _)) = wf.jobs.get_key_value(n) {
                    visit(wf, k, stack, done)?;
                }
            }
            stack.pop();
            done.insert(id);
            Ok(())
        }
        let mut done = std::collections::HashSet::new();
        for id in self.jobs.keys() {
            visit(self, id, &mut Vec::new(), &mut done)?;
        }
        Ok(())
    }

    /// Display name: `name:` or the file name.
    pub fn display_name(&self, file: &str) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| file.rsplit('/').next().unwrap_or(file).to_string())
    }
}

/// `action.yml` metadata.
#[derive(Debug, Clone, Deserialize)]
pub struct Action {
    pub name: Option<String>,
    #[serde(default)]
    pub inputs: Map<String, Value>,
    #[serde(default)]
    pub outputs: Map<String, Value>,
    pub runs: ActionRuns,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ActionRuns {
    pub using: String,
    // node
    pub main: Option<String>,
    pub pre: Option<String>,
    pub post: Option<String>,
    pub pre_if: Option<String>,
    pub post_if: Option<String>,
    // docker
    pub image: Option<String>,
    #[serde(default)]
    pub args: Vec<Value>,
    pub entrypoint: Option<String>,
    pub pre_entrypoint: Option<String>,
    pub post_entrypoint: Option<String>,
    #[serde(default)]
    pub env: Map<String, Value>,
    // composite
    #[serde(default)]
    pub steps: Vec<Step>,
}

impl Action {
    pub fn parse(src: &str) -> Result<Self> {
        let v = yaml_to_json(src).context("invalid action.yml")?;
        serde_json::from_value(v).context("invalid action.yml")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_shapes() {
        let wf = Workflow::parse(
            r#"
name: CI
on:
  push:
    branches: [main]
env: { A: 1 }
jobs:
  build:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        node: [18, 20]
    steps:
      - uses: actions/checkout@v4
      - run: npm test
        env: { CI: true }
  deploy:
    needs: build
    if: github.ref == 'refs/heads/main'
    runs-on: [self-hosted, linux]
    container: node:20
    steps:
      - run: echo hi
"#,
        )
        .unwrap();
        assert_eq!(wf.jobs.keys().collect::<Vec<_>>(), ["build", "deploy"]);
        let deploy = wf.job("deploy").unwrap();
        assert_eq!(deploy.needs, ["build"]);
        assert_eq!(deploy.container, Some(Value::String("node:20".into())));
        let build = wf.job("build").unwrap();
        assert_eq!(build.steps[0].uses.as_deref(), Some("actions/checkout@v4"));
        assert!(build.strategy.unwrap().matrix.is_some());
        assert_eq!(wf.display_name(".github/workflows/ci.yml"), "CI");
    }

    #[test]
    fn rejects_bad_workflows() {
        let base = "on: push\njobs:\n";
        for (body, err) in [
            ("  a:\n    needs: b\n    steps: [{run: x}]\n", "unknown job"),
            ("  a:\n    steps: []\n", "no steps"),
            ("  a:\n    steps: [{run: x, uses: y}]\n", "exactly one"),
            (
                "  a:\n    needs: b\n    steps: [{run: x}]\n  b:\n    needs: a\n    steps: [{run: x}]\n",
                "cycle",
            ),
        ] {
            let e = Workflow::parse(&format!("{base}{body}")).unwrap_err();
            assert!(format!("{e:#}").contains(err), "{body}: {e:#}");
        }
        assert!(Workflow::parse("jobs:\n  a:\n    steps: [{run: x}]\n").is_err());
    }

    #[test]
    fn merge_keys_apply() {
        let wf = Workflow::parse(
            "on: push\nx: &s\n  runs-on: ubuntu-latest\njobs:\n  a:\n    <<: *s\n    steps: [{run: x}]\n",
        )
        .unwrap();
        assert!(wf.job("a").unwrap().runs_on.is_some());
    }
}
