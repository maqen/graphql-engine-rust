//! Remote schema customization: type and field renaming and root field namespaces
//! (spec 08 §4).

use std::collections::{BTreeSet, HashMap};

use indexmap::IndexMap;

use super::introspection::{BUILTIN_SCALARS, RemoteDocument};
use super::metadata::Customization;
use crate::model::*;

/// Renames from original remote names to the names exposed by Hasura. Only names that
/// change are present.
#[derive(Debug, Clone, Default)]
pub struct Customizer {
    pub namespace: Option<String>,
    /// Original type name → customized name.
    pub types: HashMap<String, String>,
    /// Original parent type → original field name → customized name.
    pub fields: HashMap<String, HashMap<String, String>>,
}

impl Customizer {
    pub fn type_name<'a>(&'a self, original: &'a str) -> &'a str {
        self.types.get(original).map_or(original, String::as_str)
    }

    pub fn field_name<'a>(&'a self, parent: &str, original: &'a str) -> &'a str {
        self.fields
            .get(parent)
            .and_then(|m| m.get(original))
            .map_or(original, String::as_str)
    }

    pub fn is_identity(&self) -> bool {
        self.types.is_empty() && self.fields.is_empty()
    }
}

fn affix(prefix: Option<&str>, suffix: Option<&str>, name: &str) -> Option<String> {
    if prefix.is_none() && suffix.is_none() {
        return None;
    }
    Some(format!(
        "{}{name}{}",
        prefix.unwrap_or_default(),
        suffix.unwrap_or_default()
    ))
}

/// `getCustomizer`.
pub fn build(doc: &RemoteDocument, customization: Option<&Customization>) -> Customizer {
    let Some(c) = customization else {
        return Customizer::default();
    };
    let roots: Vec<&str> = doc.roots().collect();
    let namespace = c.root_fields_namespace.clone();

    // Left-biased union: explicit mapping, then namespaced roots, then prefix/suffix.
    let mut types: HashMap<String, String> = HashMap::new();
    let insert = |types: &mut HashMap<String, String>, k: &str, v: String| {
        types.entry(k.to_owned()).or_insert(v);
    };
    if let Some(tn) = &c.type_names {
        for (k, v) in &tn.mapping {
            insert(&mut types, k, v.clone());
        }
    }
    if let Some(ns) = &namespace {
        for r in &roots {
            insert(&mut types, r, format!("{ns}{r}"));
        }
    }
    if let Some(tn) = &c.type_names {
        for name in doc.types.keys() {
            let protected = name.starts_with("__")
                || BUILTIN_SCALARS.contains(&name.as_str())
                || (namespace.is_none() && roots.contains(&name.as_str()));
            if protected {
                continue;
            }
            if let Some(new) = affix(tn.prefix.as_deref(), tn.suffix.as_deref(), name) {
                insert(&mut types, name, new);
            }
        }
    }
    types.retain(|k, v| k != v);

    let mut fields: HashMap<String, HashMap<String, String>> = HashMap::new();
    for f in c.field_names.iter().flatten() {
        let Some(type_fields) = doc.fields_of(&f.parent_type) else {
            continue;
        };
        let mut map: HashMap<String, String> = f.mapping.clone().into_iter().collect();
        for name in type_fields.keys() {
            if let Some(new) = affix(f.prefix.as_deref(), f.suffix.as_deref(), name) {
                map.entry(name.clone()).or_insert(new);
            }
        }
        map.retain(|k, v| k != v);
        // Later entries for the same type win.
        fields.insert(f.parent_type.clone(), map);
    }
    Customizer {
        namespace,
        types,
        fields,
    }
}

fn duplicates<'a>(names: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut seen = BTreeSet::new();
    let mut dups = BTreeSet::new();
    for n in names {
        if !seen.insert(n) {
            dups.insert(n);
        }
    }
    dups.into_iter().collect()
}

fn quoted_list(names: &[&str]) -> String {
    names
        .iter()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `validateSchemaCustomizations` (spec 08 §4.2).
pub fn validate(doc: &RemoteDocument, c: &Customizer) -> Result<(), String> {
    for ty in doc.types.values() {
        let TypeDef::Interface(iface) = ty else {
            continue;
        };
        for member in &iface.possible_types {
            for f in iface.fields.keys() {
                let i_name = c.field_name(&iface.name, f);
                let t_name = c.field_name(member, f);
                if i_name != t_name {
                    return Err(format!(
                        "Remote schema customization inconsistency: field name mapping for field \"{f}\" of interface \"{}\" is inconsistent with mapping for type \"{member}\". Interface field name maps to \"{i_name}\". Type field name maps to \"{t_name}\".",
                        iface.name
                    ));
                }
            }
        }
    }
    let dups = duplicates(
        doc.types
            .keys()
            .filter(|n| !n.starts_with("__"))
            .map(|n| c.type_name(n)),
    );
    if !dups.is_empty() {
        return Err(format!(
            "Type name mappings are not distinct; the following types appear more than once: {}",
            quoted_list(&dups)
        ));
    }
    for ty in doc.types.values() {
        let (kind, fields) = match ty {
            TypeDef::Object(o) => ("object", &o.fields),
            TypeDef::Interface(i) => ("interface", &i.fields),
            _ => continue,
        };
        let dups = duplicates(fields.keys().map(|f| c.field_name(ty.name(), f)));
        if !dups.is_empty() {
            return Err(format!(
                "Field name mappings for {kind} type \"{}\" are not distinct; the following fields appear more than once: {}",
                ty.name(),
                quoted_list(&dups)
            ));
        }
    }
    Ok(())
}

fn rename_ref(c: &Customizer, ty: &TypeRef) -> TypeRef {
    match ty {
        TypeRef::Named(n) => TypeRef::Named(c.type_name(n).to_owned()),
        TypeRef::List(t) => TypeRef::List(Box::new(rename_ref(c, t))),
        TypeRef::NonNull(t) => TypeRef::NonNull(Box::new(rename_ref(c, t))),
    }
}

fn rename_input(c: &Customizer, v: &InputValueDef) -> InputValueDef {
    InputValueDef {
        ty: rename_ref(c, &v.ty),
        ..v.clone()
    }
}

fn rename_fields(
    c: &Customizer,
    parent: &str,
    fields: &IndexMap<String, FieldDef>,
) -> IndexMap<String, FieldDef> {
    fields
        .iter()
        .map(|(orig, f)| {
            let name = c.field_name(parent, orig).to_owned();
            let def = FieldDef {
                name: name.clone(),
                ty: rename_ref(c, &f.ty),
                args: f
                    .args
                    .iter()
                    .map(|(k, a)| (k.clone(), rename_input(c, a)))
                    .collect(),
                ..f.clone()
            };
            (name, def)
        })
        .collect()
}

/// The document with customized names. Fields keep their original names in their
/// [`FieldKind`].
pub fn apply(doc: &RemoteDocument, c: &Customizer) -> RemoteDocument {
    if c.is_identity() {
        return doc.clone();
    }
    let names = |list: &[String]| -> Vec<String> {
        list.iter().map(|n| c.type_name(n).to_owned()).collect()
    };
    let types = doc
        .types
        .values()
        .map(|ty| {
            let name = c.type_name(ty.name()).to_owned();
            let new = match ty {
                TypeDef::Scalar(s) => TypeDef::Scalar(ScalarDef {
                    name: name.clone(),
                    ..s.clone()
                }),
                TypeDef::Enum(e) => TypeDef::Enum(EnumDef {
                    name: name.clone(),
                    ..e.clone()
                }),
                TypeDef::InputObject(io) => TypeDef::InputObject(InputObjectDef {
                    name: name.clone(),
                    description: io.description.clone(),
                    fields: io
                        .fields
                        .iter()
                        .map(|(k, f)| (k.clone(), rename_input(c, f)))
                        .collect(),
                }),
                TypeDef::Object(o) => TypeDef::Object(ObjectDef {
                    name: name.clone(),
                    description: o.description.clone(),
                    fields: rename_fields(c, &o.name, &o.fields),
                    interfaces: names(&o.interfaces),
                }),
                TypeDef::Interface(i) => TypeDef::Interface(InterfaceDef {
                    name: name.clone(),
                    description: i.description.clone(),
                    fields: rename_fields(c, &i.name, &i.fields),
                    possible_types: names(&i.possible_types),
                }),
                TypeDef::Union(u) => TypeDef::Union(UnionDef {
                    name: name.clone(),
                    description: u.description.clone(),
                    possible_types: names(&u.possible_types),
                }),
            };
            (name, new)
        })
        .collect();
    RemoteDocument {
        query_root: c.type_name(&doc.query_root).to_owned(),
        mutation_root: doc
            .mutation_root
            .as_deref()
            .map(|r| c.type_name(r).to_owned()),
        subscription_root: doc
            .subscription_root
            .as_deref()
            .map(|r| c.type_name(r).to_owned()),
        types,
    }
}
