//! `SeaORM` Entity, @generated by sea-orm-codegen 1.1.0

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "distinct_value_fields")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub origin: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub origin_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub org_name: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub stream_name: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub stream_type: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub field_name: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}