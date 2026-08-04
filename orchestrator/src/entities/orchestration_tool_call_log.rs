use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "orchestration_tool_call_logs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub thread_id: String,
    pub workflow_run_id: Option<String>,
    pub step_run_id: Option<String>,
    pub tool_name: String,
    #[sea_orm(column_type = "JsonBinary")]
    pub input: Json,
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub output: Option<Json>,
    pub status: String,
    pub started_at: DateTimeWithTimeZone,
    pub completed_at: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
