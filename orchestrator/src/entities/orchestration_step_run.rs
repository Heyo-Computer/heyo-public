use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "orchestration_step_runs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub workflow_run_id: String,
    pub key: String,
    pub r#type: String,
    pub kind: String,
    pub phase: String,
    pub status: String,
    pub adapter: Option<String>,
    #[sea_orm(column_type = "JsonBinary")]
    pub depends_on: Json,
    pub can_fan_out: bool,
    pub requires_approval_before_start: bool,
    #[sea_orm(column_type = "JsonBinary")]
    pub inputs: Json,
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub outputs: Option<Json>,
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub artifact_contract: Option<Json>,
    pub retry_count: i32,
    pub max_retries: i32,
    pub idempotency_key: Option<String>,
    pub external_ref: Option<String>,
    pub started_at: Option<DateTimeWithTimeZone>,
    pub completed_at: Option<DateTimeWithTimeZone>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
