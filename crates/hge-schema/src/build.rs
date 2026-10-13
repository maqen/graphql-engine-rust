//! Builds the GraphQL schema for one role.
//!
//! Root-field registration is a short list of calls in [`build_role_schema`]; workstreams add
//! their root fields (mutations, functions, actions, remote schemas) there.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use hge_core::{ColumnType, QualifiedTable, RoleName};
use indexmap::IndexMap;

use crate::introspection;
use crate::model::*;
use crate::permissions::{SelectAccess, select_access};
use crate::scalars::{self, BUILTIN_SCALARS};
use crate::tables::{RelationshipInfo, TableInfo, Tables};

/// Engine options that change the schema or the lowering.
#[derive(Debug, Clone, Default)]
pub struct SchemaOptions {
    /// `HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES`
    pub stringify_numeric_types: bool,
    /// `HASURA_GRAPHQL_V1_BOOLEAN_NULL_COLLAPSE`: treat `{_eq: null}` as `true`.
    pub dangerously_collapse_booleans: bool,
}

/// The schema of one role plus what lowering needs alongside it.
#[derive(Debug, Clone)]
pub struct RoleSchema {
    pub role: RoleName,
    pub schema: Schema,
    pub tables: Arc<Tables>,
    pub access: HashMap<(Arc<str>, QualifiedTable), SelectAccess>,
    pub mutation_access: HashMap<(Arc<str>, QualifiedTable), crate::mutation::MutationAccess>,
    pub options: SchemaOptions,
}

impl RoleSchema {
    pub fn select_access(&self, table: &TableInfo) -> Option<&SelectAccess> {
        self.access.get(&(table.source.clone(), table.name.clone()))
    }
}

pub const QUERY_ROOT: &str = "query_root";

pub fn build_role_schema(
    tables: &Arc<Tables>,
    role: &RoleName,
    options: &SchemaOptions,
) -> RoleSchema {
    let mut access = HashMap::new();
    for t in &tables.ordered {
        if let Some(a) = select_access(t, role) {
            access.insert((t.source.clone(), t.name.clone()), a);
        }
    }

    let mut b = Builder {
        schema: Schema {
            types: IndexMap::new(),
            query_type: QUERY_ROOT.to_owned(),
            mutation_type: None,
            subscription_type: None,
            directives: introspection::standard_directives(),
            introspection_enabled: true,
        },
        tables,
        access: &access,
        scalars: BTreeSet::new(),
    };
    for name in BUILTIN_SCALARS {
        b.scalars.insert((*name).to_owned());
    }
    introspection::add_introspection_types(&mut b.schema);

    let mut query_root = ObjectDef::new(QUERY_ROOT, None);

    // --- root field registration -------------------------------------------------------
    for table in &tables.ordered {
        b.add_table_select_root_fields(table, &mut query_root);
    }
    // ------------------------------------------------------------------------------------

    let mutation_access = crate::mutation::access::access_map(tables, role);
    let mut mutation_root = ObjectDef::new(
        crate::mutation::MUTATION_ROOT,
        Some("mutation root".to_owned()),
    );
    for table in &tables.ordered {
        b.add_table_mutation_root_fields(table, &mutation_access, &mut mutation_root);
    }
    if !mutation_root.fields.is_empty() {
        // V2 lists fields sorted by name.
        mutation_root.fields.sort_keys();
        b.schema.mutation_type = Some(crate::mutation::MUTATION_ROOT.to_owned());
        b.schema.add(TypeDef::Object(mutation_root));
    }

    if query_root.fields.is_empty() {
        query_root = query_root.field(FieldDef::new(
            "no_queries_available",
            TypeRef::nn("String"),
            FieldKind::NoQueriesAvailable,
        ));
    }
    b.schema.add(TypeDef::Object(query_root));
    b.finish();

    RoleSchema {
        role: role.clone(),
        schema: b.schema,
        tables: tables.clone(),
        access,
        mutation_access,
        options: options.clone(),
    }
}

pub(crate) struct Builder<'a> {
    pub(crate) schema: Schema,
    pub(crate) tables: &'a Tables,
    pub(crate) access: &'a HashMap<(Arc<str>, QualifiedTable), SelectAccess>,
    pub(crate) scalars: BTreeSet<String>,
}

impl Builder<'_> {
    pub(crate) fn access(&self, table: &TableInfo) -> Option<&SelectAccess> {
        self.access.get(&(table.source.clone(), table.name.clone()))
    }

    fn target(&self, rel: &RelationshipInfo) -> Option<Arc<TableInfo>> {
        let t = self.tables.get(&rel.source, &rel.target)?;
        self.access(t)?;
        Some(t.clone())
    }

    fn add_table_select_root_fields(&mut self, table: &Arc<TableInfo>, root: &mut ObjectDef) {
        let Some(access) = self.access(table).cloned() else {
            return;
        };
        if access.columns.is_empty() {
            return;
        }
        self.ensure_table_types(table);
        let display = table.display_name();

        let select = FieldDef::new(
            table.root_fields.select.clone(),
            TypeRef::nn_list_nn(table.type_name()),
            FieldKind::SelectMany(table.clone()),
        )
        .describe(
            table
                .root_fields
                .select_comment
                .clone()
                .unwrap_or_else(|| format!("fetch data from the table: \"{display}\"")),
        );
        let select = self.with_select_args(select, table);
        if access.query_root_fields.select {
            root.fields.insert(select.name.clone(), select);
        }

        if let Some(pk) = &table.primary_key
            && access.query_root_fields.select_by_pk
            && pk.iter().all(|c| access.allows_column(c))
        {
            let mut by_pk = FieldDef::new(
                table.root_fields.select_by_pk.clone(),
                TypeRef::named(table.type_name()),
                FieldKind::SelectByPk(table.clone()),
            )
            .describe(
                table
                    .root_fields
                    .select_by_pk_comment
                    .clone()
                    .unwrap_or_else(|| {
                        format!(
                            "fetch data from the table: \"{display}\" using primary key columns"
                        )
                    }),
            );
            for col in pk {
                let col = &table.columns[col];
                let ty = scalars::column_type_ref(&col.ty, false);
                self.note_scalar(&col.ty);
                by_pk = by_pk.arg(InputValueDef::new(col.gql_name.clone(), ty));
            }
            root.fields.insert(by_pk.name.clone(), by_pk);
        }
    }

    /// `distinct_on`, `limit`, `offset`, `order_by`, `where`.
    fn with_select_args(&mut self, field: FieldDef, table: &TableInfo) -> FieldDef {
        field
            .arg(
                InputValueDef::new(
                    "distinct_on",
                    TypeRef::nn(table.select_column_name()).list(),
                )
                .describe("distinct select on columns"),
            )
            .arg(
                InputValueDef::new("limit", TypeRef::named("Int"))
                    .describe("limit the number of rows returned"),
            )
            .arg(
                InputValueDef::new("offset", TypeRef::named("Int"))
                    .describe("skip the first n rows. Use only with order_by"),
            )
            .arg(
                InputValueDef::new("order_by", TypeRef::nn(table.order_by_name()).list())
                    .describe("sort the rows by one or more columns"),
            )
            .arg(
                InputValueDef::new("where", TypeRef::named(table.bool_exp_name()))
                    .describe("filter the rows returned"),
            )
    }

    pub(crate) fn note_scalar(&mut self, ty: &ColumnType) {
        self.scalars.insert(scalars::scalar_name(&ty.element()));
    }

    /// Object, bool_exp, order_by and select_column types of a table (once per table).
    pub(crate) fn ensure_table_types(&mut self, table: &Arc<TableInfo>) {
        if self.schema.types.contains_key(table.type_name()) {
            return;
        }
        let access = self.access(table).cloned().expect("caller checked access");
        let display = table.display_name();

        // Reserve the name first so recursive relationships terminate.
        self.schema.add(TypeDef::Object(ObjectDef::new(
            table.type_name(),
            table.description.clone(),
        )));

        let mut obj = ObjectDef::new(table.type_name(), table.description.clone());
        let mut bool_exp = InputObjectDef {
            name: table.bool_exp_name(),
            description: Some(format!(
                "Boolean expression to filter rows from the table \"{display}\". All fields are combined with a logical 'AND'."
            )),
            fields: IndexMap::new(),
        };
        let bool_exp_ty = || TypeRef::named(table.bool_exp_name());
        for f in [
            InputValueDef::new("_and", TypeRef::nn(table.bool_exp_name()).list()),
            InputValueDef::new("_not", bool_exp_ty()),
            InputValueDef::new("_or", TypeRef::nn(table.bool_exp_name()).list()),
        ] {
            bool_exp.fields.insert(f.name.clone(), f);
        }
        let mut order_by = InputObjectDef {
            name: table.order_by_name(),
            description: Some(format!(
                "Ordering options when selecting data from \"{display}\"."
            )),
            fields: IndexMap::new(),
        };
        let mut select_column = EnumDef {
            name: table.select_column_name(),
            description: Some(format!("select columns of table \"{display}\"")),
            values: IndexMap::new(),
        };

        for col in table.columns.values() {
            if !access.allows_column(&col.name) {
                continue;
            }
            self.note_scalar(&col.ty);
            let mut field = FieldDef::new(
                col.gql_name.clone(),
                scalars::column_type_ref(&col.ty, col.nullable),
                FieldKind::Column {
                    column: col.name.clone(),
                    ty: col.ty.clone(),
                },
            )
            .describe_opt(col.description.clone());
            if scalars::is_json_like(&col.ty) {
                field = field.arg(
                    InputValueDef::new("path", TypeRef::named("String"))
                        .describe("JSON select path"),
                );
            }
            obj.fields.insert(field.name.clone(), field);

            let cmp = self.ensure_comparison_exp(&col.ty);
            bool_exp.fields.insert(
                col.gql_name.clone(),
                InputValueDef::new(col.gql_name.clone(), TypeRef::named(cmp)),
            );
            order_by.fields.insert(
                col.gql_name.clone(),
                InputValueDef::new(col.gql_name.clone(), TypeRef::named("order_by")),
            );
            select_column.values.insert(
                col.gql_name.clone(),
                EnumValueDef {
                    name: col.gql_name.clone(),
                    description: Some("column name".to_owned()),
                    deprecation_reason: None,
                },
            );
        }
        self.ensure_order_by_enum();

        for rel in table.object_relationships.clone() {
            let Some(target) = self.target(&rel) else {
                continue;
            };
            self.ensure_table_types(&target);
            let ty = if rel.nullable {
                TypeRef::named(target.type_name())
            } else {
                TypeRef::nn(target.type_name())
            };
            obj.fields.insert(
                rel.name.clone(),
                FieldDef::new(
                    rel.name.clone(),
                    ty,
                    FieldKind::ObjectRelationship(rel.clone()),
                )
                .describe(
                    rel.description
                        .clone()
                        .unwrap_or_else(|| "An object relationship".to_owned()),
                ),
            );
            bool_exp.fields.insert(
                rel.name.clone(),
                InputValueDef::new(rel.name.clone(), TypeRef::named(target.bool_exp_name())),
            );
            order_by.fields.insert(
                rel.name.clone(),
                InputValueDef::new(rel.name.clone(), TypeRef::named(target.order_by_name())),
            );
        }
        for rel in table.array_relationships.clone() {
            let Some(target) = self.target(&rel) else {
                continue;
            };
            self.ensure_table_types(&target);
            let field = FieldDef::new(
                rel.name.clone(),
                TypeRef::nn_list_nn(target.type_name()),
                FieldKind::ArrayRelationship(rel.clone()),
            )
            .describe(
                rel.description
                    .clone()
                    .unwrap_or_else(|| "An array relationship".to_owned()),
            );
            let field = self.with_select_args(field, &target);
            obj.fields.insert(rel.name.clone(), field);
            bool_exp.fields.insert(
                rel.name.clone(),
                InputValueDef::new(rel.name.clone(), TypeRef::named(target.bool_exp_name())),
            );
        }

        self.schema
            .types
            .insert(obj.name.clone(), TypeDef::Object(obj));
        self.schema.add(TypeDef::InputObject(bool_exp));
        self.schema.add(TypeDef::InputObject(order_by));
        self.schema.add(TypeDef::Enum(select_column));
    }

    fn ensure_order_by_enum(&mut self) {
        if self.schema.types.contains_key("order_by") {
            return;
        }
        let values = [
            ("asc", "in ascending order, nulls last"),
            ("asc_nulls_first", "in ascending order, nulls first"),
            ("asc_nulls_last", "in ascending order, nulls last"),
            ("desc", "in descending order, nulls first"),
            ("desc_nulls_first", "in descending order, nulls first"),
            ("desc_nulls_last", "in descending order, nulls last"),
        ];
        self.schema.add(TypeDef::Enum(EnumDef {
            name: "order_by".to_owned(),
            description: Some("column ordering options".to_owned()),
            values: values
                .iter()
                .map(|(n, d)| {
                    (
                        (*n).to_owned(),
                        EnumValueDef {
                            name: (*n).to_owned(),
                            description: Some((*d).to_owned()),
                            deprecation_reason: None,
                        },
                    )
                })
                .collect(),
        }));
    }

    /// `<Scalar>_comparison_exp` for a column type; returns its name.
    fn ensure_comparison_exp(&mut self, ty: &ColumnType) -> String {
        let name = scalars::comparison_exp_name(ty);
        if self.schema.types.contains_key(&name) {
            return name;
        }
        let scalar = scalars::scalar_name(&ty.element());
        // Operator values are nullable in the schema (V2 rejects or collapses nulls later).
        let value_ty = scalars::column_type_ref(ty, true);
        let value_list_ty = scalars::column_type_ref(ty, false).list();
        let mut fields: Vec<InputValueDef> = vec![];

        if scalars::is_jsonb(ty) {
            let cast_name = "jsonb_cast_exp".to_owned();
            let text_cmp = self.ensure_comparison_exp(&ColumnType::builtin("text"));
            if !self.schema.types.contains_key(&cast_name) {
                let mut cast_fields = IndexMap::new();
                cast_fields.insert(
                    "String".to_owned(),
                    InputValueDef::new("String", TypeRef::named(text_cmp)),
                );
                self.schema.add(TypeDef::InputObject(InputObjectDef {
                    name: cast_name.clone(),
                    description: None,
                    fields: cast_fields,
                }));
            }
            fields.push(InputValueDef::new("_cast", TypeRef::named(cast_name)));
        }
        fields.push(InputValueDef::new("_eq", value_ty.clone()));
        fields.push(InputValueDef::new("_gt", value_ty.clone()));
        fields.push(InputValueDef::new("_gte", value_ty.clone()));
        fields.push(InputValueDef::new("_in", value_list_ty.clone()));
        fields.push(InputValueDef::new("_is_null", TypeRef::named("Boolean")));
        fields.push(InputValueDef::new("_lt", value_ty.clone()));
        fields.push(InputValueDef::new("_lte", value_ty.clone()));
        fields.push(InputValueDef::new("_neq", value_ty.clone()));
        fields.push(InputValueDef::new("_nin", value_list_ty));

        if scalars::is_string_type(ty) {
            let ops = [
                (
                    "_ilike",
                    "does the column match the given case-insensitive pattern",
                ),
                (
                    "_iregex",
                    "does the column match the given POSIX regular expression, case insensitive",
                ),
                ("_like", "does the column match the given pattern"),
                (
                    "_nilike",
                    "does the column NOT match the given case-insensitive pattern",
                ),
                (
                    "_niregex",
                    "does the column NOT match the given POSIX regular expression, case insensitive",
                ),
                ("_nlike", "does the column NOT match the given pattern"),
                (
                    "_nregex",
                    "does the column NOT match the given POSIX regular expression, case sensitive",
                ),
                (
                    "_nsimilar",
                    "does the column NOT match the given SQL regular expression",
                ),
                (
                    "_regex",
                    "does the column match the given POSIX regular expression, case sensitive",
                ),
                (
                    "_similar",
                    "does the column match the given SQL regular expression",
                ),
            ];
            for (op, desc) in ops {
                fields.push(InputValueDef::new(op, value_ty.clone()).describe(desc));
            }
        }
        if ty.is_array() {
            fields.push(
                InputValueDef::new("_contained_in", value_ty.clone())
                    .describe("is the array contained in the given array value"),
            );
            fields.push(
                InputValueDef::new("_contains", value_ty.clone())
                    .describe("does the array contain the given value"),
            );
        }
        if scalars::is_jsonb(ty) {
            fields.push(
                InputValueDef::new("_contained_in", value_ty.clone())
                    .describe("is the column contained in the given json value"),
            );
            fields.push(
                InputValueDef::new("_contains", value_ty.clone())
                    .describe("does the column contain the given json value at the top level"),
            );
            fields.push(
                InputValueDef::new("_has_key", TypeRef::named("String"))
                    .describe("does the string exist as a top-level key in the column"),
            );
            fields.push(
                InputValueDef::new("_has_keys_all", TypeRef::nn("String").list())
                    .describe("do all of these strings exist as top-level keys in the column"),
            );
            fields.push(
                InputValueDef::new("_has_keys_any", TypeRef::nn("String").list())
                    .describe("do any of these strings exist as top-level keys in the column"),
            );
        }
        fields.sort_by(|a, b| a.name.cmp(&b.name));

        self.schema.add(TypeDef::InputObject(InputObjectDef {
            name: name.clone(),
            description: Some(format!(
                "Boolean expression to compare columns of type \"{scalar}\". All fields are combined with logical 'AND'."
            )),
            fields: fields.into_iter().map(|f| (f.name.clone(), f)).collect(),
        }));
        name
    }

    fn finish(&mut self) {
        for name in std::mem::take(&mut self.scalars) {
            if self.schema.types.contains_key(&name) {
                continue;
            }
            // V2 gives built-in and custom scalars no description.
            self.schema.add(TypeDef::Scalar(ScalarDef {
                name,
                description: None,
            }));
        }
        // V2 lists types and arguments sorted by name.
        self.schema.types.sort_keys();
        for ty in self.schema.types.values_mut() {
            if let TypeDef::Object(obj) = ty {
                for field in obj.fields.values_mut() {
                    field.args.sort_keys();
                }
            }
        }
    }
}
