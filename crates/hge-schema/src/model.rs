//! The GraphQL schema as data.
//!
//! One [`Schema`] is built per role. It is used three ways: to validate requests, to answer
//! introspection, and — through the [`FieldKind`] annotations — to lower requests to IR.

use std::sync::Arc;

use hge_core::ColumnType;
use indexmap::IndexMap;

use crate::tables::{RelationshipInfo, TableInfo};

#[derive(Debug, Clone)]
pub struct Schema {
    pub types: IndexMap<String, TypeDef>,
    pub query_type: String,
    pub mutation_type: Option<String>,
    pub subscription_type: Option<String>,
    pub directives: Vec<DirectiveDef>,
    /// `__schema` / `__type` allowed (metadata `graphql_schema_introspection`).
    pub introspection_enabled: bool,
}

impl Schema {
    pub fn get(&self, name: &str) -> Option<&TypeDef> {
        self.types.get(name)
    }

    pub fn object(&self, name: &str) -> Option<&ObjectDef> {
        match self.types.get(name) {
            Some(TypeDef::Object(o)) => Some(o),
            _ => None,
        }
    }

    /// Add a type; returns `false` (and keeps the existing one) if the name is taken.
    pub fn add(&mut self, ty: TypeDef) -> bool {
        let name = ty.name().to_owned();
        if self.types.contains_key(&name) {
            return false;
        }
        self.types.insert(name, ty);
        true
    }
}

#[derive(Debug, Clone)]
pub enum TypeDef {
    Scalar(ScalarDef),
    Object(ObjectDef),
    InputObject(InputObjectDef),
    Enum(EnumDef),
}

impl TypeDef {
    pub fn name(&self) -> &str {
        match self {
            TypeDef::Scalar(t) => &t.name,
            TypeDef::Object(t) => &t.name,
            TypeDef::InputObject(t) => &t.name,
            TypeDef::Enum(t) => &t.name,
        }
    }

    pub fn description(&self) -> Option<&str> {
        match self {
            TypeDef::Scalar(t) => t.description.as_deref(),
            TypeDef::Object(t) => t.description.as_deref(),
            TypeDef::InputObject(t) => t.description.as_deref(),
            TypeDef::Enum(t) => t.description.as_deref(),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            TypeDef::Scalar(_) => "SCALAR",
            TypeDef::Object(_) => "OBJECT",
            TypeDef::InputObject(_) => "INPUT_OBJECT",
            TypeDef::Enum(_) => "ENUM",
        }
    }

    pub fn is_input(&self) -> bool {
        matches!(
            self,
            TypeDef::Scalar(_) | TypeDef::InputObject(_) | TypeDef::Enum(_)
        )
    }

    pub fn is_leaf(&self) -> bool {
        matches!(self, TypeDef::Scalar(_) | TypeDef::Enum(_))
    }
}

#[derive(Debug, Clone)]
pub struct ScalarDef {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ObjectDef {
    pub name: String,
    pub description: Option<String>,
    pub fields: IndexMap<String, FieldDef>,
    pub interfaces: Vec<String>,
}

impl ObjectDef {
    pub fn new(name: impl Into<String>, description: Option<String>) -> Self {
        ObjectDef {
            name: name.into(),
            description,
            fields: IndexMap::new(),
            interfaces: vec![],
        }
    }

    pub fn field(mut self, field: FieldDef) -> Self {
        self.fields.insert(field.name.clone(), field);
        self
    }
}

#[derive(Debug, Clone)]
pub struct FieldDef {
    pub name: String,
    pub description: Option<String>,
    pub args: IndexMap<String, InputValueDef>,
    pub ty: TypeRef,
    pub kind: FieldKind,
    pub deprecation_reason: Option<String>,
}

impl FieldDef {
    pub fn new(name: impl Into<String>, ty: TypeRef, kind: FieldKind) -> Self {
        FieldDef {
            name: name.into(),
            description: None,
            args: IndexMap::new(),
            ty,
            kind,
            deprecation_reason: None,
        }
    }

    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn describe_opt(mut self, description: Option<String>) -> Self {
        self.description = description;
        self
    }

    pub fn arg(mut self, arg: InputValueDef) -> Self {
        self.args.insert(arg.name.clone(), arg);
        self
    }
}

/// What a field means, for the lowering step.
#[derive(Debug, Clone)]
pub enum FieldKind {
    /// A field of an introspection type (`__Type.name`, …), answered by the introspection
    /// executor.
    Introspection,
    /// `__schema` on the query root.
    SchemaField,
    /// `__type(name:)` on the query root.
    TypeField,
    /// A table column.
    Column {
        column: String,
        ty: ColumnType,
    },
    ObjectRelationship(Arc<RelationshipInfo>),
    ArrayRelationship(Arc<RelationshipInfo>),
    /// `author(where: ...)` on the query root.
    SelectMany(Arc<TableInfo>),
    /// `author_by_pk(id: ...)` on the query root.
    SelectByPk(Arc<TableInfo>),
    /// The placeholder field of an empty query root.
    NoQueriesAvailable,
}

#[derive(Debug, Clone)]
pub struct InputObjectDef {
    pub name: String,
    pub description: Option<String>,
    pub fields: IndexMap<String, InputValueDef>,
}

#[derive(Debug, Clone)]
pub struct InputValueDef {
    pub name: String,
    pub description: Option<String>,
    pub ty: TypeRef,
    /// Default value as GraphQL source text (as introspection returns it).
    pub default_value: Option<String>,
}

impl InputValueDef {
    pub fn new(name: impl Into<String>, ty: TypeRef) -> Self {
        InputValueDef {
            name: name.into(),
            description: None,
            ty,
            default_value: None,
        }
    }

    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn describe_opt(mut self, description: Option<String>) -> Self {
        self.description = description;
        self
    }
}

#[derive(Debug, Clone)]
pub struct EnumDef {
    pub name: String,
    pub description: Option<String>,
    pub values: IndexMap<String, EnumValueDef>,
}

#[derive(Debug, Clone)]
pub struct EnumValueDef {
    pub name: String,
    pub description: Option<String>,
    pub deprecation_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DirectiveDef {
    pub name: String,
    pub description: Option<String>,
    pub locations: Vec<&'static str>,
    pub args: IndexMap<String, InputValueDef>,
    pub is_repeatable: bool,
}

/// A reference to a type, with list / non-null wrappers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TypeRef {
    Named(String),
    List(Box<TypeRef>),
    NonNull(Box<TypeRef>),
}

impl TypeRef {
    pub fn named(name: impl Into<String>) -> Self {
        TypeRef::Named(name.into())
    }

    pub fn non_null(self) -> Self {
        match self {
            TypeRef::NonNull(_) => self,
            other => TypeRef::NonNull(Box::new(other)),
        }
    }

    pub fn list(self) -> Self {
        TypeRef::List(Box::new(self))
    }

    /// `name!`
    pub fn nn(name: impl Into<String>) -> Self {
        TypeRef::named(name).non_null()
    }

    /// `[name!]!`
    pub fn nn_list_nn(name: impl Into<String>) -> Self {
        TypeRef::nn(name).list().non_null()
    }

    /// The innermost named type.
    pub fn base(&self) -> &str {
        match self {
            TypeRef::Named(n) => n,
            TypeRef::List(t) | TypeRef::NonNull(t) => t.base(),
        }
    }

    pub fn is_non_null(&self) -> bool {
        matches!(self, TypeRef::NonNull(_))
    }

    /// Strip one level of non-null.
    pub fn nullable(&self) -> &TypeRef {
        match self {
            TypeRef::NonNull(t) => t,
            other => other,
        }
    }

    pub fn from_ast(ty: &async_graphql_parser::types::Type) -> Self {
        use async_graphql_parser::types::BaseType;
        let inner = match &ty.base {
            BaseType::Named(n) => TypeRef::Named(n.to_string()),
            BaseType::List(t) => TypeRef::List(Box::new(TypeRef::from_ast(t))),
        };
        if ty.nullable {
            inner
        } else {
            TypeRef::NonNull(Box::new(inner))
        }
    }
}

impl std::fmt::Display for TypeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeRef::Named(n) => f.write_str(n),
            TypeRef::List(t) => write!(f, "[{t}]"),
            TypeRef::NonNull(t) => write!(f, "{t}!"),
        }
    }
}
