//! The YAML comparison and "human" report the official CLI prints for `metadata diff`
//! (homeport/dyff v1.12 with `IgnoreOrderChanges`, `OmitHeader` and `Indent: 2`, rendered
//! through gonvenience/neat and ytbx). Output is uncoloured, as dyff prints it when stdout
//! is not a terminal.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_yaml::Value as Yaml;

const ADDITION: char = '+';
const REMOVAL: char = '-';
const MODIFICATION: char = '±';
const INDENT: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    // go-yaml's `yaml.Kind` values, which `sortNode` orders by.
    Document = 1,
    Sequence = 2,
    Mapping = 4,
    Scalar = 8,
}

/// A go-yaml v3 `yaml.Node`, reduced to what dyff looks at.
#[derive(Clone, Debug)]
pub struct Node {
    pub kind: Kind,
    pub tag: String,
    pub value: String,
    /// Sequence items, or alternating keys and values of a mapping.
    pub content: Vec<Node>,
}

impl Node {
    fn scalar(tag: &str, value: impl Into<String>) -> Node {
        Node {
            kind: Kind::Scalar,
            tag: tag.into(),
            value: value.into(),
            content: Vec::new(),
        }
    }

    fn collection(kind: Kind, content: Vec<Node>) -> Node {
        let tag = match kind {
            Kind::Sequence => "!!seq",
            Kind::Mapping => "!!map",
            _ => "",
        };
        Node {
            kind,
            tag: tag.into(),
            value: String::new(),
            content,
        }
    }

    fn from_yaml(v: &Yaml) -> Node {
        match v {
            Yaml::Null => Node::scalar("!!null", "null"),
            Yaml::Bool(b) => Node::scalar("!!bool", b.to_string()),
            Yaml::Number(n) => {
                let tag = if n.is_f64() { "!!float" } else { "!!int" };
                Node::scalar(tag, n.to_string())
            }
            Yaml::String(s) => Node::scalar("!!str", s.clone()),
            Yaml::Sequence(items) => {
                Node::collection(Kind::Sequence, items.iter().map(Node::from_yaml).collect())
            }
            Yaml::Mapping(m) => Node::collection(
                Kind::Mapping,
                m.iter()
                    .flat_map(|(k, v)| [Node::from_yaml(k), Node::from_yaml(v)])
                    .collect(),
            ),
            Yaml::Tagged(t) => {
                let mut n = Node::from_yaml(&t.value);
                n.tag = t.tag.to_string();
                n
            }
        }
    }

    fn pairs(&self) -> impl Iterator<Item = (&Node, &Node)> {
        self.content
            .chunks(2)
            .filter(|c| c.len() == 2)
            .map(|c| (&c[0], &c[1]))
    }

    fn find_value(&self, key: &str) -> Option<&Node> {
        self.pairs().find(|(k, _)| k.value == key).map(|(_, v)| v)
    }

    fn find_value_mut(&mut self, key: &str) -> Option<&mut Node> {
        let idx = self
            .content
            .chunks(2)
            .position(|c| c.len() == 2 && c[0].value == key)?;
        self.content.get_mut(idx * 2 + 1)
    }
}

/// `ytbx.LoadDocuments`: an empty input is one `null` document, input starting with `{` or
/// `[` is read as JSON, a comment-only file is an empty TOML table, anything else is YAML.
pub fn load_documents(text: &str) -> Result<Vec<Node>, String> {
    let doc = |root: Node| Node::collection(Kind::Document, vec![root]);
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "---" {
        return Ok(vec![doc(Node::scalar("!!null", ""))]);
    }
    if trimmed
        .lines()
        .all(|l| l.trim().is_empty() || l.trim_start().starts_with('#'))
    {
        return Ok(vec![doc(Node::collection(Kind::Mapping, Vec::new()))]);
    }
    if text.starts_with(['{', '[']) {
        let mut docs = Vec::new();
        for v in serde_json::Deserializer::from_str(text).into_iter::<serde_json::Value>() {
            let v = v.map_err(|e| e.to_string())?;
            let y: Yaml = serde_yaml::to_value(&v).map_err(|e| e.to_string())?;
            docs.push(doc(Node::from_yaml(&y)));
        }
        return Ok(docs);
    }
    let mut docs = Vec::new();
    for de in serde_yaml::Deserializer::from_str(text) {
        let v: Yaml = serde::Deserialize::deserialize(de).map_err(|e| format!("yaml: {e}"))?;
        docs.push(doc(Node::from_yaml(&v)));
    }
    Ok(docs)
}

/// The document `createEmptyYamlFileAccordingToContent` builds: `[]` when the first document
/// is a list, `{}` otherwise.
pub fn empty_like(docs: &[Node]) -> Vec<Node> {
    let is_seq = docs
        .first()
        .and_then(|d| d.content.first())
        .is_some_and(|n| n.kind == Kind::Sequence);
    let root = if is_seq {
        Kind::Sequence
    } else {
        Kind::Mapping
    };
    vec![Node::collection(
        Kind::Document,
        vec![Node::collection(root, Vec::new())],
    )]
}

// --- comparison

#[derive(Clone, Debug)]
enum PathElement {
    Named(String),
    NamedListEntry(String),
    Index(usize),
}

#[derive(Clone, Debug)]
struct Detail {
    kind: char,
    from: Option<Node>,
    to: Option<Node>,
}

#[derive(Clone, Debug)]
struct Diff {
    path: Vec<PathElement>,
    details: Vec<Detail>,
}

fn with(path: &[PathElement], e: PathElement) -> Vec<PathElement> {
    let mut p = path.to_vec();
    p.push(e);
    p
}

fn modification(path: &[PathElement], from: &Node, to: &Node) -> Vec<Diff> {
    vec![Diff {
        path: path.to_vec(),
        details: vec![Detail {
            kind: MODIFICATION,
            from: Some(from.clone()),
            to: Some(to.clone()),
        }],
    }]
}

/// `dyff.CompareInputFiles` with `IgnoreOrderChanges(true)`.
fn compare_documents(from: &mut [Node], to: &mut [Node]) -> Result<Vec<Diff>, String> {
    let is_empty_doc = |d: &Node| {
        d.content
            .first()
            .is_some_and(|n| n.kind == Kind::Scalar && n.tag == "!!null")
    };
    // Kubernetes entity detection: when every non-empty document is a named resource. Hasura
    // metadata files never are, so this only matters when both sides are empty.
    let from_docs = from.iter().filter(|d| !is_empty_doc(d)).count();
    let to_docs = to.iter().filter(|d| !is_empty_doc(d)).count();
    let k8s = |docs: &[Node]| {
        docs.iter()
            .filter(|d| !is_empty_doc(d))
            .all(|d| d.content.first().is_some_and(k8s_name_ok))
    };
    if k8s(from) && k8s(to) && (from_docs > 0 || to_docs > 0) {
        return Err("comparing Kubernetes resources is not supported".into());
    }
    if from_docs == 0 && to_docs == 0 {
        return Ok(Vec::new());
    }
    if from.len() != to.len() {
        return Err(
            "comparing YAMLs with a different number of documents is currently not supported"
                .into(),
        );
    }
    let mut result = Vec::new();
    for (f, t) in from.iter_mut().zip(to.iter_mut()) {
        result.extend(objects(&[], f, t));
    }
    Ok(result)
}

fn k8s_name_ok(n: &Node) -> bool {
    n.kind == Kind::Mapping
        && n.find_value("apiVersion").is_some()
        && n.find_value("kind").is_some()
        && n.find_value("metadata")
            .and_then(|m| m.find_value("name"))
            .is_some()
}

fn objects(path: &[PathElement], from: &mut Node, to: &mut Node) -> Vec<Diff> {
    if from.kind != to.kind || from.tag != to.tag {
        return modification(path, from, to);
    }
    match from.kind {
        Kind::Document => match (from.content.first_mut(), to.content.first_mut()) {
            (Some(f), Some(t)) => objects(path, f, t),
            _ => Vec::new(),
        },
        Kind::Mapping => mapping_nodes(path, from, to),
        Kind::Sequence => sequence_nodes(path, from, to),
        Kind::Scalar => match from.tag.as_str() {
            "!!null" => Vec::new(),
            "!!bool" => {
                if to_bool(&from.value) != to_bool(&to.value) {
                    modification(path, from, to)
                } else {
                    Vec::new()
                }
            }
            _ if from.value != to.value => modification(path, from, to),
            _ => Vec::new(),
        },
    }
}

fn to_bool(s: &str) -> Option<bool> {
    match s {
        "y" | "Y" | "yes" | "Yes" | "YES" | "true" | "True" | "TRUE" | "on" | "On" | "ON" => {
            Some(true)
        }
        "n" | "N" | "no" | "No" | "NO" | "false" | "False" | "FALSE" | "off" | "Off" | "OFF" => {
            Some(false)
        }
        _ => None,
    }
}

fn mapping_nodes(path: &[PathElement], from: &mut Node, to: &mut Node) -> Vec<Diff> {
    let mut result = Vec::new();
    let mut removals = Vec::new();
    let mut additions = Vec::new();
    for i in (0..from.content.len().saturating_sub(1)).step_by(2) {
        let key = from.content[i].value.clone();
        match to.find_value_mut(&key) {
            Some(t) => {
                let f = &mut from.content[i + 1];
                result.extend(objects(&with(path, PathElement::Named(key)), f, t));
            }
            None => {
                removals.push(from.content[i].clone());
                removals.push(from.content[i + 1].clone());
            }
        }
    }
    for (k, v) in to.pairs() {
        if from.find_value(&k.value).is_none() {
            additions.push(k.clone());
            additions.push(v.clone());
        }
    }
    let mut details = Vec::new();
    if !removals.is_empty() {
        details.push(Detail {
            kind: REMOVAL,
            from: Some(Node::collection(Kind::Mapping, removals)),
            to: None,
        });
    }
    if !additions.is_empty() {
        details.push(Detail {
            kind: ADDITION,
            from: None,
            to: Some(Node::collection(Kind::Mapping, additions)),
        });
    }
    if !details.is_empty() {
        result.insert(
            0,
            Diff {
                path: path.to_vec(),
                details,
            },
        );
    }
    result
}

fn sequence_nodes(path: &[PathElement], from: &mut Node, to: &mut Node) -> Vec<Diff> {
    if from.content.is_empty() && to.content.is_empty() {
        return Vec::new();
    }
    if let Some(id) =
        identifier_from_named_lists(from, to).or_else(|| non_standard_identifier(from, to))
    {
        return named_entry_lists(path, &id, from, to);
    }
    simple_lists(path, from, to)
}

fn identifier_from_named_lists(a: &Node, b: &Node) -> Option<String> {
    const CANDIDATES: [&str; 4] = ["name", "key", "id", "manager"];
    fn count(seq: &Node) -> HashMap<&str, HashSet<&str>> {
        let mut m: HashMap<&str, HashSet<&str>> = HashMap::new();
        for entry in seq.content.iter().filter(|e| e.kind == Kind::Mapping) {
            for (k, v) in entry.pairs() {
                if k.kind == Kind::Scalar && CANDIDATES.contains(&k.value.as_str()) {
                    m.entry(k.value.as_str())
                        .or_default()
                        .insert(v.value.as_str());
                }
            }
        }
        m
    }
    let (ca, cb) = (count(a), count(b));
    CANDIDATES
        .iter()
        .find(|id| {
            ca.get(**id).is_some_and(|s| s.len() == a.content.len())
                && cb.get(**id).is_some_and(|s| s.len() == b.content.len())
        })
        .map(|s| s.to_string())
}

fn non_standard_identifier(a: &Node, b: &Node) -> Option<String> {
    fn count(seq: &Node) -> BTreeMap<&str, usize> {
        let mut m: BTreeMap<&str, HashSet<&str>> = BTreeMap::new();
        for entry in &seq.content {
            if entry.kind != Kind::Mapping {
                return BTreeMap::new();
            }
            for (k, v) in entry.pairs() {
                if k.kind == Kind::Scalar
                    && k.tag == "!!str"
                    && v.kind == Kind::Scalar
                    && v.tag == "!!str"
                {
                    m.entry(k.value.as_str())
                        .or_default()
                        .insert(v.value.as_str());
                }
            }
        }
        m.into_iter()
            .map(|(k, v)| (k, v.len()))
            .collect::<BTreeMap<_, _>>()
    }
    let (ca, cb) = (count(a), count(b));
    ca.iter()
        .find(|(k, n)| **n == a.content.len() && cb.get(*k) == Some(&b.content.len()) && **n > 3)
        .map(|(k, _)| k.to_string())
}

fn entry_name(entry: &Node, id: &str) -> Option<String> {
    entry
        .pairs()
        .find(|(k, _)| k.value == id)
        .map(|(_, v)| v.value.clone())
}

fn named_entry_lists(path: &[PathElement], id: &str, from: &mut Node, to: &mut Node) -> Vec<Diff> {
    let mut result = Vec::new();
    let mut removals = Vec::new();
    let mut additions = Vec::new();
    let to_names: Vec<Option<String>> = to.content.iter().map(|e| entry_name(e, id)).collect();
    for f in from.content.iter_mut() {
        let name = entry_name(f, id).unwrap_or_default();
        match to_names
            .iter()
            .position(|n| n.as_deref() == Some(name.as_str()))
        {
            Some(idx) => {
                let p = with(path, PathElement::NamedListEntry(name));
                result.extend(objects(&p, f, &mut to.content[idx]));
            }
            None => removals.push(f.clone()),
        }
    }
    let from_names: Vec<Option<String>> = from.content.iter().map(|e| entry_name(e, id)).collect();
    for (t, name) in to.content.iter().zip(&to_names) {
        if !from_names.contains(name) {
            additions.push(t.clone());
        }
    }
    pack(result, path, additions, removals)
}

fn pack(
    mut list: Vec<Diff>,
    path: &[PathElement],
    additions: Vec<Node>,
    removals: Vec<Node>,
) -> Vec<Diff> {
    let mut details = Vec::new();
    if !removals.is_empty() {
        details.push(Detail {
            kind: REMOVAL,
            from: Some(Node::collection(Kind::Sequence, removals)),
            to: None,
        });
    }
    if !additions.is_empty() {
        details.push(Detail {
            kind: ADDITION,
            from: None,
            to: Some(Node::collection(Kind::Sequence, additions)),
        });
    }
    if !details.is_empty() {
        list.insert(
            0,
            Diff {
                path: path.to_vec(),
                details,
            },
        );
    }
    list
}

fn simple_lists(path: &[PathElement], from: &mut Node, to: &mut Node) -> Vec<Diff> {
    if from.content.len() == 1 && to.content.len() == 1 {
        let p = with(path, PathElement::Index(0));
        return objects(&p, &mut from.content[0], &mut to.content[0]);
    }
    // Hashing sorts nested lists in place, as dyff's `basicType` does.
    let from_hashes: Vec<String> = from.content.iter_mut().map(node_hash).collect();
    let to_hashes: Vec<String> = to.content.iter_mut().map(node_hash).collect();
    let lookup = |hs: &[String]| {
        let mut m: HashMap<String, usize> = HashMap::new();
        for h in hs {
            *m.entry(h.clone()).or_default() += 1;
        }
        m
    };
    let (fl, tl) = (lookup(&from_hashes), lookup(&to_hashes));
    let mut removals: Vec<(String, Node)> = Vec::new();
    for (h, n) in from_hashes.iter().zip(&from.content) {
        match tl.get(h) {
            None => removals.push((h.clone(), n.clone())),
            Some(tc) if fl[h] > *tc && !removals.iter().any(|(rh, _)| rh == h) => {
                for _ in 0..fl[h] - tc {
                    removals.push((h.clone(), n.clone()));
                }
            }
            _ => {}
        }
    }
    let mut additions: Vec<(String, Node)> = Vec::new();
    for (h, n) in to_hashes.iter().zip(&to.content) {
        match fl.get(h) {
            None => additions.push((h.clone(), n.clone())),
            Some(fc) if tl[h] > *fc && !additions.iter().any(|(ah, _)| ah == h) => {
                for _ in 0..tl[h] - fc {
                    additions.push((h.clone(), n.clone()));
                }
            }
            _ => {}
        }
    }
    pack(
        Vec::new(),
        path,
        additions.into_iter().map(|(_, n)| n).collect(),
        removals.into_iter().map(|(_, n)| n).collect(),
    )
}

/// An identity for `calcNodeHash`: scalars by tag and value, collections by their plain
/// value (`basicType`), with maps unordered and lists sorted first.
fn node_hash(n: &mut Node) -> String {
    match n.kind {
        Kind::Scalar => format!("s{}/{}", n.tag, n.value),
        _ => basic_type(n),
    }
}

fn basic_type(n: &mut Node) -> String {
    match n.kind {
        Kind::Mapping => {
            let mut entries: Vec<String> = Vec::new();
            let len = n.content.len() / 2 * 2;
            let mut i = 0;
            while i < len {
                let k = basic_type(&mut n.content[i]);
                let v = basic_type(&mut n.content[i + 1]);
                entries.push(format!("{k}:{v}"));
                i += 2;
            }
            entries.sort();
            entries.dedup();
            format!("{{{}}}", entries.join(","))
        }
        Kind::Sequence => {
            sort_node(n);
            let items: Vec<String> = n.content.iter_mut().map(basic_type).collect();
            format!("[{}]", items.join(","))
        }
        Kind::Scalar => quote_str(&n.value),
        Kind::Document => match n.content.first_mut() {
            Some(c) => basic_type(c),
            None => String::new(),
        },
    }
}

fn quote_str(s: &str) -> String {
    format!("{s:?}")
}

fn sort_node(n: &mut Node) {
    n.content.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.tag.cmp(&b.tag))
            .then_with(|| {
                if a.kind == Kind::Scalar {
                    a.value.cmp(&b.value)
                } else {
                    a.content.len().cmp(&b.content.len())
                }
            })
    });
}

// --- report

/// `diff.YamlDiff`: the file name and dyff's report when the files differ.
pub fn yaml_diff(from: &mut [Node], to: &mut [Node], file_name: &str) -> Result<String, String> {
    let diffs =
        compare_documents(from, to).map_err(|e| format!("error while getting diff: {e}"))?;
    if diffs.is_empty() {
        return Ok(String::new());
    }
    let mut out = format!("{file_name}\n");
    for diff in &diffs {
        out.push('\n');
        out.push_str(&path_to_string(&diff.path));
        out.push('\n');
        let blocks: Vec<String> = diff.details.iter().map(detail_output).collect();
        let indent = if diff.path.is_empty() { 0 } else { INDENT };
        write_text_blocks(&mut out, indent, &blocks);
    }
    out.push('\n');
    Ok(out)
}

fn path_to_string(path: &[PathElement]) -> String {
    if path.is_empty() {
        return "(root level)".into();
    }
    path.iter()
        .filter_map(|e| match e {
            PathElement::Named(n) if !n.is_empty() => Some(n.clone()),
            PathElement::Named(_) => None,
            PathElement::NamedListEntry(n) => Some(n.clone()),
            PathElement::Index(i) => Some(i.to_string()),
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn plural(amount: usize, one: &str, many: &str) -> String {
    const WORDS: [&str; 13] = [
        "no", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
        "eleven", "twelve",
    ];
    let number = WORDS
        .get(amount)
        .map(|s| s.to_string())
        .unwrap_or_else(|| amount.to_string());
    format!("{number} {}", if amount == 1 { one } else { many })
}

fn detail_output(d: &Detail) -> String {
    match (d.kind, &d.from, &d.to) {
        (ADDITION, _, Some(to)) => added_or_removed(ADDITION, "added", to),
        (REMOVAL, Some(from), _) => added_or_removed(REMOVAL, "removed", from),
        (_, Some(from), Some(to)) => modification_output(from, to),
        _ => String::new(),
    }
}

fn added_or_removed(sign: char, verb: &str, node: &Node) -> String {
    let mut out = String::new();
    let what = match node.kind {
        Kind::Document => Some(plural(node.content.len(), "document", "documents")),
        Kind::Sequence => Some(plural(node.content.len(), "list entry", "list entries")),
        Kind::Mapping => Some(plural(node.content.len() / 2, "map entry", "map entries")),
        Kind::Scalar => None,
    };
    if let Some(what) = what {
        out.push_str(&format!("{sign} {what} {verb}:\n"));
    }
    let mut node = node.clone();
    restructure(&mut node);
    let yaml = neat(&node);
    write_text_blocks(&mut out, INDENT, &[yaml]);
    out
}

fn human_type(n: &Node) -> String {
    match n.kind {
        Kind::Document => "document".into(),
        Kind::Mapping => "map".into(),
        Kind::Sequence => "list".into(),
        Kind::Scalar => match n.tag.as_str() {
            "!!str" => "string".into(),
            "!!null" => "<nil>".into(),
            t => t.get(2..).unwrap_or_default().to_string(),
        },
    }
}

fn modification_output(from: &Node, to: &Node) -> String {
    let (ft, tt) = (human_type(from), human_type(to));
    let mut out = String::new();
    if ft == "string" && tt == "string" {
        string_diff(&mut out, &from.value, &to.value);
        return out;
    }
    if ft != tt {
        out.push_str(&format!("{MODIFICATION} type change from {ft} to {tt}\n"));
    } else {
        out.push_str(&format!("{MODIFICATION} value change\n"));
    }
    out.push_str(&prefixed(
        "- ",
        yaml_string(from).trim_end_matches('\n'),
        INDENT,
    ));
    out.push_str(&prefixed(
        "+ ",
        yaml_string(to).trim_end_matches('\n'),
        INDENT,
    ));
    out
}

fn yaml_string(n: &Node) -> String {
    if n.tag == "!!null" {
        "<nil>".into()
    } else {
        neat(n)
    }
}

fn string_diff(out: &mut String, from: &str, to: &str) {
    if from.trim_matches([' ', '\n']) == to.trim_matches([' ', '\n']) {
        let show = |s: &str| s.replace('\n', "↵\n").replace(' ', "·");
        out.push_str(&format!("{MODIFICATION} whitespace only change\n"));
        let blocks = [
            prefixed("- ", &show(from), INDENT),
            prefixed("+ ", &show(to), INDENT),
        ];
        write_text_blocks(out, 0, &blocks);
    } else if from.contains('\n') || to.contains('\n') {
        let mut buf = String::new();
        let (mut ins, mut del) = (0, 0);
        for (kind, text) in line_diff(from, to) {
            match kind {
                '+' => {
                    buf.push_str(&continuous_prefixed("+ ", &text, INDENT));
                    ins += 1;
                }
                '-' => {
                    buf.push_str(&continuous_prefixed("- ", &text, INDENT));
                    del += 1;
                }
                _ => {}
            }
        }
        out.push_str(&format!(
            "{MODIFICATION} value change in multiline text ({}, {})\n",
            plural(ins, "insert", "inserts"),
            plural(del, "deletion", "deletions")
        ));
        out.push_str(&buf);
        out.push('\n');
    } else {
        // Minor changes are highlighted by colour only, so without colours they read the same.
        out.push_str(&format!("{MODIFICATION} value change\n"));
        out.push_str(&prefixed("- ", from, INDENT));
        out.push_str(&prefixed("+ ", to, INDENT));
    }
}

/// A line diff like diff-match-patch's line mode: runs of deleted lines, then inserted lines,
/// between unchanged runs.
fn line_diff(from: &str, to: &str) -> Vec<(char, String)> {
    let a: Vec<&str> = from.split_inclusive('\n').collect();
    let b: Vec<&str> = to.split_inclusive('\n').collect();
    let ops = crate::metadata::diff::myers_ops(&a, &b);
    let mut out: Vec<(char, String)> = Vec::new();
    let (mut del, mut ins) = (String::new(), String::new());
    let flush = |out: &mut Vec<(char, String)>, del: &mut String, ins: &mut String| {
        if !del.is_empty() {
            out.push(('-', std::mem::take(del)));
        }
        if !ins.is_empty() {
            out.push(('+', std::mem::take(ins)));
        }
    };
    for (op, line) in ops {
        match op {
            '-' => del.push_str(line),
            '+' => ins.push_str(line),
            _ => {
                flush(&mut out, &mut del, &mut ins);
                out.push(('=', line.to_string()));
            }
        }
    }
    flush(&mut out, &mut del, &mut ins);
    out
}

fn continuous_prefixed(prefix: &str, text: &str, indent: usize) -> String {
    let trimmed = text.strip_suffix('\n').unwrap_or(text);
    trimmed
        .split('\n')
        .map(|l| format!("{}{prefix}{l}\n", " ".repeat(indent)))
        .collect()
}

fn prefixed(prefix: &str, text: &str, indent: usize) -> String {
    let mut out = String::new();
    for (i, line) in text.split('\n').enumerate() {
        if i == 0 {
            out.push_str(&" ".repeat(indent));
            out.push_str(prefix);
        } else {
            out.push_str(&" ".repeat(prefix.chars().count() + indent));
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn terminal_width() -> usize {
    if crate::cmd::is_terminal()
        && let Some(w) = std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok())
    {
        return w;
    }
    80
}

fn write_text_blocks(out: &mut String, indent: usize, blocks: &[String]) {
    const SEPARATOR: &str = "   ";
    let mut max_len = indent + blocks.len().saturating_sub(1) * SEPARATOR.len();
    for b in blocks {
        max_len += b.split('\n').map(|l| l.chars().count()).max().unwrap_or(0);
    }
    if max_len > terminal_width() {
        for b in blocks {
            for line in b.split('\n') {
                out.push_str(&" ".repeat(indent));
                out.push_str(line);
                out.push('\n');
            }
        }
    } else {
        out.push_str(&table_style(SEPARATOR, indent, blocks));
    }
}

fn table_style(separator: &str, indent: usize, columns: &[String]) -> String {
    let split: Vec<Vec<&str>> = columns.iter().map(|c| c.split('\n').collect()).collect();
    let rows = split.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = split
        .iter()
        .map(|lines| lines.iter().map(|l| l.chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = Vec::with_capacity(rows);
    for r in 0..rows {
        let cells: Vec<String> = split
            .iter()
            .zip(&widths)
            .map(|(lines, w)| match lines.get(r) {
                Some(l) => format!(
                    "{}{l}{}",
                    " ".repeat(indent),
                    " ".repeat(w - l.chars().count())
                ),
                None => " ".repeat(w + indent),
            })
            .collect();
        out.push(cells.join(separator).trim_end_matches(' ').to_string());
    }
    out.join("\n")
}

// --- ytbx.RestructureObject

const KNOWN_KEY_ORDERS: &[&[&str]] = &[
    &[
        "name",
        "director_uuid",
        "releases",
        "instance_groups",
        "networks",
        "resource_pools",
        "compilation",
    ],
    &["name", "url", "version", "sha1"],
    &["jobs", "resources", "resource_types"],
    &["name", "type", "source"],
    &["get"],
    &["put"],
    &["task"],
    &["releases", "instance_groups", "configuration", "variables"],
    &["auth", "templates"],
    &["apiVersion", "kind", "metadata", "spec", "status"],
    &["name"],
    &["key"],
    &["id"],
];

fn restructure(n: &mut Node) {
    match n.kind {
        Kind::Document => {
            if let Some(c) = n.content.first_mut() {
                restructure(c);
            }
        }
        Kind::Mapping => {
            let keys: Vec<String> = n.pairs().map(|(k, _)| k.value.clone()).collect();
            let mut best: Option<(usize, usize)> = None;
            for (idx, candidate) in KNOWN_KEY_ORDERS.iter().enumerate() {
                let count = keys
                    .iter()
                    .filter(|k| candidate.contains(&k.as_str()))
                    .count();
                if count > 0 && best.is_none_or(|(_, hits)| count > hits) {
                    best = Some((idx, count));
                }
            }
            if let Some((idx, _)) = best {
                let ordered: Vec<String> = KNOWN_KEY_ORDERS[idx]
                    .iter()
                    .filter(|k| keys.iter().any(|x| x == *k))
                    .map(|k| k.to_string())
                    .collect();
                reorder(n, &ordered);
            }
            for i in (1..n.content.len()).step_by(2) {
                restructure(&mut n.content[i]);
            }
        }
        Kind::Sequence => n.content.iter_mut().for_each(restructure),
        Kind::Scalar => {}
    }
}

fn reorder(n: &mut Node, keys: &[String]) {
    let mut remaining: Vec<(String, usize)> = n
        .pairs()
        .filter(|(k, _)| !keys.contains(&k.value))
        .map(|(k, v)| (k.value.clone(), max_depth(v, 0)))
        .collect();
    remaining.sort_by_key(|(_, d)| *d);
    let mut content = Vec::with_capacity(n.content.len());
    for key in keys.iter().chain(remaining.iter().map(|(k, _)| k)) {
        // `lookupMapOfContentList` keeps the last index of a duplicated key.
        if let Some(i) = (0..n.content.len())
            .step_by(2)
            .rfind(|i| n.content[*i].value == *key)
        {
            content.push(n.content[i].clone());
            content.push(n.content[i + 1].clone());
        }
    }
    n.content = content;
}

/// The deepest leaf path length below `n` (`ytbx`'s `maxDepth`).
fn max_depth(n: &Node, depth: usize) -> usize {
    match n.kind {
        Kind::Document => n.content.first().map_or(0, |c| max_depth(c, depth)),
        Kind::Sequence => match named_list_identifier(n) {
            Some(id) => n
                .content
                .iter()
                .flat_map(|e| {
                    e.pairs()
                        .filter(|(k, _)| k.value != id)
                        .map(|(_, v)| max_depth(v, depth + 2))
                        .collect::<Vec<_>>()
                })
                .max()
                .unwrap_or(0),
            None => n
                .content
                .iter()
                .map(|e| max_depth(e, depth + 1))
                .max()
                .unwrap_or(0),
        },
        Kind::Mapping => n
            .pairs()
            .map(|(_, v)| max_depth(v, depth + 1))
            .max()
            .unwrap_or(0),
        Kind::Scalar => depth,
    }
}

fn named_list_identifier(n: &Node) -> Option<&'static str> {
    let mut counters: HashMap<&str, usize> = HashMap::new();
    for e in &n.content {
        for (k, _) in e.pairs() {
            *counters.entry(k.value.as_str()).or_default() += 1;
        }
    }
    ["name", "key", "id"]
        .into_iter()
        .find(|id| counters.get(id) == Some(&n.content.len()))
}

// --- gonvenience/neat YAML output

fn neat(n: &Node) -> String {
    let mut out = String::new();
    neat_node(&mut out, "", false, n);
    out
}

fn needs_quotes(n: &Node) -> bool {
    if n.tag != "!!str" {
        return false;
    }
    let v = n.value.as_str();
    if matches!(
        v,
        "true" | "false" | "null" | ".nan" | ".inf" | "-.inf" | "+.inf"
    ) || v.starts_with('-')
    {
        return true;
    }
    let body = v.strip_prefix(['-', '+']).unwrap_or(v);
    if !body.is_empty()
        && body
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | '+'))
    {
        return true;
    }
    v.contains([' ', '*', '&', ':', ','])
}

fn neat_node(out: &mut String, prefix: &str, skip_first: bool, n: &Node) {
    const ADD: &str = "  ";
    match n.kind {
        Kind::Document => {
            for c in &n.content {
                neat_node(out, prefix, false, c);
            }
        }
        Kind::Sequence => {
            for (i, entry) in n.content.iter().enumerate() {
                if i > 0 || !skip_first {
                    out.push_str(prefix);
                }
                out.push_str("- ");
                neat_node(out, &format!("{prefix}{ADD}"), true, entry);
            }
        }
        Kind::Mapping => {
            for (i, (key, value)) in n.pairs().enumerate() {
                if !skip_first || i > 0 {
                    out.push_str(prefix);
                }
                out.push_str(&key.value);
                out.push(':');
                match value.kind {
                    Kind::Mapping if value.content.is_empty() => out.push_str(" {}\n"),
                    Kind::Mapping => {
                        out.push('\n');
                        neat_node(out, &format!("{prefix}{ADD}"), false, value);
                    }
                    Kind::Sequence if value.content.is_empty() => out.push_str(" []\n"),
                    Kind::Sequence => {
                        out.push('\n');
                        neat_node(out, prefix, false, value);
                    }
                    Kind::Scalar => {
                        out.push(' ');
                        neat_node(out, &format!("{prefix}{ADD}"), false, value);
                    }
                    Kind::Document => {}
                }
            }
        }
        Kind::Scalar => {
            let lines: Vec<&str> = n.value.split('\n').collect();
            if lines.len() == 1 {
                if needs_quotes(n) {
                    out.push_str(&crate::gofmt::quote(&n.value));
                } else {
                    out.push_str(&n.value);
                }
            } else {
                out.push_str("|\n");
                for (i, line) in lines.iter().enumerate() {
                    out.push_str(prefix);
                    out.push_str(line);
                    if i != lines.len() - 1 {
                        out.push('\n');
                    }
                }
            }
            out.push('\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(a: &str, b: &str) -> String {
        let mut from = load_documents(a).unwrap();
        let mut to = load_documents(b).unwrap();
        yaml_diff(&mut from, &mut to, "f.yaml").unwrap()
    }

    fn plain(s: &str) -> String {
        s.lines()
            .map(str::trim_end)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn added_map_entries_with_reorder() {
        let to = "table:\n  name: t\n  schema: public\nconfiguration:\n  custom_root_fields: {}\nselect_permissions:\n  - role: user\n    permission:\n      columns:\n        - a\n      filter: {}\n";
        let out = diff("{}\n", to);
        assert_eq!(
            plain(&out),
            "f.yaml\n(root level)\n+ three map entries added:\n  configuration:\n    custom_root_fields: {}\n  table:\n    name: t\n    schema: public\n  select_permissions:\n  - role: user\n    permission:\n      columns:\n      - a\n      filter: {}"
        );
    }

    #[test]
    fn named_list_entry_added() {
        let out = diff(
            "actions: []\ncustom_types:\n  objects: []\n",
            "actions:\n  - name: x\n    comment: \"a b\"\n    definition:\n      kind: \"\"\n      handler: '{{U}}/p'\ncustom_types:\n  objects: []\n",
        );
        assert_eq!(
            plain(&out),
            "f.yaml\nactions\n  + one list entry added:\n    - name: x\n      comment: \"a b\"\n      definition:\n        kind:\n        handler: {{U}}/p"
        );
    }

    #[test]
    fn value_changes() {
        let out = diff("a: 1\nb: x\nc: true\n", "a: 2\nb: y\nc: false\n");
        assert_eq!(
            plain(&out),
            "f.yaml\na\n  ± value change\n    - 1\n    + 2\nb\n  ± value change\n    - x\n    + y\nc\n  ± value change\n    - false\n    + true"
                .replace("- false\n    + true", "- true\n    + false")
        );
    }

    #[test]
    fn identical_files_print_nothing() {
        assert_eq!(diff("a: [1, 2]\n", "a:\n  - 1\n  - 2\n"), "");
    }
}
