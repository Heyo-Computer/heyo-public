//! Immutable per-deployment creation provenance. Resolved credentials never enter
//! this ledger; replay uses the exact versioned secret references instead.
use anyhow::{Context, Result};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use super::service_deploy::ServiceDeployRequest;
use crate::cloud_client::{self, CreateDeploymentRequest};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all="camelCase", deny_unknown_fields)]
pub(super) struct Recipe {
    pub request: ServiceDeployRequest,
    pub archive_sha256: String,
    pub guest_port: u16,
}

pub(super) async fn record(db: &impl ConnectionTrait, recipe: &Recipe, cloud: &CreateDeploymentRequest) -> Result<()> {
    let value=serde_json::to_value(recipe)?;
    let digest=cloud_client::deployment_request_digest(cloud)?;
    db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO service_creation_recipes(deployment_id,service_id,recipe,request_digest) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING",
        vec![cloud.deployment_id.clone().into(),recipe.request.service_id.clone().into(),value.clone().into(),digest.clone().into()])).await?;
    let row=db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT service_id,recipe,request_digest FROM service_creation_recipes WHERE deployment_id=$1",
        [cloud.deployment_id.clone().into()])).await?.context("creation recipe missing")?;
    anyhow::ensure!(row.try_get::<String>("","service_id")? == recipe.request.service_id
        && row.try_get::<Value>("","recipe")? == value && row.try_get::<String>("","request_digest")? == digest,
        "deployment creation recipe changed on replay");
    Ok(())
}

pub(super) async fn bind(db: &impl ConnectionTrait, deployment: &str, binding: Value) -> Result<()> {
    db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE service_creation_recipes SET binding=$2 WHERE deployment_id=$1",
        vec![deployment.into(),binding.into()])).await?;
    Ok(())
}

/// Missing provenance is a hard refusal. A caller cannot substitute a similarly
/// named deployment, active scalar metadata, or an unbound creation attempt.
pub(super) async fn load(db: &impl ConnectionTrait, service: &str, deployment: &str) -> Result<Recipe> {
    let row=db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT recipe,binding FROM service_creation_recipes WHERE service_id=$1 AND deployment_id=$2",
        [service.into(),deployment.into()])).await?.context("baseline has no immutable creation recipe")?;
    let mut recipe: Recipe=serde_json::from_value(row.try_get("","recipe")?)?;
    let binding: Value=row.try_get::<Option<Value>>("","binding")?.context("baseline creation recipe is not bound")?;
    let archive=binding["archiveId"].as_str().filter(|s|!s.is_empty()).context("bound archive missing")?;
    recipe.request.archive_id=Some(archive.into());
    recipe.request.archive_bytes_base64=None;
    recipe.request.deployment_id=None;
    recipe.request.retire_previous=false;
    recipe.request.retire_previous_async=false;
    recipe.request.delete_previous=false;
    recipe.request.route=None;
    Ok(recipe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    #[ignore = "requires disposable ORCHESTRATOR_TEST_DATABASE_URL"]
    async fn immutable_recipe_binding_and_fresh_identity_postgres() -> Result<()> {
        let url=std::env::var("ORCHESTRATOR_TEST_DATABASE_URL")?;
        let root=sea_orm::Database::connect(url.clone()).await?;
        let schema=format!("recipe_{}",uuid::Uuid::new_v4().simple());
        root.execute_unprepared(&format!("CREATE SCHEMA {schema}")).await?;
        let mut options=sea_orm::ConnectOptions::new(url);
        options.set_schema_search_path(schema.clone());
        let db=sea_orm::Database::connect(options.clone()).await?;
        db.execute_unprepared(include_str!("../../migrations/045_service_creation_recipes.sql")).await?;
        let recipe=Recipe {request:serde_json::from_value(json!({"serviceId":"ci","userId":"operator",
            "deploymentId":"old-us","region":"us3","driver":"firecracker","image":"pinned-image",
            "archiveId":"old-archive","envRefs":["DATABASE_URL=heyosecret://ci/database@7"],
            "env":{"MODE":"production"},"startCommand":"/ci serve","workingDirectory":"/app",
            "mounts":[{"hostPath":"/retained/data","sandboxPath":"/data","readOnly":true}],
            "retirePrevious":true,"deletePrevious":true}))?,archive_sha256:"a".repeat(64),guest_port:8080};
        db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
            "INSERT INTO service_creation_recipes(deployment_id,service_id,recipe,request_digest) VALUES('old-us','ci',$1,'digest')",
            [serde_json::to_value(&recipe)?.into()])).await?;
        assert!(load(&db,"ci","old-us").await.is_err(),"unbound attempt is not provenance");
        let binding=json!({"archiveId":"old-archive","backendServerId":"host-us","backendSandboxId":"runtime-old"});
        bind(&db,"old-us",binding.clone()).await?;
        bind(&db,"old-us",binding).await?;
        assert!(bind(&db,"old-us",json!({"archiveId":"substitute"})).await.is_err());
        assert!(db.execute_unprepared("UPDATE service_creation_recipes SET recipe='{}' WHERE deployment_id='old-us'").await.is_err());
        assert!(db.execute_unprepared("DELETE FROM service_creation_recipes WHERE deployment_id='old-us'").await.is_err());
        let restarted=sea_orm::Database::connect(options).await?;
        assert!(load(&restarted,"other-app","old-us").await.is_err());
        assert!(load(&restarted,"ci","invented").await.is_err());
        let fresh=load(&restarted,"ci","old-us").await?;
        assert!(fresh.request.deployment_id.is_none());
        assert!(!fresh.request.retire_previous && !fresh.request.delete_previous);
        assert_eq!(fresh.request.archive_id.as_deref(),Some("old-archive"));
        assert_eq!(fresh.request.env_refs,vec!["DATABASE_URL=heyosecret://ci/database@7"]);
        assert_eq!(fresh.request.start_command.as_deref(),Some("/ci serve"));
        assert_eq!(fresh.request.mounts,recipe.request.mounts);
        assert_eq!(fresh.request.env,recipe.request.env);
        assert_eq!(fresh.archive_sha256,"a".repeat(64));
        root.execute_unprepared(&format!("DROP SCHEMA {schema} CASCADE")).await?;
        Ok(())
    }
}
