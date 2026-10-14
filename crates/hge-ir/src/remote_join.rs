//! Walking a database result along the remote relationship fields its IR selects
//! (spec 08 §9.3): the executor visits each join's slot to collect arguments, then again to
//! put the results in.

use serde_json::Value as Json;

use crate::{
    AggregateSelectField, Field, Mutation, MutationOutput, MutationResponseField, QueryRoot,
    RemoteJoin,
};

/// Does `fields` (or anything nested in it) select a remote relationship?
pub fn fields_have_joins(fields: &[(String, Field)]) -> bool {
    fields.iter().any(|(_, f)| match f {
        Field::RemoteJoin(_) => true,
        Field::ObjectRelationship(r) | Field::ArrayRelationship(r) => {
            fields_have_joins(&r.select.fields)
        }
        Field::ComputedTable(s) => fields_have_joins(&s.fields),
        Field::ArrayAggregate(a) => aggregate_has_joins(&a.select.fields),
        _ => false,
    })
}

fn aggregate_has_joins(fields: &[(String, AggregateSelectField)]) -> bool {
    fields.iter().any(|(_, f)| match f {
        AggregateSelectField::Nodes(n) => fields_have_joins(n),
        _ => false,
    })
}

impl QueryRoot {
    pub fn has_remote_joins(&self) -> bool {
        match self {
            QueryRoot::Select(s) | QueryRoot::SelectOne(s) => fields_have_joins(&s.fields),
            QueryRoot::Aggregate(a) => aggregate_has_joins(&a.fields),
            QueryRoot::Stream(s) => fields_have_joins(&s.select.fields),
            QueryRoot::Count(_) => false,
        }
    }

    /// Visit the remote join slots of this root field's value.
    pub fn visit_remote_joins(&self, value: &mut Json, f: &mut dyn FnMut(&RemoteJoin, &mut Json)) {
        match self {
            QueryRoot::Select(s) | QueryRoot::SelectOne(s) => visit_rows(&s.fields, value, f),
            QueryRoot::Aggregate(a) => visit_aggregate(&a.fields, value, f),
            QueryRoot::Stream(s) => visit_rows(&s.select.fields, value, f),
            QueryRoot::Count(_) => {}
        }
    }
}

impl Mutation {
    pub fn has_remote_joins(&self) -> bool {
        match self {
            Mutation::Insert(i) => output_has_joins(&i.output),
            Mutation::Update(u) => output_has_joins(&u.output),
            Mutation::UpdateMany(us) => us.iter().any(|u| output_has_joins(&u.output)),
            Mutation::Delete(d) => output_has_joins(&d.output),
            Mutation::Function(q) => q.has_remote_joins(),
        }
    }

    pub fn visit_remote_joins(&self, value: &mut Json, f: &mut dyn FnMut(&RemoteJoin, &mut Json)) {
        match self {
            Mutation::Insert(i) => visit_output(&i.output, value, f),
            Mutation::Update(u) => visit_output(&u.output, value, f),
            Mutation::UpdateMany(us) => {
                if let Json::Array(items) = value {
                    for (u, item) in us.iter().zip(items) {
                        visit_output(&u.output, item, f);
                    }
                }
            }
            Mutation::Delete(d) => visit_output(&d.output, value, f),
            Mutation::Function(q) => q.visit_remote_joins(value, f),
        }
    }
}

fn output_has_joins(output: &MutationOutput) -> bool {
    match output {
        MutationOutput::Single(fields) => fields_have_joins(fields),
        MutationOutput::Multi(fields) => fields.iter().any(|(_, f)| match f {
            MutationResponseField::Returning(r) => fields_have_joins(r),
            _ => false,
        }),
    }
}

fn visit_output(
    output: &MutationOutput,
    value: &mut Json,
    f: &mut dyn FnMut(&RemoteJoin, &mut Json),
) {
    match output {
        MutationOutput::Single(fields) => visit_rows(fields, value, f),
        MutationOutput::Multi(fields) => {
            let Json::Object(obj) = value else {
                return;
            };
            for (key, field) in fields {
                if let (MutationResponseField::Returning(r), Some(v)) = (field, obj.get_mut(key)) {
                    visit_rows(r, v, f);
                }
            }
        }
    }
}

/// `value` is a row, an array of rows, or `null`.
fn visit_rows(
    fields: &[(String, Field)],
    value: &mut Json,
    f: &mut dyn FnMut(&RemoteJoin, &mut Json),
) {
    match value {
        Json::Array(rows) => {
            for row in rows {
                visit_rows(fields, row, f);
            }
        }
        Json::Object(row) => {
            for (key, field) in fields {
                let Some(v) = row.get_mut(key) else {
                    continue;
                };
                match field {
                    Field::RemoteJoin(j) => f(j, v),
                    Field::ObjectRelationship(r) | Field::ArrayRelationship(r) => {
                        visit_rows(&r.select.fields, v, f);
                    }
                    Field::ComputedTable(s) => visit_rows(&s.fields, v, f),
                    Field::ArrayAggregate(a) => visit_aggregate(&a.select.fields, v, f),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn visit_aggregate(
    fields: &[(String, AggregateSelectField)],
    value: &mut Json,
    f: &mut dyn FnMut(&RemoteJoin, &mut Json),
) {
    let Json::Object(obj) = value else {
        return;
    };
    for (key, field) in fields {
        if let (AggregateSelectField::Nodes(n), Some(v)) = (field, obj.get_mut(key)) {
            visit_rows(n, v, f);
        }
    }
}
