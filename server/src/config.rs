//! `.moloko.yml` pipeline definitions: parsing, validation and shard expansion.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

pub const PIPELINE_FILE: &str = ".moloko.yml";
const MAX_SHARDS: u32 = 256;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pipeline {
    pub jobs: BTreeMap<String, JobSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSpec {
    /// Jobs that must succeed before this one starts.
    #[serde(default)]
    pub needs: Vec<String>,
    /// Run steps inside this Docker image instead of directly on the host.
    pub image: Option<String>,
    /// Split the job into N parallel instances (MOLOKO_SHARD_INDEX / MOLOKO_SHARD_TOTAL).
    pub shards: Option<u32>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cache: Vec<CacheSpec>,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Step {
    Run(String),
    Named { name: Option<String>, run: String },
}

impl Step {
    pub fn script(&self) -> &str {
        match self {
            Step::Run(s) | Step::Named { run: s, .. } => s,
        }
    }
    pub fn label(&self) -> String {
        match self {
            Step::Named { name: Some(n), .. } => n.clone(),
            _ => self.script().lines().next().unwrap_or("").to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheSpec {
    /// Directory (relative to the workspace) to restore before and save after the job.
    pub path: String,
    /// Files whose contents form the cache key, e.g. `Cargo.lock`.
    #[serde(default)]
    pub key_files: Vec<String>,
}

/// One schedulable unit after shard expansion.
#[derive(Debug, Clone, PartialEq)]
pub struct JobInstance {
    /// Unique within a run, e.g. `test` or `test[2/4]`.
    pub name: String,
    /// The job key in `.moloko.yml`.
    pub base: String,
    /// (index, total), 0-based index.
    pub shard: Option<(u32, u32)>,
    /// Instance names this one waits for.
    pub needs: Vec<String>,
}

impl Pipeline {
    pub fn parse(src: &str) -> Result<Self> {
        let p: Pipeline = serde_yaml::from_str(src).context("invalid .moloko.yml")?;
        p.validate()?;
        Ok(p)
    }

    fn validate(&self) -> Result<()> {
        if self.jobs.is_empty() {
            bail!("pipeline has no jobs");
        }
        for (name, job) in &self.jobs {
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                bail!("job name {name:?} may only contain letters, digits, '-' and '_'");
            }
            if job.steps.is_empty() {
                bail!("job {name:?} has no steps");
            }
            if let Some(n) = job.shards
                && (n == 0 || n > MAX_SHARDS)
            {
                bail!("job {name:?}: shards must be between 1 and {MAX_SHARDS}");
            }
            for need in &job.needs {
                if !self.jobs.contains_key(need) {
                    bail!("job {name:?} needs unknown job {need:?}");
                }
            }
            for c in &job.cache {
                let p = std::path::Path::new(&c.path);
                if c.path.is_empty()
                    || p.is_absolute()
                    || p.components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    bail!(
                        "job {name:?}: cache path {:?} must be relative and stay inside the workspace",
                        c.path
                    );
                }
            }
        }
        self.check_cycles()
    }

    fn check_cycles(&self) -> Result<()> {
        // 0 = unvisited, 1 = in progress, 2 = done
        fn visit<'a>(p: &'a Pipeline, n: &'a str, state: &mut HashMap<&'a str, u8>) -> Result<()> {
            match state.get(n) {
                Some(1) => bail!("dependency cycle involving job {n:?}"),
                Some(2) => return Ok(()),
                _ => {}
            }
            state.insert(n, 1);
            for need in &p.jobs[n].needs {
                visit(p, need, state)?;
            }
            state.insert(n, 2);
            Ok(())
        }
        let mut state = HashMap::new();
        for name in self.jobs.keys() {
            visit(self, name, &mut state)?;
        }
        Ok(())
    }

    fn instance_names(&self, base: &str) -> Vec<String> {
        match self.jobs[base].shards {
            Some(n) if n > 1 => (1..=n).map(|i| format!("{base}[{i}/{n}]")).collect(),
            _ => vec![base.to_string()],
        }
    }

    /// Expand shards into instances. Depending on a sharded job waits for all its shards.
    pub fn expand(&self) -> Vec<JobInstance> {
        let mut out = Vec::new();
        for (base, job) in &self.jobs {
            let needs: Vec<String> = job
                .needs
                .iter()
                .flat_map(|n| self.instance_names(n))
                .collect();
            let total = job.shards.unwrap_or(1);
            for (i, name) in self.instance_names(base).into_iter().enumerate() {
                out.push(JobInstance {
                    name,
                    base: base.clone(),
                    shard: (total > 1).then_some((i as u32, total)),
                    needs: needs.clone(),
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_expands_shards() {
        let p = Pipeline::parse(
            r#"
jobs:
  build:
    steps: [cargo build]
  test:
    needs: [build]
    shards: 3
    steps:
      - name: unit
        run: cargo test
  deploy:
    needs: [test]
    steps: [./deploy.sh]
"#,
        )
        .unwrap();
        let jobs = p.expand();
        assert_eq!(jobs.len(), 5);
        let t2 = jobs.iter().find(|j| j.name == "test[2/3]").unwrap();
        assert_eq!(t2.shard, Some((1, 3)));
        assert_eq!(t2.needs, vec!["build"]);
        let deploy = jobs.iter().find(|j| j.name == "deploy").unwrap();
        assert_eq!(deploy.needs, vec!["test[1/3]", "test[2/3]", "test[3/3]"]);
    }

    #[test]
    fn rejects_cycles_unknown_needs_and_bad_cache_paths() {
        let cycle = "jobs:\n  a: {needs: [b], steps: [x]}\n  b: {needs: [a], steps: [x]}\n";
        assert!(
            Pipeline::parse(cycle)
                .unwrap_err()
                .to_string()
                .contains("cycle")
        );
        let unknown = "jobs:\n  a: {needs: [nope], steps: [x]}\n";
        assert!(
            Pipeline::parse(unknown)
                .unwrap_err()
                .to_string()
                .contains("unknown")
        );
        let escape = "jobs:\n  a: {steps: [x], cache: [{path: ../etc}]}\n";
        assert!(Pipeline::parse(escape).is_err());
        let empty = "jobs:\n  a: {steps: []}\n";
        assert!(Pipeline::parse(empty).is_err());
    }
}
