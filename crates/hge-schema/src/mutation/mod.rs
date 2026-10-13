//! Mutation root fields: `insert_t`, `insert_t_one`, `update_t`, `update_t_by_pk`,
//! `update_t_many`, `delete_t`, `delete_t_by_pk`. Owned by the *Mutations* workstream.
//!
//! [`schema`] builds the `mutation_root` fields and their input types for a role, [`lower`]
//! turns a request into [`hge_ir::Mutation`]s, and [`access`] says what a role may mutate.

pub mod access;
pub(crate) mod lower;
pub(crate) mod schema;

use std::sync::Arc;

use crate::tables::TableInfo;

pub use access::{DeleteAccess, InsertAccess, MutationAccess, UpdateAccess};

pub const MUTATION_ROOT: &str = "mutation_root";

/// What a mutation-related schema field means, for lowering.
#[derive(Debug, Clone)]
pub enum MutationField {
    Insert(Arc<TableInfo>),
    InsertOne(Arc<TableInfo>),
    Update(Arc<TableInfo>),
    UpdateByPk(Arc<TableInfo>),
    UpdateMany(Arc<TableInfo>),
    Delete(Arc<TableInfo>),
    DeleteByPk(Arc<TableInfo>),
    /// `t_mutation_response.affected_rows`
    AffectedRows,
    /// `t_mutation_response.returning`
    Returning(Arc<TableInfo>),
}

/// Mutation root field names of a table (after `custom_root_fields`) and their descriptions.
#[derive(Debug, Clone)]
pub(crate) struct RootNames {
    pub insert: (String, String),
    pub insert_one: (String, String),
    pub update: (String, String),
    pub update_by_pk: (String, String),
    pub update_many: (String, String),
    pub delete: (String, String),
    pub delete_by_pk: (String, String),
}

impl RootNames {
    pub fn of(table: &TableInfo) -> Self {
        let t = &table.gql_name;
        let display = table.display_name();
        let crf = table
            .entry
            .configuration
            .custom_root_fields
            .clone()
            .unwrap_or_default();
        let pick = |f: &Option<hge_metadata::CustomRootField>, name: String, desc: String| {
            let f = f.as_ref();
            (
                f.and_then(|f| f.name()).map(str::to_owned).unwrap_or(name),
                f.and_then(|f| f.comment())
                    .map(str::to_owned)
                    .unwrap_or(desc),
            )
        };
        RootNames {
            insert: pick(
                &crf.insert,
                format!("insert_{t}"),
                format!("insert data into the table: \"{display}\""),
            ),
            insert_one: pick(
                &crf.insert_one,
                format!("insert_{t}_one"),
                format!("insert a single row into the table: \"{display}\""),
            ),
            update: pick(
                &crf.update,
                format!("update_{t}"),
                format!("update data of the table: \"{display}\""),
            ),
            update_by_pk: pick(
                &crf.update_by_pk,
                format!("update_{t}_by_pk"),
                format!("update single row of the table: \"{display}\""),
            ),
            update_many: pick(
                &crf.update_many,
                format!("update_{t}_many"),
                format!("update multiples rows of table: \"{display}\""),
            ),
            delete: pick(
                &crf.delete,
                format!("delete_{t}"),
                format!("delete data from the table: \"{display}\""),
            ),
            delete_by_pk: pick(
                &crf.delete_by_pk,
                format!("delete_{t}_by_pk"),
                format!("delete single row from the table: \"{display}\""),
            ),
        }
    }
}
