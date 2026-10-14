//! Builds the GraphQL schema for one role.
//!
//! Root-field registration is a short list of calls in [`build_role_schema`]; workstreams add
//! their root fields (mutations, functions, actions, remote schemas) there.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use hge_core::{ColumnType, QualifiedTable, RoleName};
use indexmap::IndexMap;

use crate::actions::ResolvedActions;
use crate::functions::{ComputedFieldInfo, ComputedReturn, ExposedAs, FunctionInfo};
use crate::introspection;
use crate::model::*;
use crate::native::NativeQueryInfo;
use crate::permissions::{SelectAccess, select_access};
use crate::scalars::{self, BUILTIN_SCALARS};
use crate::tables::{RelKind, RelationshipInfo, TableInfo, Tables};

/// Engine options that change the schema or the lowering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaOptions {
    /// `HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES`
    pub stringify_numeric_types: bool,
    /// `HASURA_GRAPHQL_V1_BOOLEAN_NULL_COLLAPSE`: treat `{_eq: null}` as `true`.
    pub dangerously_collapse_booleans: bool,
    /// Not `HASURA_GRAPHQL_INFER_FUNCTION_PERMISSIONS=false`: query functions need no
    /// function permission.
    pub infer_function_permissions: bool,
}

impl Default for SchemaOptions {
    fn default() -> Self {
        SchemaOptions {
            stringify_numeric_types: false,
            dangerously_collapse_booleans: false,
            infer_function_permissions: true,
        }
    }
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

pub use crate::mutation::MUTATION_ROOT;

/// The schema of `role`. `backend_only` builds it for requests that use backend-only
/// permissions (`x-hasura-use-backend-only-permissions: true`, honoured with the admin secret
/// or when no auth is configured), which also see mutations whose permission is
/// `backend_only`.
pub fn build_role_schema(
    tables: &Arc<Tables>,
    actions: &ResolvedActions,
    remotes: &crate::remote::RemoteSchemas,
    role: &RoleName,
    backend_only: bool,
    options: &SchemaOptions,
) -> RoleSchema {
    let access = role_access(tables, role);
    let mutation_access = crate::mutation::access::access_map(tables, role, backend_only);
    let schema = build_schema(
        tables,
        &access,
        &mutation_access,
        Some(actions),
        remotes,
        role,
        options,
    );
    RoleSchema {
        role: role.clone(),
        schema,
        tables: tables.clone(),
        access,
        mutation_access,
        options: options.clone(),
    }
}

fn role_access(
    tables: &Tables,
    role: &RoleName,
) -> HashMap<(Arc<str>, QualifiedTable), SelectAccess> {
    let mut access = HashMap::new();
    for t in tables.ordered.iter().chain(&tables.logical_models) {
        if let Some(a) = select_access(t, role) {
            access.insert((t.source.clone(), t.name.clone()), a);
        }
    }
    access
}

/// The root fields and types the tracked tables contribute to the admin schema (a superset
/// of every role's), so that actions and custom types can be checked for name conflicts.
pub(crate) struct TableSchemaNames {
    pub query_fields: HashSet<String>,
    pub mutation_fields: HashSet<String>,
    pub subscription_fields: HashSet<String>,
    /// Type name → whether it is a scalar.
    pub types: HashMap<String, bool>,
}

pub(crate) fn table_schema_names(tables: &Tables) -> TableSchemaNames {
    let role = RoleName::admin();
    let access = role_access(tables, &role);
    let mutation_access = crate::mutation::access::access_map(tables, &role, true);
    let schema = build_schema(
        tables,
        &access,
        &mutation_access,
        None,
        &crate::remote::RemoteSchemas::default(),
        &role,
        &SchemaOptions::default(),
    );
    let fields = |root: &str| -> HashSet<String> {
        match schema.types.get(root) {
            Some(TypeDef::Object(o)) => o
                .fields
                .values()
                .filter(|f| !matches!(f.kind, FieldKind::NoQueriesAvailable))
                .map(|f| f.name.clone())
                .collect(),
            _ => HashSet::new(),
        }
    };
    TableSchemaNames {
        query_fields: fields(QUERY_ROOT),
        mutation_fields: fields(MUTATION_ROOT),
        subscription_fields: fields(crate::subscription::SUBSCRIPTION_ROOT),
        types: schema
            .types
            .iter()
            .filter(|(name, _)| {
                !matches!(
                    name.as_str(),
                    QUERY_ROOT | MUTATION_ROOT | crate::subscription::SUBSCRIPTION_ROOT
                )
            })
            .map(|(name, ty)| (name.clone(), matches!(ty, TypeDef::Scalar(_))))
            // Built-in scalars are reserved even when no table uses them (and so pruned).
            .chain(BUILTIN_SCALARS.iter().map(|s| ((*s).to_owned(), true)))
            .collect(),
    }
}

fn build_schema(
    tables: &Tables,
    access: &HashMap<(Arc<str>, QualifiedTable), SelectAccess>,
    mutation_access: &HashMap<(Arc<str>, QualifiedTable), crate::mutation::MutationAccess>,
    actions: Option<&ResolvedActions>,
    remotes: &crate::remote::RemoteSchemas,
    role: &RoleName,
    options: &SchemaOptions,
) -> Schema {
    let subscription_access = crate::subscription::subscription_access(access);
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
        access,
        remotes,
        scalars: BTreeSet::new(),
        unused_model_objects: HashSet::new(),
        native_query_types: HashSet::new(),
    };
    for name in BUILTIN_SCALARS {
        b.scalars.insert((*name).to_owned());
    }
    introspection::add_introspection_types(&mut b.schema);

    let mut query_root = ObjectDef::new(QUERY_ROOT, None);
    let mut mutation_root = ObjectDef::new(MUTATION_ROOT, Some("mutation root".to_owned()));

    // --- root field registration -------------------------------------------------------
    for table in &tables.ordered {
        b.add_table_select_root_fields(table, &mut query_root);
        b.add_table_aggregate_root_field(table, &mut query_root);
    }
    for table in &tables.ordered {
        b.add_table_mutation_root_fields(table, mutation_access, &mut mutation_root);
    }
    for function in &tables.functions {
        let root = match function.exposed_as {
            ExposedAs::Query => &mut query_root,
            ExposedAs::Mutation => &mut mutation_root,
        };
        b.add_function_root_field(function, role, options.infer_function_permissions, root);
    }
    for nq in &tables.native_queries {
        b.add_native_query_root_field(nq, &mut query_root);
    }
    // Actions come last; `resolve_actions` has dropped those whose names conflict with
    // table root fields or types.
    if let Some(actions) = actions {
        crate::actions::add_root_fields(&mut b, actions, role, &mut query_root, &mut mutation_root);
    }
    // Remote schemas come after actions (their conflicts with the above were checked at
    // load).
    crate::remote::add_remote_schemas(
        &mut b.schema,
        &b.scalars,
        remotes,
        &mut query_root,
        &mut mutation_root,
    );
    // ------------------------------------------------------------------------------------

    if !mutation_root.fields.is_empty() {
        // V2 lists fields sorted by name.
        mutation_root.fields.sort_keys();
        b.schema.mutation_type = Some(MUTATION_ROOT.to_owned());
        b.schema.add(TypeDef::Object(mutation_root));
    }

    // The subscription root repeats the query root's table fields, as allowed by
    // `subscription_root_fields`, and adds the streaming fields.
    let mut subscription_root = ObjectDef::new(crate::subscription::SUBSCRIPTION_ROOT, None);
    b.access = &subscription_access;
    for table in &tables.ordered {
        b.add_table_select_root_fields(table, &mut subscription_root);
        b.add_table_aggregate_root_field(table, &mut subscription_root);
        b.add_table_stream_root_field(table, &mut subscription_root);
    }
    for function in &tables.functions {
        if function.exposed_as == ExposedAs::Query {
            let infer = options.infer_function_permissions;
            b.add_function_root_field(function, role, infer, &mut subscription_root);
        }
    }
    for nq in &tables.native_queries {
        b.add_native_query_root_field(nq, &mut subscription_root);
    }
    b.access = access;
    // Async actions' `<action>(id:)` result fields can be subscribed to as well.
    for field in query_root.fields.values() {
        if matches!(field.kind, FieldKind::AsyncActionQuery(_)) {
            subscription_root
                .fields
                .insert(field.name.clone(), field.clone());
        }
    }
    if !subscription_root.fields.is_empty() {
        subscription_root.fields.sort_keys();
        b.schema.subscription_type = Some(crate::subscription::SUBSCRIPTION_ROOT.to_owned());
        b.schema.add(TypeDef::Object(subscription_root));
    }

    if query_root.fields.is_empty() {
        query_root = query_root.field(FieldDef::new(
            "no_queries_available",
            TypeRef::nn("String"),
            FieldKind::NoQueriesAvailable,
        ));
    }
    query_root.fields.sort_keys();
    b.schema.add(TypeDef::Object(query_root));
    b.finish();
    b.schema
}

pub(crate) struct Builder<'a> {
    pub(crate) schema: Schema,
    pub(crate) tables: &'a Tables,
    pub(crate) access: &'a HashMap<(Arc<str>, QualifiedTable), SelectAccess>,
    /// The remote schemas in this schema, with the tables' relationships to them.
    pub(crate) remotes: &'a crate::remote::RemoteSchemas,
    pub(crate) scalars: BTreeSet<String>,
    /// Logical model object types built only for their bool_exp, order_by and enum types
    /// (native queries with relationships have their own object type): dropped at the end
    /// unless something else uses them.
    pub(crate) unused_model_objects: HashSet<String>,
    /// Object types of native queries with relationships, built or being built.
    native_query_types: HashSet<String>,
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
        // Like V2, even a permission without columns gives the root fields (with an empty
        // object type when nothing else is selectable).
        let Some(access) = self.access(table).cloned() else {
            return;
        };
        self.ensure_table_types(table);
        let display = table.display_name();

        let select = FieldDef::new(
            table.root_fields.select.clone(),
            TypeRef::nn_list_nn(table.type_name()),
            FieldKind::SelectMany(table.clone()),
        )
        .describe_opt(crate::tables::comment_or(
            table.root_fields.select_comment.as_deref(),
            || Some(format!("fetch data from the table: \"{display}\"")),
        ));
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
            .describe_opt(crate::tables::comment_or(
                table.root_fields.select_by_pk_comment.as_deref(),
                || {
                    Some(format!(
                        "fetch data from the table: \"{display}\" using primary key columns"
                    ))
                },
            ));
            for col in pk {
                let col = &table.columns[col];
                let ty = self.column_value_type(col, false);
                by_pk = by_pk.arg(InputValueDef::new(col.gql_name.clone(), ty).for_column());
            }
            root.fields.insert(by_pk.name.clone(), by_pk);
        }
    }

    /// `distinct_on`, `limit`, `offset`, `order_by`, `where`.
    pub(crate) fn with_select_args(&mut self, field: FieldDef, table: &TableInfo) -> FieldDef {
        // Without selectable columns there is no `<table>_select_column` enum (an enum needs
        // values), so no `distinct_on` either, as in V2.
        let field = if self.has_selectable_columns(table) {
            field.arg(
                InputValueDef::new(
                    "distinct_on",
                    TypeRef::nn(table.select_column_name()).list(),
                )
                .describe("distinct select on columns"),
            )
        } else {
            field
        };
        field
            .arg(
                InputValueDef::new("limit", TypeRef::named("Int"))
                    .describe("limit the number of rows returned")
                    .with_parser(ValueParser::NonNegativeInt),
            )
            .arg(
                InputValueDef::new("offset", TypeRef::named("Int"))
                    .describe("skip the first n rows. Use only with order_by")
                    .with_parser(ValueParser::BigInt),
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

    fn has_selectable_columns(&self, table: &TableInfo) -> bool {
        self.access(table)
            .is_some_and(|a| table.columns.keys().any(|c| a.allows_column(c)))
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
        let what = if table.logical_model {
            "the logical model for"
        } else {
            "the table"
        };
        let mut bool_exp = InputObjectDef {
            name: table.bool_exp_name(),
            description: Some(format!(
                "Boolean expression to filter rows from {what} \"{display}\". All fields are combined with a logical 'AND'."
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
            description: (!table.logical_model)
                .then(|| format!("select columns of table \"{display}\"")),
            values: IndexMap::new(),
        };

        for col in table.columns.values() {
            if !access.allows_column(&col.name) {
                continue;
            }
            let mut field = FieldDef::new(
                col.gql_name.clone(),
                // Inherited-role redaction can null any column.
                self.column_value_type(col, col.nullable || access.redaction(&col.name).is_some()),
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

            let cmp = self.column_comparison_exp(col);
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

        for rel in table
            .object_relationships
            .iter()
            .chain(&table.array_relationships)
        {
            if rel.native_query.is_none() {
                continue;
            }
            // V2 offers no ordering (nor aggregates) by a relationship onto a native query.
            if let Some((field, bool_exp_ty)) = self.native_query_relationship_field(rel) {
                obj.fields.insert(rel.name.clone(), field);
                bool_exp.fields.insert(
                    rel.name.clone(),
                    InputValueDef::new(rel.name.clone(), TypeRef::named(bool_exp_ty)),
                );
            }
        }
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
            if let Some(f) = self.relationship_aggregate_field(&rel, &target) {
                obj.fields.insert(f.name.clone(), f);
            }
            if let Some(f) = self.relationship_aggregation_predicate(&rel, &target) {
                bool_exp.fields.insert(f.name.clone(), f);
            }
            let f = self.relationship_aggregate_order_by(&rel, &target);
            order_by.fields.insert(f.name.clone(), f);
        }

        for cf in table.computed_fields.clone() {
            let mut field = match &cf.returns {
                ComputedReturn::Scalar(ty) => {
                    if !access.computed_fields.contains(&cf.name) {
                        continue;
                    }
                    self.note_scalar(ty);
                    let mut field = FieldDef::new(
                        cf.name.clone(),
                        scalars::column_type_ref(ty, true),
                        FieldKind::ComputedField(cf.clone()),
                    );
                    if scalars::is_json_like(ty) {
                        field = field.arg(
                            InputValueDef::new("path", TypeRef::named("String"))
                                .describe("JSON select path"),
                        );
                    }
                    if !cf.has_input_args() {
                        let cmp = self.ensure_comparison_exp(ty);
                        bool_exp.fields.insert(
                            cf.name.clone(),
                            InputValueDef::new(cf.name.clone(), TypeRef::named(cmp)),
                        );
                        order_by.fields.insert(
                            cf.name.clone(),
                            InputValueDef::new(cf.name.clone(), TypeRef::named("order_by")),
                        );
                    }
                    field
                }
                ComputedReturn::Table(target) => {
                    let Some(target) = self.tables.get(&table.source, target).cloned() else {
                        continue;
                    };
                    if self.access(&target).is_none() {
                        continue;
                    }
                    self.ensure_table_types(&target);
                    if !cf.has_input_args() {
                        bool_exp.fields.insert(
                            cf.name.clone(),
                            InputValueDef::new(
                                cf.name.clone(),
                                TypeRef::named(target.bool_exp_name()),
                            ),
                        );
                        // Like an array relationship's, without needing allow_aggregations.
                        let name = format!("{}_aggregate", cf.name);
                        let ty = self.ensure_aggregate_order_by(&target);
                        order_by
                            .fields
                            .insert(name.clone(), InputValueDef::new(name, TypeRef::named(ty)));
                    }
                    let field = FieldDef::new(
                        cf.name.clone(),
                        TypeRef::nn(target.type_name()).list(),
                        FieldKind::ComputedField(cf.clone()),
                    );
                    self.with_select_args(field, &target)
                }
            };
            field.description = cf.description.clone();
            let field = self.with_computed_field_args(field, table, &cf);
            obj.fields.insert(cf.name.clone(), field);
        }

        // Remote relationships need no column permissions (spec 08 §8.6).
        let remotes = self.remotes;
        let key = (table.source.clone(), table.name.clone());
        for rel in remotes.relationships.get(&key).into_iter().flatten() {
            let mut field = FieldDef::new(
                rel.name.clone(),
                rel.ty.clone(),
                FieldKind::RemoteRelationship(rel.clone()),
            );
            field.description = rel.description.clone();
            field.args = rel.args.clone();
            obj.fields.insert(rel.name.clone(), field);
            for ty in &rel.input_types {
                self.schema.add(TypeDef::InputObject(ty.clone()));
            }
        }

        self.schema
            .types
            .insert(obj.name.clone(), TypeDef::Object(obj));
        self.schema.add(TypeDef::InputObject(bool_exp));
        self.schema.add(TypeDef::InputObject(order_by));
        if !select_column.values.is_empty() {
            self.schema.add(TypeDef::Enum(select_column));
        }
    }

    /// A tracked function's root field, if `role` may call it.
    /// A tracked function's root field, and for query functions `<function>_aggregate` when
    /// the role may aggregate the returned table.
    fn add_function_root_field(
        &mut self,
        f: &Arc<FunctionInfo>,
        role: &RoleName,
        infer: bool,
        root: &mut ObjectDef,
    ) {
        let Some(table) = self.tables.get(&f.source, &f.returns).cloned() else {
            return;
        };
        if self.access(&table).is_none() || !f.permits(role, infer) {
            return;
        }
        self.ensure_table_types(&table);
        let ty = if f.returns_set {
            TypeRef::nn_list_nn(table.type_name())
        } else {
            TypeRef::named(table.type_name())
        };
        let field = FieldDef::new(f.root_field.clone(), ty, FieldKind::Function(f.clone()))
            .describe(f.description.clone());
        let field = self.with_select_args(field, &table);
        let field = self.with_function_args(field, f);
        root.fields.insert(field.name.clone(), field);

        if f.exposed_as == ExposedAs::Query && self.may_aggregate(&table) {
            let ty = self.ensure_aggregate_types(&table);
            let field = FieldDef::new(
                f.aggregate_root_field.clone(),
                TypeRef::nn(ty),
                FieldKind::FunctionAggregate(f.clone()),
            )
            .describe(format!(
                "execute function \"{}\" and query aggregates on result of table type \"{}\"",
                f.function.v2_text(),
                table.display_name()
            ));
            let field = self.with_select_args(field, &table);
            let field = self.with_function_args(field, f);
            root.fields.insert(field.name.clone(), field);
        }
    }

    /// A native query's root field: the rows of its logical model, for roles that may select
    /// from the logical model.
    fn add_native_query_root_field(&mut self, nq: &Arc<NativeQueryInfo>, root: &mut ObjectDef) {
        let Some(model) = self.tables.logical_model(&nq.source, &nq.returns).cloned() else {
            return;
        };
        if self.access(&model).is_none() {
            return;
        }
        let ty = self.ensure_native_query_type(nq, &model);
        let field = FieldDef::new(
            nq.root_field.clone(),
            TypeRef::nn_list_nn(ty),
            FieldKind::NativeQuery(nq.clone()),
        )
        .describe_opt(nq.description.clone());
        let field = self.with_native_query_args(field, nq, &nq.root_field);
        let field = self.with_select_args(field, &model);
        root.fields.insert(field.name.clone(), field);
    }

    /// The `args` argument of a field selecting from a native query with arguments, of type
    /// `<field>_arguments`.
    fn with_native_query_args(
        &mut self,
        field: FieldDef,
        nq: &NativeQueryInfo,
        field_name: &str,
    ) -> FieldDef {
        if nq.arguments.is_empty() {
            return field;
        }
        let name = crate::native::arguments_type_name(field_name);
        let description = crate::native::arguments_description(field_name);
        if !self.schema.types.contains_key(&name) {
            let mut fields = IndexMap::new();
            for (arg_name, arg) in &nq.arguments {
                self.note_scalar(&arg.ty);
                let description = arg
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("Native Query argument {arg_name}"));
                fields.insert(
                    arg_name.clone(),
                    InputValueDef::new(
                        arg_name.clone(),
                        scalars::column_type_ref(&arg.ty, arg.nullable),
                    )
                    .describe(description)
                    .for_column(),
                );
            }
            self.schema.add(TypeDef::InputObject(InputObjectDef {
                name: name.clone(),
                description: Some(description.clone()),
                fields,
            }));
        }
        field.arg(InputValueDef::new("args", TypeRef::nn(name)).describe(description))
    }

    /// `args` of a computed field that takes arguments besides the row and the session.
    pub(crate) fn with_computed_field_args(
        &mut self,
        field: FieldDef,
        table: &TableInfo,
        cf: &ComputedFieldInfo,
    ) -> FieldDef {
        if !cf.has_input_args() {
            return field;
        }
        let name = self.ensure_computed_field_args(table, cf);
        let ty = if cf.args_required() {
            TypeRef::nn(name)
        } else {
            TypeRef::named(name)
        };
        field.arg(InputValueDef::new("args", ty).describe(format!(
            "input parameters for computed field \"{}\" defined on table \"{}\"",
            cf.name,
            table.display_name()
        )))
    }

    /// The object type of a native query's rows (see [`NativeQueryInfo::object_type_name`]),
    /// with the logical model's types.
    fn ensure_native_query_type(
        &mut self,
        nq: &Arc<NativeQueryInfo>,
        model: &Arc<TableInfo>,
    ) -> String {
        let model_type_existed = self.schema.types.contains_key(model.type_name());
        self.ensure_table_types(model);
        let name = nq.object_type_name(model).to_owned();
        if !nq.has_relationships() {
            self.unused_model_objects.remove(model.type_name());
            return name;
        }
        // An inline model's object type has the native query's name, so it is replaced here.
        let shares_model_type = name == model.type_name();
        if !model_type_existed && !shares_model_type {
            self.unused_model_objects
                .insert(model.type_name().to_owned());
        }
        if !self.native_query_types.insert(name.clone())
            || (!shares_model_type && self.schema.types.contains_key(&name))
        {
            return name;
        }
        let fields = match self.schema.types.get(model.type_name()) {
            Some(TypeDef::Object(obj)) => obj.fields.clone(),
            _ => IndexMap::new(),
        };
        // Reserve the name first so recursive relationships terminate.
        self.schema.add(TypeDef::Object(ObjectDef::new(
            &name,
            model.description.clone(),
        )));
        let mut obj = ObjectDef::new(&name, model.description.clone());
        obj.fields = fields;
        let rels = nq.object_relationships.iter();
        for rel in rels.chain(nq.array_relationships.iter()) {
            let field = if rel.native_query.is_some() {
                self.native_query_relationship_field(rel).map(|(f, _)| f)
            } else {
                self.table_relationship_field(rel)
            };
            if let Some(field) = field {
                obj.fields.insert(field.name.clone(), field);
            }
        }
        self.schema.types.insert(name.clone(), TypeDef::Object(obj));
        name
    }

    /// A native query's relationship onto a table: unlike a table's own relationships, with
    /// a nullable object and without aggregates.
    fn table_relationship_field(&mut self, rel: &Arc<RelationshipInfo>) -> Option<FieldDef> {
        let target = self.target(rel)?;
        self.ensure_table_types(&target);
        Some(match rel.kind {
            RelKind::Object => FieldDef::new(
                rel.name.clone(),
                TypeRef::named(target.type_name()),
                FieldKind::ObjectRelationship(rel.clone()),
            )
            .describe("An object relationship"),
            RelKind::Array => {
                let field = FieldDef::new(
                    rel.name.clone(),
                    TypeRef::nn_list_nn(target.type_name()),
                    FieldKind::ArrayRelationship(rel.clone()),
                )
                .describe("An array relationship");
                self.with_select_args(field, &target)
            }
        })
    }

    /// A relationship onto a native query, if the role may select from its logical model,
    /// and the model's bool_exp type (for filtering on the relationship).
    fn native_query_relationship_field(
        &mut self,
        rel: &Arc<RelationshipInfo>,
    ) -> Option<(FieldDef, String)> {
        let target = rel.native_query.as_deref()?;
        let nq = self
            .tables
            .native_queries
            .iter()
            .find(|q| q.source == rel.source && q.root_field == target)?
            .clone();
        let model = self.tables.logical_model(&nq.source, &nq.returns)?.clone();
        self.access(&model)?;
        let ty = self.ensure_native_query_type(&nq, &model);
        let (ty, default_description) = match rel.kind {
            RelKind::Object => (TypeRef::named(ty), "An object relationship"),
            RelKind::Array => (TypeRef::nn(ty).list(), "An array relationship"),
        };
        let kind = FieldKind::NativeQueryRelationship {
            rel: rel.clone(),
            native_query: nq.clone(),
        };
        let field = FieldDef::new(rel.name.clone(), ty, kind).describe(
            rel.description
                .clone()
                .unwrap_or_else(|| default_description.to_owned()),
        );
        let mut field = self.with_native_query_args(field, &nq, &rel.name);
        if rel.kind == RelKind::Array {
            field = self.with_select_args(field, &model);
        }
        Some((field, model.bool_exp_name()))
    }

    /// The `args` argument of a function root field (when the function has input arguments).
    fn with_function_args(&mut self, field: FieldDef, f: &FunctionInfo) -> FieldDef {
        if !f.has_input_args() {
            return field;
        }
        let name = f.args_type_name();
        if !self.schema.types.contains_key(&name) {
            let mut fields = IndexMap::new();
            for (_, gql_name, arg) in f.input_args() {
                self.note_scalar(&arg.ty);
                fields.insert(
                    gql_name.clone(),
                    InputValueDef::new(gql_name, scalars::column_type_ref(&arg.ty, true))
                        .for_column(),
                );
            }
            self.schema.add(TypeDef::InputObject(InputObjectDef {
                name: name.clone(),
                description: None,
                fields,
            }));
        }
        let ty = if f.args_required() {
            TypeRef::nn(name)
        } else {
            TypeRef::named(name)
        };
        let description = format!("input parameters for function \"{}\"", field.name);
        field.arg(InputValueDef::new("args", ty).describe(description))
    }

    /// `<field>_<table>_args`: the input arguments of a computed field.
    fn ensure_computed_field_args(&mut self, table: &TableInfo, cf: &ComputedFieldInfo) -> String {
        let name = cf.args_type_name(table.type_name());
        if self.schema.types.contains_key(&name) {
            return name;
        }
        let mut fields = IndexMap::new();
        for (_, gql_name, arg) in cf.input_args() {
            self.note_scalar(&arg.ty);
            fields.insert(
                gql_name.clone(),
                InputValueDef::new(gql_name, scalars::column_type_ref(&arg.ty, true)).for_column(),
            );
        }
        self.schema.add(TypeDef::InputObject(InputObjectDef {
            name: name.clone(),
            description: None,
            fields,
        }));
        name
    }

    pub(crate) fn ensure_order_by_enum(&mut self) {
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
    pub(crate) fn ensure_comparison_exp(&mut self, ty: &ColumnType) -> String {
        let name = scalars::comparison_exp_name(ty);
        if self.schema.types.contains_key(&name) {
            return name;
        }
        // Also reached for cast targets, which may be no column's type.
        self.note_scalar(ty);
        let scalar = scalars::scalar_name(&ty.element());
        // Operator values are nullable in the schema (V2 rejects or collapses nulls later).
        let value_ty = scalars::column_type_ref(ty, true);
        let value_list_ty = scalars::column_type_ref(ty, false).list();
        let mut fields: Vec<InputValueDef> = vec![];

        // Cast targets' comparison types are built after this one: geometry and geography
        // refer to each other.
        let casts = crate::operators::cast_targets(ty);
        if !casts.is_empty() {
            let cast_name = format!("{scalar}_cast_exp");
            if !self.schema.types.contains_key(&cast_name) {
                let mut cast_fields = IndexMap::new();
                for (field, target) in &casts {
                    let cmp = scalars::comparison_exp_name(target);
                    cast_fields.insert(
                        (*field).to_owned(),
                        InputValueDef::new(*field, TypeRef::named(cmp)),
                    );
                }
                self.schema.add(TypeDef::InputObject(InputObjectDef {
                    name: cast_name.clone(),
                    description: None,
                    fields: cast_fields,
                }));
            }
            fields.push(InputValueDef::new("_cast", TypeRef::named(cast_name)));
        }
        fields.extend(crate::operators::spatial_fields(&mut self.schema, ty));
        fields.extend(crate::operators::ltree_fields(ty));
        for t in crate::operators::ltree_scalars(ty) {
            self.note_scalar(&t);
        }
        fields.push(InputValueDef::new("_eq", value_ty.clone()).for_column());
        fields.push(InputValueDef::new("_gt", value_ty.clone()).for_column());
        fields.push(InputValueDef::new("_gte", value_ty.clone()).for_column());
        fields.push(InputValueDef::new("_in", value_list_ty.clone()).for_column());
        fields.push(InputValueDef::new("_is_null", TypeRef::named("Boolean")));
        fields.push(InputValueDef::new("_lt", value_ty.clone()).for_column());
        fields.push(InputValueDef::new("_lte", value_ty.clone()).for_column());
        fields.push(InputValueDef::new("_neq", value_ty.clone()).for_column());
        fields.push(InputValueDef::new("_nin", value_list_ty).for_column());

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
                    .describe("is the array contained in the given array value")
                    .for_column(),
            );
            fields.push(
                InputValueDef::new("_contains", value_ty.clone())
                    .describe("does the array contain the given value")
                    .for_column(),
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
            fields.push(
                InputValueDef::new("_jsonb_path_exists", TypeRef::named("String"))
                    .describe("does the jsonpath return any item for the specified JSON value"),
            );
            fields.push(
                InputValueDef::new("_jsonb_path_match", TypeRef::named("String")).describe(
                    "does the jsonpath predicate check return true for the specified JSON value",
                ),
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
        for (_, target) in &casts {
            self.ensure_comparison_exp(target);
        }
        name
    }

    /// Names of every type some field, argument, input field or directive argument refers to.
    fn referenced_types(&self) -> HashSet<String> {
        let mut used = HashSet::new();
        let add_args = |used: &mut HashSet<String>, args: &IndexMap<String, InputValueDef>| {
            for arg in args.values() {
                used.insert(arg.ty.base().to_owned());
            }
        };
        for ty in self.schema.types.values() {
            match ty {
                TypeDef::Object(obj) => {
                    for field in obj.fields.values() {
                        used.insert(field.ty.base().to_owned());
                        add_args(&mut used, &field.args);
                    }
                }
                TypeDef::Interface(i) => {
                    for field in i.fields.values() {
                        used.insert(field.ty.base().to_owned());
                        add_args(&mut used, &field.args);
                    }
                }
                TypeDef::InputObject(obj) => add_args(&mut used, &obj.fields),
                // Union members are objects, which are never pruned.
                TypeDef::Enum(_) | TypeDef::Union(_) | TypeDef::Scalar(_) => {}
            }
        }
        for directive in &self.schema.directives {
            add_args(&mut used, &directive.args);
        }
        used
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
        // V2 lists only the types reachable from the roots: drop scalars nothing refers to
        // (an unused `ID`, `Float`, …) and logical model objects only native query types
        // were built from.
        let used = self.referenced_types();
        let unused_objects = std::mem::take(&mut self.unused_model_objects);
        self.schema.types.retain(|name, ty| {
            let prunable = matches!(ty, TypeDef::Scalar(_)) || unused_objects.contains(name);
            !prunable || used.contains(name.as_str())
        });
        // V2 lists types, fields, arguments, input fields (and enum values) sorted by name.
        self.schema.types.sort_keys();
        for ty in self.schema.types.values_mut() {
            match ty {
                TypeDef::Object(obj) => {
                    obj.fields.sort_keys();
                    for field in obj.fields.values_mut() {
                        field.args.sort_keys();
                    }
                }
                TypeDef::Interface(i) => {
                    i.fields.sort_keys();
                    for field in i.fields.values_mut() {
                        field.args.sort_keys();
                    }
                }
                TypeDef::InputObject(obj) => obj.fields.sort_keys(),
                // Enum values keep definition order; introspection sorts them.
                TypeDef::Enum(_) | TypeDef::Union(_) | TypeDef::Scalar(_) => {}
            }
        }
    }
}
