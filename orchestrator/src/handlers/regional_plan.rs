//! Immutable, versioned program saved before a regional rollout executes.
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Plan {
    pub version: u32,
    pub steps: Vec<Step>,
    pub rollback_steps: Vec<Step>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Step {
    pub id: String,
    pub depends_on: Option<String>,
    pub phase: String,
    pub region: Option<String>,
    pub region_index: usize,
    pub slot_index: usize,
    pub candidate_id: Option<String>,
}

impl Plan {
    pub fn compile(regions: &[String], slots: &[(String, String)]) -> Self {
        let mut plan = Self { version: 1, steps: Vec::new(), rollback_steps: Vec::new() };
        for (region_index, region) in regions.iter().enumerate() {
            let candidates: Vec<_> = slots.iter().filter(|(r, _)| r == region).collect();
            let mut append = |phase: &str, slot_index, candidate_id| {
                push(&mut plan.steps, phase, Some(region.clone()), region_index, slot_index, candidate_id);
            };
            for phase in ["preflight", "exclude_region", "wait_drained"] {
                append(phase, 0, None);
            }
            for (index, (_, id)) in candidates.iter().enumerate() {
                append("create_slot", index, Some(id.clone()));
                append("creating", index, Some(id.clone()));
            }
            for phase in ["create_slot", "probe_candidates", "mark_old_draining",
                "restore_region", "wait_restored", "bake"] {
                append(phase, candidates.len(), None);
            }
            for phase in ["rollback_restore", "rollback_wait"] {
                push(&mut plan.rollback_steps, phase, Some(region.clone()), region_index, 0, None);
            }
        }
        push(&mut plan.steps, "verify", None, regions.len(), 0, None);
        push(&mut plan.steps, "passed", None, regions.len(), 0, None);
        push(&mut plan.rollback_steps, "rollback_restore", None, regions.len(), 0, None);
        push(&mut plan.rollback_steps, "rolled_back", None, regions.len(), 0, None);
        plan
    }

    pub fn step(&self, phase: &str, region: usize, slot: usize) -> Result<&Step> {
        if self.version != 1 {
            bail!("unsupported regional plan version {}; refusing to reinterpret it", self.version);
        }
        self.steps.iter().chain(&self.rollback_steps)
            .find(|s| s.phase == phase && s.region_index == region && s.slot_index == slot)
            .ok_or_else(|| anyhow::anyhow!("execution cursor is not in the persisted rollout plan"))
    }
}

fn push(steps: &mut Vec<Step>, phase: &str, region: Option<String>, region_index: usize,
    slot_index: usize, candidate_id: Option<String>) {
    steps.push(Step {
        id: format!("{region_index}:{phase}:{slot_index}"),
        depends_on: steps.last().map(|s| s.id.clone()),
        phase: phase.into(), region, region_index, slot_index, candidate_id,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_asymmetric_plan_keeps_order_dependencies_and_candidate_identity() {
        let plan = Plan::compile(&["us".into(), "eu".into()], &[
            ("eu".into(), "eu-a".into()), ("us".into(), "us-a".into()),
            ("eu".into(), "eu-b".into()),
        ]);
        let mut restored: Plan = serde_json::from_value(serde_json::to_value(&plan).unwrap()).unwrap();
        assert_eq!(restored.step("creating", 0, 0).unwrap().candidate_id.as_deref(), Some("us-a"));
        assert_eq!(restored.step("creating", 1, 1).unwrap().candidate_id.as_deref(), Some("eu-b"));
        assert!(restored.step("creating", 0, 1).is_err());
        let first_eu = restored.steps.iter().position(|s| s.phase == "preflight" && s.region_index == 1).unwrap();
        assert_eq!(restored.steps[first_eu].depends_on.as_deref(), Some("0:bake:1"));
        for pair in restored.steps.windows(2) {
            assert_eq!(pair[1].depends_on.as_ref(), Some(&pair[0].id));
        }
        restored.version = 2;
        assert!(restored.step("preflight", 0, 0).is_err());
    }
}
