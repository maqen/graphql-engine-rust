//! Lowering a normalised operation to IR.

use std::sync::Arc;

use hge_core::{Code, ColumnType, Error};
use hge_ir::{
    BoolExp, ColumnField, CompareOp, ComputedScalarField, DatabaseRootField, Field, FunctionCall,
    JsonPathElement, Mutation, MutationRootField, NativeQueryPart, NullsOrder, Operation, OrderBy,
    OrderByTarget, OrderDirection, QueryRoot, RelationshipSelect, RootField, RowSource, Select,
    SelectArgs, Value,
};
use serde_json::{Map, Value as Json};

use crate::aggregate::aggregate_relationship;
use crate::build::RoleSchema;
use crate::functions::{ComputedFieldInfo, ComputedReturn, ExposedAs, FunctionInfo};
use crate::introspection;
use crate::model::FieldKind;
use crate::native::{CodePart, NativeQueryInfo};
use crate::normalize::{NormalizedOperation, Selected, SelectedField};
use crate::scalars;
use crate::tables::{RelationshipInfo, TableInfo};

const NO_QUERIES_MESSAGE: &str = "There are no queries available to the current role. Either there are no sources or remote schemas configured, or the current role doesn't have the required permissions.";

pub fn lower(rs: &RoleSchema, op: &NormalizedOperation<'_>) -> Result<Operation, Error> {
    let lw = Lowerer { rs };
    let mut root_fields = Vec::with_capacity(op.fields.len());
    for field in &op.fields {
        let rf = match field.kind {
            Selected::Typename(t) => RootField::Static(Json::String(t.to_owned())),
            Selected::Field(def) => match &def.kind {
                FieldKind::SchemaField | FieldKind::TypeField => {
                    RootField::Static(introspection::resolve_root(&rs.schema, field))
                }
                FieldKind::NoQueriesAvailable => {
                    RootField::Static(Json::String(NO_QUERIES_MESSAGE.to_owned()))
                }
                FieldKind::SelectMany(table) => RootField::Database(Box::new(DatabaseRootField {
                    source: table.source.clone(),
                    query: QueryRoot::Select(lw.select(table, field, true)?),
                })),
                FieldKind::SelectByPk(table) => RootField::Database(Box::new(DatabaseRootField {
                    source: table.source.clone(),
                    query: QueryRoot::SelectOne(lw.select_by_pk(table, field)?),
                })),
                FieldKind::SelectStream(table) => {
                    RootField::Database(Box::new(DatabaseRootField {
                        source: table.source.clone(),
                        query: QueryRoot::Stream(Box::new(crate::subscription::lower_stream(
                            &lw, table, field,
                        )?)),
                    }))
                }
                FieldKind::Mutation(m) => crate::mutation::lower::root_field(&lw, m, field)?,
                FieldKind::Function(f) => lw.function(f, field)?,
                FieldKind::FunctionAggregate(f) => lw.function_aggregate(f, field)?,
                FieldKind::NativeQuery(nq) => lw.native_query(nq, field)?,
                FieldKind::Action(action) => crate::actions::lower_action(rs, action, field)?,
                FieldKind::AsyncActionQuery(action) => {
                    crate::actions::lower_async_query(rs, action, field)?
                }
                FieldKind::SelectAggregate(table) => {
                    RootField::Database(Box::new(DatabaseRootField {
                        source: table.source.clone(),
                        query: QueryRoot::Aggregate(lw.aggregate_select(table, field)?),
                    }))
                }
                other => {
                    return Err(Error::unexpected(format!(
                        "unexpected root field kind {other:?}"
                    )));
                }
            },
        };
        root_fields.push((field.alias.clone(), rf));
    }
    Ok(Operation {
        kind: op.kind,
        root_fields,
        multiple_top_level_fields: op.multiple_top_level_fields,
    })
}

/// Lower the selection on a table reached from outside a table (e.g. an action
/// relationship), with the role's permissions.
pub(crate) fn table_select(
    rs: &RoleSchema,
    table: &TableInfo,
    field: &SelectedField<'_>,
    with_args: bool,
) -> Result<Select, Error> {
    Lowerer { rs }.select(table, field, with_args)
}

pub(crate) struct Lowerer<'a> {
    pub(crate) rs: &'a RoleSchema,
}

impl Lowerer<'_> {
    pub(crate) fn target(&self, rel: &RelationshipInfo) -> Result<&Arc<TableInfo>, Error> {
        self.rs
            .tables
            .get(&rel.source, &rel.target)
            .ok_or_else(|| Error::unexpected(format!("unknown table {}", rel.target)))
    }

    /// The condition under which `column` of `table` is visible, if it is redacted for this
    /// (inherited) role.
    pub(crate) fn redaction(&self, table: &TableInfo, column: &str) -> Option<&BoolExp> {
        self.rs.select_access(table)?.redaction(column)
    }

    /// A column as an order-by / distinct-on target, redacted if it must be.
    fn column_target(&self, table: &TableInfo, column: &str) -> OrderByTarget {
        match self.redaction(table, column) {
            None => OrderByTarget::Column(column.to_owned()),
            Some(condition) => OrderByTarget::RedactedColumn {
                column: column.to_owned(),
                condition: Box::new(condition.clone()),
            },
        }
    }

    /// The role's select filter on `table`, unless it lets every row through. Without select
    /// access (the schema should never lead here) no row is visible.
    pub(crate) fn permission_filter(&self, table: &TableInfo) -> Option<BoolExp> {
        match self.rs.select_access(table) {
            Some(a) => a.row_filter().cloned(),
            None => Some(BoolExp::always_false()),
        }
    }

    pub(crate) fn select(
        &self,
        table: &TableInfo,
        field: &SelectedField<'_>,
        with_args: bool,
    ) -> Result<Select, Error> {
        let fields = self.fields(table, &field.selection)?;
        let args = if with_args {
            self.select_args(table, field)?
        } else {
            SelectArgs::default()
        };
        Ok(Select {
            table: table.name.clone(),
            fields,
            args,
            permission_filter: self.permission_filter(table),
            permission_limit: self.rs.select_access(table).and_then(|a| a.limit),
            from: None,
        })
    }

    fn select_by_pk(&self, table: &TableInfo, field: &SelectedField<'_>) -> Result<Select, Error> {
        let mut select = self.select(table, field, false)?;
        let pk = table
            .primary_key
            .as_ref()
            .ok_or_else(|| Error::unexpected("by_pk field on table without primary key"))?;
        let mut conds = Vec::with_capacity(pk.len());
        for col in pk {
            let col = &table.columns[col];
            let value = field.arg(&col.gql_name).ok_or_else(|| {
                Error::validation(format!("missing required field '{}'", col.gql_name))
                    .with_path(field.arg_path(&col.gql_name))
            })?;
            let cmp = BoolExp::Compare {
                column: col.name.clone(),
                ty: col.ty.clone(),
                op: CompareOp::Eq(to_value(value, &col.ty, &field.arg_path(&col.gql_name))?),
            };
            conds.push(match self.redaction(table, &col.name) {
                None => cmp,
                Some(condition) => redacted_comparison(condition, cmp),
            });
        }
        select.args.where_ = Some(BoolExp::And(conds));
        Ok(select)
    }

    pub(crate) fn fields(
        &self,
        table: &TableInfo,
        selection: &[SelectedField<'_>],
    ) -> Result<Vec<(String, Field)>, Error> {
        let mut out = Vec::with_capacity(selection.len());
        for sel in selection {
            let f = match sel.kind {
                Selected::Typename(t) => Field::Typename(t.to_owned()),
                Selected::Field(def) => match &def.kind {
                    FieldKind::Column { column, ty } => {
                        let json_path = match sel.arg("path") {
                            Some(Json::String(p)) => Some(parse_json_path(p).map_err(|e| {
                                Error::validation(e).with_path(sel.arg_path("path"))
                            })?),
                            _ => None,
                        };
                        Field::Column(ColumnField {
                            column: column.clone(),
                            ty: ty.clone(),
                            stringify: self.rs.options.stringify_numeric_types
                                && scalars::is_big_num(ty),
                            json_path,
                            redact: self.redaction(table, column).cloned().map(Box::new),
                        })
                    }
                    FieldKind::ObjectRelationship(rel) => {
                        let target = self.target(rel)?;
                        Field::ObjectRelationship(Box::new(RelationshipSelect {
                            mapping: rel.mapping.clone(),
                            select: self.select(target, sel, false)?,
                        }))
                    }
                    FieldKind::ArrayRelationship(rel) => {
                        let target = self.target(rel)?;
                        Field::ArrayRelationship(Box::new(RelationshipSelect {
                            mapping: rel.mapping.clone(),
                            select: self.select(target, sel, true)?,
                        }))
                    }
                    FieldKind::ArrayRelationshipAggregate(rel) => {
                        Field::ArrayAggregate(Box::new(self.relationship_aggregate(rel, sel)?))
                    }
                    FieldKind::ComputedField(cf) => {
                        let user_args = match sel.arg("args") {
                            Some(Json::Object(o)) => Some(o),
                            Some(Json::Null) => {
                                return Err(Error::validation(format!(
                                    "expected an object for type '{}', but found null",
                                    cf.args_type_name(table.type_name())
                                ))
                                .with_path(sel.arg_path("args")));
                            }
                            _ => None,
                        };
                        let call =
                            self.computed_call(table, cf, user_args, &sel.arg_path("args"))?;
                        match &cf.returns {
                            ComputedReturn::Scalar(ty) => {
                                let json_path = match sel.arg("path") {
                                    Some(Json::String(p)) => {
                                        Some(parse_json_path(p).map_err(|e| {
                                            Error::validation(e).with_path(sel.arg_path("path"))
                                        })?)
                                    }
                                    _ => None,
                                };
                                Field::ComputedScalar(Box::new(ComputedScalarField {
                                    call,
                                    ty: ty.clone(),
                                    stringify: self.rs.options.stringify_numeric_types
                                        && scalars::is_big_num(ty),
                                    json_path,
                                }))
                            }
                            ComputedReturn::Table(target) => {
                                let target = self.table(&table.source, target)?;
                                let mut select = self.select(target, sel, true)?;
                                select.from = Some(RowSource::Function(call));
                                Field::ComputedTable(Box::new(select))
                            }
                        }
                    }
                    other => {
                        return Err(Error::unexpected(format!(
                            "unexpected field kind {other:?} in table selection"
                        )));
                    }
                },
            };
            out.push((sel.alias.clone(), f));
        }
        Ok(out)
    }

    pub(crate) fn table(
        &self,
        source: &str,
        name: &hge_core::QualifiedTable,
    ) -> Result<&Arc<TableInfo>, Error> {
        self.rs
            .tables
            .get(source, name)
            .ok_or_else(|| Error::unexpected(format!("unknown table {name}")))
    }

    /// The call of a computed field on the current row of `table`. `user_args` is the
    /// field's `args` argument (at `path`).
    fn computed_call(
        &self,
        table: &TableInfo,
        cf: &ComputedFieldInfo,
        user_args: Option<&Map<String, Json>>,
        path: &str,
    ) -> Result<FunctionCall, Error> {
        let mut inputs = vec![];
        for (i, gql_name, arg) in cf.input_args() {
            if let Some(v) = user_args.and_then(|a| a.get(&gql_name)) {
                inputs.push((i, to_value(v, &arg.ty, &format!("{path}.{gql_name}"))?));
            }
        }
        cf.call(&table.name, inputs)
            .map_err(|m| Error::new(Code::NotSupported, m).with_path(path.to_owned()))
    }

    /// A tracked function's root field: its rows, read in a query or (for mutations) in the
    /// mutation transaction.
    fn function(&self, f: &FunctionInfo, field: &SelectedField<'_>) -> Result<RootField, Error> {
        let table = self.table(&f.source, &f.returns)?;
        let mut select = self.select(table, field, true)?;
        select.from = Some(RowSource::Function(self.function_call(f, field)?));
        let query = if f.returns_set {
            QueryRoot::Select(select)
        } else {
            QueryRoot::SelectOne(select)
        };
        Ok(match f.exposed_as {
            ExposedAs::Query => RootField::Database(Box::new(DatabaseRootField {
                source: f.source.clone(),
                query,
            })),
            ExposedAs::Mutation => RootField::Mutation(Box::new(MutationRootField {
                source: f.source.clone(),
                mutation: Mutation::Function(Box::new(query)),
            })),
        })
    }

    /// A native query's root field: its logical model's rows, read from its code with the
    /// field's `args` bound as parameters.
    fn native_query(
        &self,
        nq: &NativeQueryInfo,
        field: &SelectedField<'_>,
    ) -> Result<RootField, Error> {
        let model = self
            .rs
            .tables
            .logical_model(&nq.source, &nq.returns)
            .ok_or_else(|| Error::unexpected(format!("unknown logical model {}", nq.returns)))?;
        let mut select = self.select(model, field, true)?;
        let args = match field.arg("args") {
            Some(Json::Object(o)) => Some(o),
            _ => None,
        };
        let path = field.arg_path("args");
        let mut parts = Vec::with_capacity(nq.code.len());
        for part in &nq.code {
            parts.push(match part {
                CodePart::Sql(sql) => NativeQueryPart::Sql(sql.clone()),
                CodePart::Arg(name) => {
                    let arg = nq.arguments.get(name).ok_or_else(|| {
                        Error::unexpected(format!("undeclared native query argument {name}"))
                    })?;
                    let value = args.and_then(|a| a.get(name)).unwrap_or(&Json::Null);
                    NativeQueryPart::Arg(to_value(value, &arg.ty, &format!("{path}.{name}"))?)
                }
            });
        }
        select.from = Some(RowSource::NativeQuery(parts));
        Ok(RootField::Database(Box::new(DatabaseRootField {
            source: nq.source.clone(),
            query: QueryRoot::Select(select),
        })))
    }

    /// `<function>_aggregate`: aggregates over the function's rows.
    fn function_aggregate(
        &self,
        f: &FunctionInfo,
        field: &SelectedField<'_>,
    ) -> Result<RootField, Error> {
        let table = self.table(&f.source, &f.returns)?;
        let mut select = self.aggregate_select(table, field)?;
        select.from = Some(RowSource::Function(self.function_call(f, field)?));
        Ok(RootField::Database(Box::new(DatabaseRootField {
            source: f.source.clone(),
            query: QueryRoot::Aggregate(select),
        })))
    }

    /// The call of a tracked function with the field's `args`.
    fn function_call(
        &self,
        f: &FunctionInfo,
        field: &SelectedField<'_>,
    ) -> Result<FunctionCall, Error> {
        let path = field.arg_path("args");
        let user_args = match field.arg("args") {
            Some(Json::Object(o)) => Some(o),
            Some(Json::Null) => {
                return Err(Error::validation(format!(
                    "expected an object for type '{}', but found null",
                    f.args_type_name()
                ))
                .with_path(path));
            }
            _ => None,
        };
        let mut inputs = vec![];
        for (i, gql_name, arg) in f.input_args() {
            if let Some(v) = user_args.and_then(|a| a.get(&gql_name)) {
                inputs.push((i, to_value(v, &arg.ty, &format!("{path}.{gql_name}"))?));
            }
        }
        f.call(inputs)
            .map_err(|m| Error::new(Code::NotSupported, m).with_path(path))
    }

    pub(crate) fn select_args(
        &self,
        table: &TableInfo,
        field: &SelectedField<'_>,
    ) -> Result<SelectArgs, Error> {
        let mut args = SelectArgs::default();
        if let Some(w) = field.arg("where")
            && let Json::Object(obj) = w
        {
            args.where_ = self.bool_exp(table, obj, &field.arg_path("where"))?;
        }
        if let Some(Json::Array(items)) = field.arg("order_by") {
            let path = field.arg_path("order_by");
            for (i, item) in items.iter().enumerate() {
                if let Json::Object(obj) = item {
                    self.order_by(table, obj, &format!("{path}[{i}]"), &mut args.order_by)?;
                }
            }
        }
        args.limit = non_negative(field.arg("limit"), &field.arg_path("limit"))?;
        args.offset = non_negative(field.arg("offset"), &field.arg_path("offset"))?;
        if let Some(Json::Array(cols)) = field.arg("distinct_on") {
            for c in cols {
                let name = c.as_str().unwrap_or_default();
                let col = table.column_by_gql_name(name).ok_or_else(|| {
                    Error::validation(format!("unknown column '{name}'"))
                        .with_path(field.arg_path("distinct_on"))
                })?;
                args.distinct_on.push(self.column_target(table, &col.name));
            }
            if !args.distinct_on.is_empty() && !args.order_by.is_empty() {
                let prefix_ok = args.order_by.len() >= args.distinct_on.len()
                    && args
                        .distinct_on
                        .iter()
                        .zip(&args.order_by)
                        .all(|(d, o)| *d == o.target);
                if !prefix_ok {
                    return Err(Error::validation(
                        "\"distinct_on\" columns must match initial \"order_by\" columns",
                    )
                    .with_path(field.arg_path("distinct_on")));
                }
            }
        }
        Ok(args)
    }

    pub(crate) fn bool_exp(
        &self,
        table: &TableInfo,
        obj: &Map<String, Json>,
        path: &str,
    ) -> Result<Option<BoolExp>, Error> {
        let mut items = Vec::with_capacity(obj.len());
        for (key, value) in obj {
            if value.is_null() {
                continue;
            }
            let item_path = format!("{path}.{key}");
            match key.as_str() {
                "_and" | "_or" => {
                    let mut parts = vec![];
                    for (i, v) in as_list(value).iter().enumerate() {
                        if let Json::Object(o) = v {
                            parts.push(
                                self.bool_exp(table, o, &format!("{item_path}[{i}]"))?
                                    .unwrap_or_else(BoolExp::always_true),
                            );
                        }
                    }
                    items.push(if key == "_and" {
                        BoolExp::And(parts)
                    } else {
                        BoolExp::Or(parts)
                    });
                }
                "_not" => {
                    if let Json::Object(o) = value {
                        let inner = self
                            .bool_exp(table, o, &item_path)?
                            .unwrap_or_else(BoolExp::always_true);
                        items.push(BoolExp::Not(Box::new(inner)));
                    }
                }
                _ => {
                    if let Some(col) = table.column_by_gql_name(key) {
                        if let Json::Object(ops) = value {
                            let cmps =
                                self.comparisons(&col.ty, ops, &item_path)?
                                    .into_iter()
                                    .map(|op| BoolExp::Compare {
                                        column: col.name.clone(),
                                        ty: col.ty.clone(),
                                        op,
                                    });
                            match self.redaction(table, &col.name) {
                                None => items.extend(cmps),
                                Some(condition) => items
                                    .extend(cmps.map(|cmp| redacted_comparison(condition, cmp))),
                            }
                        }
                    } else if let Some(cf) = table.computed_field(key) {
                        let Json::Object(o) = value else { continue };
                        let call = self.computed_call(table, cf, None, &item_path)?;
                        match &cf.returns {
                            ComputedReturn::Scalar(ty) => {
                                for op in self.comparisons(ty, o, &item_path)? {
                                    items.push(BoolExp::CompareComputed {
                                        call: call.clone(),
                                        ty: ty.clone(),
                                        op,
                                    });
                                }
                            }
                            ComputedReturn::Table(target) => {
                                let target = self.table(&table.source, target)?;
                                let user = self.bool_exp(target, o, &item_path)?;
                                let permission = self
                                    .permission_filter(target)
                                    .map(|p| BoolExp::Rooted(Box::new(p)));
                                let exp = BoolExp::and(permission, user)
                                    .unwrap_or_else(BoolExp::always_true);
                                items.push(BoolExp::ComputedTable {
                                    call,
                                    exp: Box::new(exp),
                                });
                            }
                        }
                    } else if let Some(rel) = table.relationship_by_name(key) {
                        let target = self.target(rel)?;
                        if let Json::Object(o) = value {
                            let user = self.bool_exp(target, o, &item_path)?;
                            // Filtering through a relationship never sees rows the role
                            // cannot select.
                            let permission = self
                                .permission_filter(target)
                                .map(|p| BoolExp::Rooted(Box::new(p)));
                            let exp =
                                BoolExp::and(permission, user).unwrap_or_else(BoolExp::always_true);
                            items.push(BoolExp::Relationship {
                                table: target.name.clone(),
                                mapping: rel.mapping.clone(),
                                exp: Box::new(exp),
                            });
                        }
                    } else if let Some(rel) = aggregate_relationship(table, key) {
                        items.push(self.aggregation_predicate(rel, value, &item_path)?);
                    } else {
                        return Err(Error::validation(format!(
                            "field '{key}' not found in type: '{}'",
                            table.bool_exp_name()
                        ))
                        .with_path(item_path));
                    }
                }
            }
        }
        Ok(match items.len() {
            0 => None,
            1 => items.pop(),
            _ => Some(BoolExp::And(items)),
        })
    }

    pub(crate) fn comparisons(
        &self,
        ty: &ColumnType,
        ops: &Map<String, Json>,
        path: &str,
    ) -> Result<Vec<CompareOp>, Error> {
        let mut out = vec![];
        for (op, v) in ops {
            let op_path = format!("{path}.{op}");
            if v.is_null() {
                if self.rs.options.dangerously_collapse_booleans || op == "_is_null" {
                    continue;
                }
                return Err(Error::validation(format!(
                    "unexpected null value for type '{}'",
                    scalars::scalar_name(&ty.element())
                ))
                .with_path(op_path));
            }
            if op == "_cast" {
                out.extend(self.cast_comparisons(ty, v, &op_path)?);
                continue;
            }
            if let Some(cmp) = crate::operators::graphql_spatial_op(op, v, ty, &op_path) {
                out.push(cmp?);
                continue;
            }
            let val = |t: &ColumnType| to_value(v, t, &op_path);
            let text_ty = ColumnType::builtin("text");
            let cmp = match op.as_str() {
                "_eq" => CompareOp::Eq(val(ty)?),
                "_neq" => CompareOp::Neq(val(ty)?),
                "_gt" => CompareOp::Gt(val(ty)?),
                "_lt" => CompareOp::Lt(val(ty)?),
                "_gte" => CompareOp::Gte(val(ty)?),
                "_lte" => CompareOp::Lte(val(ty)?),
                "_in" => CompareOp::In(val(&ty.array_of())?),
                "_nin" => CompareOp::Nin(val(&ty.array_of())?),
                "_like" => CompareOp::Like(val(ty)?),
                "_nlike" => CompareOp::NLike(val(ty)?),
                "_ilike" => CompareOp::ILike(val(ty)?),
                "_nilike" => CompareOp::NILike(val(ty)?),
                "_similar" => CompareOp::Similar(val(ty)?),
                "_nsimilar" => CompareOp::NSimilar(val(ty)?),
                "_regex" => CompareOp::Regex(val(ty)?),
                "_nregex" => CompareOp::NRegex(val(ty)?),
                "_iregex" => CompareOp::IRegex(val(ty)?),
                "_niregex" => CompareOp::NIRegex(val(ty)?),
                "_is_null" => CompareOp::IsNull(v.as_bool().unwrap_or(false)),
                "_contains" => CompareOp::Contains(val(ty)?),
                "_contained_in" => CompareOp::ContainedIn(val(ty)?),
                "_has_key" => CompareOp::HasKey(val(&text_ty)?),
                "_has_keys_any" => CompareOp::HasKeysAny(val(&text_ty.array_of())?),
                "_has_keys_all" => CompareOp::HasKeysAll(val(&text_ty.array_of())?),
                "_jsonb_path_exists" => {
                    CompareOp::JsonbPathExists(val(&ColumnType::builtin("jsonpath"))?)
                }
                "_jsonb_path_match" => {
                    CompareOp::JsonbPathMatch(val(&ColumnType::builtin("jsonpath"))?)
                }
                other => {
                    return Err(Error::new(
                        hge_core::Code::NotSupported,
                        format!("the operator '{other}' is not supported yet"),
                    )
                    .with_path(op_path));
                }
            };
            out.push(cmp);
        }
        Ok(out)
    }

    /// `_cast: {<target>: {<ops>}}`: the comparisons on the column cast to each target.
    fn cast_comparisons(
        &self,
        ty: &ColumnType,
        v: &Json,
        path: &str,
    ) -> Result<Vec<CompareOp>, Error> {
        let mut out = vec![];
        let Json::Object(targets) = v else {
            return Ok(out);
        };
        for (field, ops) in targets {
            let item_path = format!("{path}.{field}");
            let Some(to) = crate::operators::cast_target(ty, field) else {
                continue;
            };
            let ops = match ops {
                Json::Object(ops) => ops,
                Json::Null if self.rs.options.dangerously_collapse_booleans => continue,
                _ => {
                    return Err(Error::validation(format!(
                        "unexpected null value for type '{}'",
                        scalars::comparison_exp_name(&to)
                    ))
                    .with_path(item_path));
                }
            };
            for op in self.comparisons(&to, ops, &item_path)? {
                out.push(CompareOp::Cast {
                    to: to.clone(),
                    op: Box::new(op),
                });
            }
        }
        Ok(out)
    }

    pub(crate) fn order_by(
        &self,
        table: &TableInfo,
        obj: &Map<String, Json>,
        path: &str,
        out: &mut Vec<OrderBy>,
    ) -> Result<(), Error> {
        for (key, value) in obj {
            if value.is_null() {
                continue;
            }
            let item_path = format!("{path}.{key}");
            if let Some(col) = table.column_by_gql_name(key) {
                let (direction, nulls) = order_type(value.as_str().unwrap_or("asc"));
                out.push(OrderBy {
                    target: self.column_target(table, &col.name),
                    direction,
                    nulls,
                });
            } else if let Some(cf) = table.computed_field(key)
                && cf.scalar_type().is_some()
            {
                let (direction, nulls) = order_type(value.as_str().unwrap_or("asc"));
                out.push(OrderBy {
                    target: OrderByTarget::ComputedField(
                        self.computed_call(table, cf, None, &item_path)?,
                    ),
                    direction,
                    nulls,
                });
            } else if let Some(rel) = table.relationship_by_name(key) {
                let target = self.target(rel)?;
                if let Json::Object(o) = value {
                    let mut nested = vec![];
                    self.order_by(target, o, &item_path, &mut nested)?;
                    for item in nested {
                        out.push(OrderBy {
                            target: OrderByTarget::ObjectRelationship {
                                table: target.name.clone(),
                                mapping: rel.mapping.clone(),
                                filter: self.permission_filter(target).map(Box::new),
                                target: Box::new(item.target),
                            },
                            direction: item.direction,
                            nulls: item.nulls,
                        });
                    }
                }
            } else if let Some(rel) = aggregate_relationship(table, key) {
                self.aggregate_order_by(rel, value, &item_path, out)?;
            } else {
                return Err(Error::validation(format!(
                    "field '{key}' not found in type: '{}'",
                    table.order_by_name()
                ))
                .with_path(item_path));
            }
        }
        Ok(())
    }
}

/// Compare a redacted column like V2 compares `CASE WHEN condition THEN col ELSE NULL END`.
fn redacted_comparison(condition: &BoolExp, cmp: BoolExp) -> BoolExp {
    BoolExp::Redacted {
        condition: Box::new(BoolExp::Rooted(Box::new(condition.clone()))),
        exp: Box::new(cmp),
    }
}

pub(crate) fn order_type(s: &str) -> (OrderDirection, NullsOrder) {
    match s {
        "asc_nulls_first" => (OrderDirection::Asc, NullsOrder::First),
        "desc" | "desc_nulls_first" => (OrderDirection::Desc, NullsOrder::First),
        "desc_nulls_last" => (OrderDirection::Desc, NullsOrder::Last),
        _ => (OrderDirection::Asc, NullsOrder::Last),
    }
}

fn as_list(v: &Json) -> Vec<Json> {
    match v {
        Json::Array(items) => items.clone(),
        other => vec![other.clone()],
    }
}

pub(crate) fn non_negative(v: Option<&Json>, path: &str) -> Result<Option<u64>, Error> {
    match v {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Number(n)) => {
            match n.as_i64() {
                Some(i) if i >= 0 => Ok(Some(i as u64)),
                _ => Err(Error::validation("expecting a non-negative integer")
                    .with_path(path.to_owned())),
            }
        }
        Some(_) => {
            Err(Error::validation("expecting a non-negative integer").with_path(path.to_owned()))
        }
    }
}

/// Convert a coerced JSON input value to a typed IR value (text representation).
pub fn to_value(v: &Json, ty: &ColumnType, path: &str) -> Result<Value, Error> {
    Ok(Value::Literal {
        text: to_text(v, ty, path)?,
        ty: ty.clone(),
    })
}

fn to_text(v: &Json, ty: &ColumnType, path: &str) -> Result<Option<String>, Error> {
    if v.is_null() {
        return Ok(None);
    }
    if ty.is_array() {
        return match v {
            Json::Array(items) => {
                let elem = ColumnType {
                    dimensions: ty.dimensions - 1,
                    ..ty.clone()
                };
                let mut out = String::from("{");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    match to_text(item, &elem, path)? {
                        None => out.push_str("NULL"),
                        Some(t) if elem.is_array() => out.push_str(&t),
                        Some(t) => {
                            out.push('"');
                            for c in t.chars() {
                                if c == '"' || c == '\\' {
                                    out.push('\\');
                                }
                                out.push(c);
                            }
                            out.push('"');
                        }
                    }
                }
                out.push('}');
                Ok(Some(out))
            }
            // A string is passed through as a Postgres array literal.
            Json::String(s) => Ok(Some(s.clone())),
            _ => Err(Error::validation(format!(
                "expected a list for an array of type '{}'",
                scalars::scalar_name(&ty.element())
            ))
            .with_path(path.to_owned())),
        };
    }
    if scalars::is_json_like(ty) {
        return Ok(Some(v.to_string()));
    }
    if scalars::is_geo(ty) {
        crate::geojson::validate(v)
            .map_err(|m| Error::new(Code::ParseFailed, m).with_path(path.to_owned()))?;
        return Ok(Some(v.to_string()));
    }
    Ok(Some(match v {
        Json::String(s) => s.clone(),
        Json::Number(n) if ty.name == "numeric" => numeric_text(n),
        Json::Number(n) => n.to_string(),
        Json::Bool(b) => b.to_string(),
        other => other.to_string(),
    }))
}

/// V2 reads JSON numbers as Haskell `Scientific`s and sends them to Postgres in their `show`
/// form. That changes a `numeric`'s scale: `2` is stored as `2.0`, `-1E-30` as `-1.0e-30`
/// (one more digit of scale than `-1e-30`).
fn numeric_text(n: &serde_json::Number) -> String {
    scientific_show(&n.to_string()).unwrap_or_else(|| n.to_string())
}

/// Haskell's `show` of the `Scientific` written as `s` (`formatScientific Generic Nothing`):
/// fixed notation for 0.1 <= |x| < 10^7, otherwise `d.ddde<exp>`; always a fractional digit.
fn scientific_show(s: &str) -> Option<String> {
    let (negative, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(i) => (
            &s[..i],
            s[i + 1..].trim_start_matches('+').parse::<i64>().ok()?,
        ),
        None => (s, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if !(int.chars().chain(frac.chars())).all(|c| c.is_ascii_digit()) {
        return None;
    }
    // value = 0.<digits> * 10^e, digits without leading or trailing zeros.
    let all: String = format!("{int}{frac}");
    let leading = all.len() - all.trim_start_matches('0').len();
    let digits = all.trim_matches('0');
    let sign = if negative { "-" } else { "" };
    if digits.is_empty() {
        return Some(format!("{sign}0.0"));
    }
    let e = int.len() as i64 - leading as i64 + exp;
    let shown = if !(0..=7).contains(&e) {
        let (first, rest) = digits.split_at(1);
        let rest = if rest.is_empty() { "0" } else { rest };
        format!("{first}.{rest}e{}", e - 1)
    } else if e == 0 {
        format!("0.{digits}")
    } else {
        let e = e as usize;
        let int: String = digits
            .chars()
            .chain(std::iter::repeat('0'))
            .take(e)
            .collect();
        let frac = digits.get(e..).filter(|f| !f.is_empty()).unwrap_or("0");
        format!("{int}.{frac}")
    };
    Some(format!("{sign}{shown}"))
}

#[cfg(test)]
mod scientific_tests {
    use super::scientific_show;

    #[test]
    fn matches_haskell_show() {
        for (input, shown) in [
            ("2", "2.0"),
            ("-2", "-2.0"),
            ("0", "0.0"),
            ("0.5", "0.5"),
            ("0.05", "5.0e-2"),
            ("12.25", "12.25"),
            ("1234567", "1234567.0"),
            ("10000000", "1.0e7"),
            ("123456789", "1.23456789e8"),
            ("-1e-30", "-1.0e-30"),
            ("-1E-30", "-1.0e-30"),
            ("1.5e3", "1500.0"),
            ("12344.57", "12344.57"),
        ] {
            assert_eq!(scientific_show(input).as_deref(), Some(shown), "{input}");
        }
    }
}

/// Parse a V2 JSON path argument (`$.a.b[0]`, `a`, `['key']`, `objs[0]["k"]`).
pub fn parse_json_path(input: &str) -> Result<Vec<JsonPathElement>, String> {
    let s = input.strip_prefix('$').unwrap_or(input);
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = vec![];
    let err = |msg: &str| format!("parse json path error: {msg}: {input}");
    while i < chars.len() {
        match chars[i] {
            '.' => {
                i += 1;
                if i < chars.len() && chars[i] == '[' {
                    continue;
                }
                let start = i;
                while i < chars.len() && chars[i] != '.' && chars[i] != '[' {
                    i += 1;
                }
                if start == i {
                    return Err(err("empty key"));
                }
                out.push(JsonPathElement::Key(chars[start..i].iter().collect()));
            }
            '[' => {
                i += 1;
                if i < chars.len() && (chars[i] == '\'' || chars[i] == '"') {
                    let quote = chars[i];
                    i += 1;
                    let start = i;
                    while i < chars.len() && chars[i] != quote {
                        i += 1;
                    }
                    if i >= chars.len() {
                        return Err(err("unterminated string"));
                    }
                    out.push(JsonPathElement::Key(chars[start..i].iter().collect()));
                    i += 1;
                } else {
                    let start = i;
                    while i < chars.len() && chars[i] != ']' {
                        i += 1;
                    }
                    let idx: String = chars[start..i].iter().collect();
                    out.push(JsonPathElement::Index(
                        idx.trim().parse().map_err(|_| err("invalid array index"))?,
                    ));
                }
                if i >= chars.len() || chars[i] != ']' {
                    return Err(err("expected ']'"));
                }
                i += 1;
            }
            _ if out.is_empty() && i == 0 => {
                let start = i;
                while i < chars.len() && chars[i] != '.' && chars[i] != '[' {
                    i += 1;
                }
                out.push(JsonPathElement::Key(chars[start..i].iter().collect()));
            }
            c => return Err(err(&format!("unexpected character '{c}'"))),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use JsonPathElement::{Index, Key};

    #[test]
    fn json_paths() {
        assert_eq!(parse_json_path("$").unwrap(), vec![]);
        assert_eq!(
            parse_json_path(".obj.c1").unwrap(),
            vec![Key("obj".into()), Key("c1".into())]
        );
        assert_eq!(parse_json_path("a").unwrap(), vec![Key("a".into())]);
        assert_eq!(
            parse_json_path("arr[0]").unwrap(),
            vec![Key("arr".into()), Index(0)]
        );
        assert_eq!(
            parse_json_path("['!@#$%^']").unwrap(),
            vec![Key("!@#$%^".into())]
        );
        assert_eq!(
            parse_json_path("objs.[0].[\"你好\"]").unwrap(),
            vec![Key("objs".into()), Index(0), Key("你好".into())]
        );
        assert_eq!(
            parse_json_path("translations['hello world!']").unwrap(),
            vec![Key("translations".into()), Key("hello world!".into())]
        );
    }

    #[test]
    fn array_literals() {
        let ty = ColumnType::builtin("text").array_of();
        let v = serde_json::json!(["a", "b\"c", null, "d\\e"]);
        assert_eq!(
            to_text(&v, &ty, "$").unwrap().unwrap(),
            r#"{"a","b\"c",NULL,"d\\e"}"#
        );
    }
}
