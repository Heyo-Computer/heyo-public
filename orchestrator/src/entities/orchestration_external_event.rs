use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "orchestration_external_events")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub workflow_run_id: Option<String>,
    pub step_run_id: Option<String>,
    pub external_ref: String,
    pub event_type: String,
    pub event_status: String,
    pub idempotency_key: String,
    pub message_id: Option<String>,
    #[sea_orm(column_type = "JsonBinary")]
    pub payload: Json,
    pub processing_status: String,
    pub processing_error: Option<String>,
    pub received_at: DateTimeWithTimeZone,
    pub processed_at: Option<DateTimeWithTimeZone>,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
