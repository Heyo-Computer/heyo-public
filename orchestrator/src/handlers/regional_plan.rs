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

    /// Routing-only transitions use the same durable cursor and item journal,
    /// but never create candidates or reinterpret an admitted legacy plan.
    pub fn policy_transition(region: Option<String>) -> Self {
        let mut plan = Self { version: 2, steps: Vec::new(), rollback_steps: Vec::new() };
        for phase in ["publish_policy", "wait_policy_prepared", "activate_policy", "wait_policy_adopted"] {
            push(&mut plan.steps, phase, region.clone(), 0, 0, None);
        }
        if region.is_some() {
            for phase in ["wait_assignments_drained", "close_peer_admission", "wait_admission_drained"] {
                push(&mut plan.steps, phase, region.clone(), 0, 0, None);
            }
        }
        push(&mut plan.steps, "passed", region, 0, 0, None);
        plan
    }

    /// V3 reserves policy slots 0/1 for forward withdrawal/restoration and 2/3
    /// for rollback. Candidate ordinals are independent because their phase is
    /// distinct. Rollback enters the current region's suffix, restoring it
    /// before withdrawing any already-updated surviving region.
    pub fn application(regions: &[String], slots: &[(String, String)]) -> Result<Self> {
        let unique: std::collections::HashSet<_> = regions.iter().collect();
        anyhow::ensure!(!regions.is_empty() && unique.len() == regions.len()
            && regions.iter().all(|r| !r.is_empty()), "application plan needs distinct ordered regions");
        let candidates: std::collections::HashSet<_> = slots.iter().map(|(_,id)| id).collect();
        anyhow::ensure!(candidates.len() == slots.len() && slots.iter().all(|(r,id)| unique.contains(r) && !id.is_empty())
            && regions.iter().all(|r| slots.iter().any(|(s,_)| s == r)), "application plan needs uniquely identified candidates in every region");
        let mut plan = Self { version: 3, steps: Vec::new(), rollback_steps: Vec::new() };
        for (index, region) in regions.iter().enumerate() {
            push(&mut plan.steps, "preflight", Some(region.clone()), index, 0, None);
            policy_steps(&mut plan.steps, Some(region.clone()), index, 0);
            for (slot, (_,id)) in slots.iter().filter(|(r,_)| r == region).enumerate() {
                push(&mut plan.steps, "create_candidate", Some(region.clone()), index, slot, Some(id.clone()));
            }
            push(&mut plan.steps, "probe_candidates", Some(region.clone()), index, 0, None);
            policy_steps(&mut plan.steps, None, index, 1);
            push(&mut plan.steps, "bake", Some(region.clone()), index, 1, None);
        }
        let last = regions.len() - 1;
        push(&mut plan.steps, "verify", Some(regions[last].clone()), last, 1, None);
        push(&mut plan.steps, "passed", Some(regions[last].clone()), last, 1, None);
        for (index, region) in regions.iter().enumerate().rev() {
            push(&mut plan.rollback_steps, "rollback_entry", Some(region.clone()), index, 2, None);
            policy_steps(&mut plan.rollback_steps, Some(region.clone()), index, 2);
            push(&mut plan.rollback_steps, "probe_retained", Some(region.clone()), index, 2, None);
            policy_steps(&mut plan.rollback_steps, None, index, 3);
            push(&mut plan.rollback_steps, "bake", Some(region.clone()), index, 3, None);
        }
        push(&mut plan.rollback_steps, "verify_baseline", Some(regions[0].clone()), 0, 3, None);
        push(&mut plan.rollback_steps, "rolled_back", Some(regions[0].clone()), 0, 3, None);
        Ok(plan)
    }

    pub fn step(&self, phase: &str, region: usize, slot: usize) -> Result<&Step> {
        if !matches!(self.version, 1..=3) {
            bail!("unsupported regional plan version {}; refusing to reinterpret it", self.version);
        }
        self.steps.iter().chain(&self.rollback_steps)
            .find(|s| s.phase == phase && s.region_index == region && s.slot_index == slot)
            .ok_or_else(|| anyhow::anyhow!("execution cursor is not in the persisted rollout plan"))
    }

    pub fn publication(&self, step_id: &str) -> Result<&Step> {
        anyhow::ensure!(matches!(self.version, 2 | 3), "policy publication requires a hierarchical plan");
        self.steps.iter().chain(self.rollback_steps.iter().filter(|_| self.version == 3))
            .find(|step| step.id == step_id && step.phase == "publish_policy")
            .ok_or_else(|| anyhow::anyhow!("proposal does not name a persisted publication step"))
    }

    pub fn successor(&self, step_id: &str) -> Result<&Step> {
        let steps = if self.steps.iter().any(|s| s.id == step_id) { &self.steps }
            else if self.rollback_steps.iter().any(|s| s.id == step_id) { &self.rollback_steps }
            else { bail!("unknown plan item"); };
        steps.iter().find(|s| s.depends_on.as_deref() == Some(step_id))
            .ok_or_else(|| anyhow::anyhow!("plan item has no persisted successor"))
    }
}

fn policy_steps(steps: &mut Vec<Step>, region: Option<String>, region_index: usize, slot_index: usize) {
    for phase in ["publish_policy", "wait_policy_prepared", "activate_policy", "wait_policy_adopted"] {
        push(steps, phase, region.clone(), region_index, slot_index, None);
    }
    if region.is_some() {
        for phase in ["wait_assignments_drained", "close_peer_admission", "wait_admission_drained"] {
            push(steps, phase, region.clone(), region_index, slot_index, None);
        }
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
        restored.version = 4;
        assert!(restored.step("preflight", 0, 0).is_err());
    }

    #[test]
    fn application_policy_occurrences_and_reverse_rollback_suffixes_are_distinct() -> Result<()> {
        let plan = Plan::application(&["us3".into(),"eu1".into()], &[
            ("eu1".into(),"eu-a".into()),("us3".into(),"us-a".into()),("eu1".into(),"eu-b".into())])?;
        let plan: Plan = serde_json::from_value(serde_json::to_value(plan)?)?;
        let mut ids = std::collections::HashSet::new();
        for steps in [&plan.steps,&plan.rollback_steps] {
            for item in steps { assert!(ids.insert(&item.id), "duplicate {}",item.id); }
            for pair in steps.windows(2) {
                assert_eq!(pair[1].depends_on.as_deref(),Some(pair[0].id.as_str()));
                assert_eq!(plan.successor(&pair[0].id)?.id,pair[1].id);
            }
        }
        for region in 0..2 {
            for slot in 0..4 {
                let publication = plan.publication(&format!("{region}:publish_policy:{slot}"))?;
                assert_eq!(publication.region.is_some(),slot % 2 == 0);
                assert!(plan.step("activate_policy",region,slot).is_ok());
            }
        }
        assert_eq!(plan.step("create_candidate",1,1)?.candidate_id.as_deref(),Some("eu-b"));
        assert_eq!(plan.rollback_steps[0].id,"1:rollback_entry:2");
        assert_eq!(plan.successor("1:bake:3")?.id,"0:rollback_entry:2");
        assert_eq!(plan.steps.last().unwrap().region_index,1,"terminal verification must retain the rollback frontier");
        assert!(plan.rollback_steps.iter().all(|s| s.candidate_id.is_none()));
        assert!(Plan::application(&[],&[]).is_err());
        assert!(Plan::application(&["us3".into()],&[("eu1".into(),"wrong".into())]).is_err());
        assert!(Plan::application(&["us3".into()],&[("us3".into(),"same".into()),("us3".into(),"same".into())]).is_err());
        Ok(())
    }

    #[test]
    fn policy_plan_separates_preparation_adoption_and_both_drain_barriers() {
        let plan = Plan::policy_transition(Some("eu1".into()));
        let restored: Plan = serde_json::from_value(serde_json::to_value(plan).unwrap()).unwrap();
        assert_eq!(restored.version, 2);
        assert_eq!(restored.steps.iter().map(|s| s.phase.as_str()).collect::<Vec<_>>(),
            ["publish_policy", "wait_policy_prepared", "activate_policy", "wait_policy_adopted",
                "wait_assignments_drained", "close_peer_admission", "wait_admission_drained", "passed"]);
        for pair in restored.steps.windows(2) {
            assert_eq!(pair[1].depends_on.as_ref(), Some(&pair[0].id));
        }
        assert!(restored.steps.iter().all(|s| s.candidate_id.is_none()));
        assert!(restored.step("create_slot", 0, 0).is_err());
        assert!(restored.step("activate_policy", 0, 0).is_ok());
        let weights = Plan::policy_transition(None);
        assert_eq!(weights.steps.iter().map(|s| s.phase.as_str()).collect::<Vec<_>>(),
            ["publish_policy", "wait_policy_prepared", "activate_policy", "wait_policy_adopted", "passed"]);
        assert!(weights.steps.iter().all(|s| s.region.is_none() && s.candidate_id.is_none()));
    }

    #[tokio::test]
    #[ignore = "requires ORCHESTRATOR_TEST_DATABASE_URL (disposable PostgreSQL)"]
    async fn application_journal_enforces_rollback_entry_dependencies_and_no_rewind_postgres() -> Result<()> {
        use sea_orm::{ConnectionTrait, DbBackend, Statement};
        async fn insert(db: &sea_orm::DatabaseConnection, id: &str, plan: &Plan) -> Result<()> {
            let first = &plan.steps[0];
            db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
                "INSERT INTO regional_service_rollouts(operation_id,service_id,request_hash,deployment_request,target_revision,observer_topology,baseline_state,regions,slots,plan,minimum_serving_replicas,bake_seconds,drain_timeout_seconds,status,phase,region_index,slot_index)
                 VALUES($1,$1,'hash','{}','rev','[]','{}','[\"us3\",\"eu1\"]','[]',$2,1,1,30,'running',$3,$4,$5)",
                vec![id.into(),serde_json::to_value(plan)?.into(),first.phase.clone().into(),
                    i32::try_from(first.region_index)?.into(),i32::try_from(first.slot_index)?.into()])).await?;
            Ok(())
        }
        async fn move_to(db: &sea_orm::DatabaseConnection, id: &str, step: &Step) -> Result<()> {
            let status = match step.phase.as_str() { "passed" => "passed", "rolled_back" => "rolled_back", _ => "running" };
            db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
                "UPDATE regional_service_rollouts SET phase=$2,region_index=$3,slot_index=$4,status=$5 WHERE operation_id=$1",
                vec![id.into(),step.phase.clone().into(),i32::try_from(step.region_index)?.into(),
                    i32::try_from(step.slot_index)?.into(),status.into()])).await?;
            Ok(())
        }
        let url = std::env::var("ORCHESTRATOR_TEST_DATABASE_URL")?;
        let root = sea_orm::Database::connect(url.clone()).await?;
        let schema = format!("application_journal_{}", uuid::Uuid::new_v4().simple());
        root.execute_unprepared(&format!("CREATE SCHEMA {schema}")).await?;
        let mut options = sea_orm::ConnectOptions::new(url);
        options.set_schema_search_path(schema.clone());
        let db = sea_orm::Database::connect(options.clone()).await?;
        for migration in [include_str!("../../migrations/031_add_service_discovery.sql"),
            include_str!("../../migrations/035_add_regional_service_rollouts.sql"),
            include_str!("../../migrations/040_add_application_plan_journal.sql"),
            include_str!("../../migrations/040_add_application_plan_journal.sql")] {
            db.execute_unprepared(migration).await?;
        }
        let plan = Plan::application(&["us3".into(),"eu1".into()],&[("us3".into(),"us-a".into()),("eu1".into(),"eu-a".into())])?;
        for (index, frontier) in ["0:preflight:0","0:create_candidate:0","1:probe_candidates:0","1:verify:1"].iter().enumerate() {
            let id = format!("application-journal-{index}");
            insert(&db,&id,&plan).await?;
            let position = plan.steps.iter().position(|s| s.id == *frontier).unwrap();
            // Only the journal is under test: these cursor updates deliberately
            // do not claim to execute policy effects or acknowledge traffic.
            for step in plan.steps.iter().skip(1).take(position) { move_to(&db,&id,step).await?; }
            let current = &plan.steps[position];
            assert!(move_to(&db,&id,plan.step("activate_policy",current.region_index,2)?).await.is_err());
            assert!(move_to(&db,&id,plan.step("rollback_entry",1-current.region_index,2)?).await.is_err());
            db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
                "UPDATE regional_service_rollouts SET status='blocked' WHERE operation_id=$1",[id.clone().into()])).await?;
            assert!(move_to(&db,&id,plan.successor(&current.id)?).await.is_err(), "blocked work cannot advance normally");
            let restarted = sea_orm::Database::connect(options.clone()).await?;
            let entry = plan.step("rollback_entry",current.region_index,2)?;
            move_to(&restarted,&id,entry).await?;
            move_to(&db,&id,entry).await?; // same-cursor retry, not a new attempt
            let old = db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
                "SELECT status FROM regional_rollout_items WHERE operation_id=$1 AND step_id=$2",
                [id.clone().into(),current.id.clone().into()])).await?.unwrap();
            assert_eq!(old.try_get::<String>("","status")?,"interrupted");
            let attempts = db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
                "SELECT attempts FROM regional_rollout_items WHERE operation_id=$1 AND step_id=$2",
                [id.clone().into(),entry.id.clone().into()])).await?.unwrap();
            assert_eq!(attempts.try_get::<i32>("","attempts")?,1);
            assert!(move_to(&db,&id,plan.successor(&current.id)?).await.is_err(), "late forward worker cannot cross back");
            assert!(move_to(&db,&id,plan.step("activate_policy",current.region_index,2)?).await.is_err(), "rollback cannot skip preparation");
            let start = plan.rollback_steps.iter().position(|s| s.id == entry.id).unwrap();
            for step in plan.rollback_steps.iter().skip(start+1) { move_to(&restarted,&id,step).await?; }
            assert!(move_to(&db,&id,entry).await.is_err(), "terminal rollback cannot rewind");
            assert!(move_to(&db,&id,&plan.steps[0]).await.is_err(), "terminal rollback cannot restart forward");
            if current.region_index == 0 {
                let untouched = db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
                    "SELECT status FROM regional_rollout_items WHERE operation_id=$1 AND step_id='1:rollback_entry:2'",
                    [id.clone().into()])).await?.unwrap();
                assert_eq!(untouched.try_get::<String>("","status")?,"pending","rollback must not enter an untouched future region");
            }
        }
        let mut invalid = plan.clone();
        invalid.rollback_steps[0].id = invalid.steps[0].id.clone();
        assert!(insert(&db,"duplicate-identities",&invalid).await.is_err());
        let mut invalid = plan.clone(); invalid.steps[1].depends_on = None;
        assert!(insert(&db,"broken-dependency",&invalid).await.is_err());
        let mut invalid = plan.clone(); invalid.version = 4;
        assert!(insert(&db,"future-version",&invalid).await.is_err());
        // Installing the v3 trigger must leave admitted v1/v2 behavior intact.
        for (id,legacy) in [("legacy-one",Plan::compile(&["us3".into()],&[])),("legacy-two",Plan::policy_transition(None))] {
            insert(&db,id,&legacy).await?;
            move_to(&db,id,&legacy.steps[1]).await?;
        }
        root.execute_unprepared(&format!("DROP SCHEMA {schema} CASCADE")).await?;
        Ok(())
    }
}
