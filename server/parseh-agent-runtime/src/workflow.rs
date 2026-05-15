//! Minimal workflow chainer — the n8n-analogue.
//!
//! A [`Workflow`] is a DAG of agent steps. Each [`WorkflowStep`] names
//! an agent (by [`AgentId`]), declares which prior steps it depends on,
//! and supplies an `input_mapping` JSON template wiring prior step
//! outputs into this step's input. Steps run in topological order; the
//! last step's output is the workflow result.
//!
//! This is the "build an automation graph powered by free network AI"
//! primitive from [the project notes](the project notes) —
//! **locally for now**. Distributed execution across the volunteer
//! network needs the network (bootstrap servers, not provisioned); see
//! the crate-level docs.
//!
//! ## Input mapping
//!
//! `input_mapping` is a JSON value. Any string of the exact form
//! `{{steps.<step_id>.output<.path>}}` or
//! `{{initial.<path>}}` is replaced by the referenced JSON value
//! (whole-string substitution preserving the original JSON type — so a
//! step can pass a number/object through, not just text). Strings
//! without a recognised placeholder pass through literally. This is the
//! same conservative no-code-execution stance as [`crate::render_prompt`].
//!
//! Cycles are rejected before any agent runs.

use crate::backend::LlmBackend;
use crate::executor::{AgentExecutor, ExecError};
use parseh_agent_spec::{AgentDefinition, AgentId};
use parseh_task::JobSpec;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use thiserror::Error;

/// Errors raised while building or running a [`Workflow`].
#[derive(Error, Debug)]
pub enum WorkflowError {
    /// Two steps share a `step_id`.
    #[error("duplicate step_id `{0}`")]
    DuplicateStepId(String),
    /// A step depends on a `step_id` that does not exist.
    #[error("step `{step}` depends on unknown step `{dep}`")]
    UnknownDependency {
        /// The step with the bad dependency.
        step: String,
        /// The dependency that does not exist.
        dep: String,
    },
    /// The dependency graph contains a cycle.
    #[error("workflow dependency graph contains a cycle (involving steps: {0})")]
    CycleDetected(String),
    /// A referenced agent id was not in the supplied registry.
    #[error("step `{step}` references agent {agent} not in the registry")]
    AgentNotFound {
        /// The step.
        step: String,
        /// The missing agent id (hex).
        agent: String,
    },
    /// An `input_mapping` placeholder referenced an unknown step /
    /// output path.
    #[error("step `{step}` input_mapping references undefined path `{path}`")]
    UndefinedMappingPath {
        /// The step whose mapping was bad.
        step: String,
        /// The unresolved `{{...}}` path.
        path: String,
    },
    /// The workflow had no steps.
    #[error("workflow has no steps")]
    Empty,
    /// Executing the step's agent failed.
    #[error("step `{step}` execution failed: {source}")]
    StepFailed {
        /// The step that failed.
        step: String,
        /// The underlying executor error.
        #[source]
        source: ExecError,
    },
}

/// One node in the workflow DAG.
#[derive(Clone, Debug)]
pub struct WorkflowStep {
    /// Unique identifier within the workflow.
    pub step_id: String,
    /// The agent this step runs, looked up in the registry passed to
    /// [`Workflow::run`].
    pub agent_id: AgentId,
    /// JSON template producing this step's input. `{{steps.X.output}}`
    /// and `{{initial}}` placeholders are resolved at run time.
    pub input_mapping: Value,
    /// Step ids this step consumes output from. Must be a subset of
    /// the steps that precede it topologically.
    pub depends_on: Vec<String>,
}

/// A workflow: a set of steps forming a DAG.
#[derive(Clone, Debug)]
pub struct Workflow {
    /// The steps. Order in this vec does NOT matter — [`Workflow::run`]
    /// topologically sorts them and rejects cycles.
    pub steps: Vec<WorkflowStep>,
}

/// The result of running a workflow.
#[derive(Clone, Debug)]
pub struct WorkflowResult {
    /// Output JSON of every step, keyed by `step_id`.
    pub step_outputs: BTreeMap<String, Value>,
    /// `step_id` of the terminal step (the one no other step depends
    /// on; if several, the last in topological order).
    pub final_step_id: String,
    /// Convenience: the terminal step's output.
    pub final_output: Value,
}

impl Workflow {
    /// Build a workflow, validating ids + dependencies eagerly.
    ///
    /// # Errors
    ///
    /// [`WorkflowError::DuplicateStepId`], [`WorkflowError::Empty`], or
    /// [`WorkflowError::UnknownDependency`].
    pub fn new(steps: Vec<WorkflowStep>) -> Result<Self, WorkflowError> {
        if steps.is_empty() {
            return Err(WorkflowError::Empty);
        }
        let mut seen = HashSet::new();
        for s in &steps {
            if !seen.insert(s.step_id.clone()) {
                return Err(WorkflowError::DuplicateStepId(s.step_id.clone()));
            }
        }
        for s in &steps {
            for d in &s.depends_on {
                if !seen.contains(d) {
                    return Err(WorkflowError::UnknownDependency {
                        step: s.step_id.clone(),
                        dep: d.clone(),
                    });
                }
            }
        }
        Ok(Self { steps })
    }

    /// Topologically sort the steps (Kahn's algorithm). Rejects cycles.
    fn topo_order(&self) -> Result<Vec<usize>, WorkflowError> {
        let index: HashMap<&str, usize> = self
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| (s.step_id.as_str(), i))
            .collect();
        let mut indegree = vec![0usize; self.steps.len()];
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); self.steps.len()];
        for (i, s) in self.steps.iter().enumerate() {
            for d in &s.depends_on {
                // Safe: validated in `new`.
                let di = index[d.as_str()];
                adj[di].push(i);
                indegree[i] += 1;
            }
        }
        // Deterministic ordering: process ready nodes in original
        // declaration order so the workflow result is reproducible.
        let mut ready: Vec<usize> = (0..self.steps.len())
            .filter(|&i| indegree[i] == 0)
            .collect();
        let mut order = Vec::with_capacity(self.steps.len());
        let mut cursor = 0;
        while cursor < ready.len() {
            let n = ready[cursor];
            cursor += 1;
            order.push(n);
            for &m in &adj[n] {
                indegree[m] -= 1;
                if indegree[m] == 0 {
                    ready.push(m);
                }
            }
        }
        if order.len() != self.steps.len() {
            let in_cycle: Vec<String> = (0..self.steps.len())
                .filter(|&i| !order.contains(&i))
                .map(|i| self.steps[i].step_id.clone())
                .collect();
            return Err(WorkflowError::CycleDetected(in_cycle.join(", ")));
        }
        Ok(order)
    }

    /// Run the workflow against `executor`, looking agents up in
    /// `registry`. `initial_input` is exposed to step mappings as
    /// `{{initial...}}`. Every step is dispatched with the same
    /// `spec` (so the seed — and thus determinism — is uniform across
    /// the graph).
    ///
    /// # Errors
    ///
    /// Any [`WorkflowError`] variant; cycles are caught before the
    /// first agent runs.
    pub async fn run<B: LlmBackend>(
        &self,
        executor: &AgentExecutor<B>,
        registry: &HashMap<AgentId, AgentDefinition>,
        spec: &JobSpec,
        initial_input: Value,
    ) -> Result<WorkflowResult, WorkflowError> {
        let order = self.topo_order()?;

        let mut step_outputs: BTreeMap<String, Value> = BTreeMap::new();
        for &idx in &order {
            let step = &self.steps[idx];
            let agent = registry.get(&step.agent_id).ok_or_else(|| {
                WorkflowError::AgentNotFound {
                    step: step.step_id.clone(),
                    agent: step.agent_id.as_hex(),
                }
            })?;
            // Resolve the input mapping against initial + prior outputs.
            let input = resolve_mapping(
                &step.input_mapping,
                &initial_input,
                &step_outputs,
                &step.step_id,
            )?;
            let outcome = executor
                .execute(agent, spec, input)
                .await
                .map_err(|e| WorkflowError::StepFailed {
                    step: step.step_id.clone(),
                    source: e,
                })?;
            step_outputs.insert(step.step_id.clone(), outcome.output_json);
        }

        // Terminal step = last in topological order.
        let final_idx = *order.last().ok_or(WorkflowError::Empty)?;
        let final_step_id = self.steps[final_idx].step_id.clone();
        let final_output = step_outputs
            .get(&final_step_id)
            .cloned()
            .unwrap_or(Value::Null);
        Ok(WorkflowResult {
            step_outputs,
            final_step_id,
            final_output,
        })
    }
}

/// Recursively resolve `{{steps.X.output...}}` / `{{initial...}}`
/// placeholders inside a mapping value, preserving JSON types for
/// whole-string placeholders.
fn resolve_mapping(
    mapping: &Value,
    initial: &Value,
    outputs: &BTreeMap<String, Value>,
    step_id: &str,
) -> Result<Value, WorkflowError> {
    match mapping {
        Value::String(s) => {
            if let Some(path) = whole_placeholder(s) {
                resolve_path(path, initial, outputs).ok_or_else(|| {
                    WorkflowError::UndefinedMappingPath {
                        step: step_id.to_string(),
                        path: path.to_string(),
                    }
                })
            } else {
                Ok(Value::String(s.clone()))
            }
        }
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for v in arr {
                out.push(resolve_mapping(v, initial, outputs, step_id)?);
            }
            Ok(Value::Array(out))
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), resolve_mapping(v, initial, outputs, step_id)?);
            }
            Ok(Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

/// If `s` is exactly `{{ ... }}`, return the trimmed inner path.
fn whole_placeholder(s: &str) -> Option<&str> {
    let t = s.trim();
    let inner = t.strip_prefix("{{")?.strip_suffix("}}")?;
    Some(inner.trim())
}

/// Resolve `steps.<id>.output[.path]` or `initial[.path]`.
fn resolve_path(
    path: &str,
    initial: &Value,
    outputs: &BTreeMap<String, Value>,
) -> Option<Value> {
    let mut segs = path.split('.');
    match segs.next()? {
        "initial" => walk(initial, segs),
        "steps" => {
            let step_id = segs.next()?;
            // The grammar is `steps.<id>.output[.<path>]`.
            if segs.next()? != "output" {
                return None;
            }
            let base = outputs.get(step_id)?;
            walk(base, segs)
        }
        _ => None,
    }
}

/// Walk the remaining dotted segments into a JSON value. A segment
/// that parses as `usize` indexes into an array; otherwise it is an
/// object key.
fn walk<'a, I: Iterator<Item = &'a str>>(base: &Value, segs: I) -> Option<Value> {
    let mut cur = base;
    for seg in segs {
        cur = match (cur, seg.parse::<usize>()) {
            (Value::Array(arr), Ok(idx)) => arr.get(idx)?,
            _ => cur.get(seg)?,
        };
    }
    Some(cur.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step(id: &str, deps: &[&str]) -> WorkflowStep {
        WorkflowStep {
            step_id: id.to_string(),
            agent_id: AgentId::zero(),
            input_mapping: json!({}),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn duplicate_step_id_rejected() {
        let err = Workflow::new(vec![step("a", &[]), step("a", &[])]).unwrap_err();
        assert!(matches!(err, WorkflowError::DuplicateStepId(_)));
    }

    #[test]
    fn unknown_dependency_rejected() {
        let err = Workflow::new(vec![step("a", &["ghost"])]).unwrap_err();
        assert!(matches!(err, WorkflowError::UnknownDependency { .. }));
    }

    #[test]
    fn cycle_detected() {
        // a -> b -> a
        let wf = Workflow::new(vec![step("a", &["b"]), step("b", &["a"])]).unwrap();
        let err = wf.topo_order().unwrap_err();
        assert!(matches!(err, WorkflowError::CycleDetected(_)));
    }

    #[test]
    fn linear_topo_order_is_deterministic() {
        let wf = Workflow::new(vec![
            step("c", &["b"]),
            step("a", &[]),
            step("b", &["a"]),
        ])
        .unwrap();
        let order = wf.topo_order().unwrap();
        let ids: Vec<&str> = order.iter().map(|&i| wf.steps[i].step_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn whole_placeholder_path_resolves_with_type() {
        let mut outs = BTreeMap::new();
        outs.insert("s1".to_string(), json!({"n": 7}));
        let v = resolve_path("steps.s1.output.n", &json!(null), &outs).unwrap();
        assert_eq!(v, json!(7)); // number preserved, not stringified
    }
}
