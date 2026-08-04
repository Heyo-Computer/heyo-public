use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "orchestration_step_attempts")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub workflow_run_id: String,
    pub step_run_id: String,
    pub attempt_number: i32,
    pub worker_id: String,
    pub status: String,
    pub lease_expires_at: DateTimeWithTimeZone,
    pub heartbeat_at: Option<DateTimeWithTimeZone>,
    pub failure_reason: Option<String>,
    pub started_at: DateTimeWithTimeZone,
    pub completed_at: Option<DateTimeWithTimeZone>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
