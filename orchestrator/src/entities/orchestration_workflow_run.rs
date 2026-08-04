use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "orchestration_workflow_runs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub thread_id: String,
    pub template_id: String,
    pub template_version: i32,
    pub goal: String,
    pub status: String,
    pub phase: String,
    pub target: String,
    #[sea_orm(column_type = "JsonBinary")]
    pub inputs: Json,
    #[sea_orm(column_type = "JsonBinary")]
    pub compiled_plan: Json,
    pub current_child_job_key: Option<String>,
    pub started_at: Option<DateTimeWithTimeZone>,
    pub completed_at: Option<DateTimeWithTimeZone>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
