use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "orchestration_approvals")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub thread_id: String,
    pub workflow_run_id: String,
    pub step_run_id: String,
    pub kind: String,
    pub status: String,
    #[sea_orm(column_type = "JsonBinary")]
    pub request_artifact_ids: Json,
    pub decided_by: Option<String>,
    pub response_comment: Option<String>,
    pub decided_at: Option<DateTimeWithTimeZone>,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
