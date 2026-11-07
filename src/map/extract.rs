//! Tree-sitter extraction of entities, raw calls and raw imports.

use sha2::{Digest, Sha256};
use tree_sitter::{Language, Node, Parser};

use super::{Entity, Kind};

/// A call seen in the source, not resolved yet.
#[derive(Debug, Clone)]
pub struct RawCall {
    pub name: String,
    pub line: u32,
    pub owner: Option<String>,
    pub owner_id: Option<String>,
    /// ident | member | subscript | dynamic | other | jsx | ref
    pub shape: &'static str,
    /// Enclosing class name, used to resolve `this.m()`.
    pub class: Option<String>,
}

/// A name bound by an import statement.
#[derive(Debug, Clone)]
pub struct Binding {
    pub local: String,
    pub imported: String,
    /// named | default | namespace
    pub kind: &'static str,
}

/// An import seen in the source, not resolved yet.
#[derive(Debug, Clone)]
pub struct RawImport {
    pub spec: String,
    pub names: Vec<String>,
    pub bindings: Vec<Binding>,
    pub line: u32,
}

/// An export that is not a plain declaration.
#[derive(Debug, Clone)]
pub struct RawExport {
    pub exported: String,
    pub local: Option<String>,
    pub spec: Option<String>,
    /// named | reexport | star | starns | default
    pub kind: &'static str,
    pub line: u32,
}

/// A parameter or local variable, visible inside the entity `scope_id`.
#[derive(Debug, Clone)]
pub struct Local {
    pub scope_id: String,
    pub name: String,
}

#[derive(Debug, Default)]
pub struct Extracted {
    pub locals: Vec<Local>,
    pub entities: Vec<Entity>,
    pub calls: Vec<RawCall>,
    pub imports: Vec<RawImport>,
    pub exports: Vec<RawExport>,
}

/// Entity that encloses the code being walked.
#[derive(Clone)]
struct Ctx {
    name: String,
    id: String,
}

pub fn is_supported(path: &str) -> bool {
    matches!(
        ext(path).as_deref(),
        Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs")
    )
}

fn ext(path: &str) -> Option<String> {
    let (_, tail) = path.rsplit_once('.')?;
    if tail.is_empty() || tail.len() > 4 {
        return None;
    }
    Some(tail.to_ascii_lowercase())
}

fn language_for(name: &str) -> Option<Language> {
    Some(match name {
        "ts" | "mts" | "cts" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "js" | "mjs" | "cjs" | "jsx" => tree_sitter_javascript::LANGUAGE.into(),
        _ => return None,
    })
}

/// Parse `source`; `None` when the extension is not mapped.
pub fn parse(path: &str, source: &str) -> Option<Extracted> {
    let language = language_for(&ext(path)?)?;
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(source, None)?;
    let src = source.as_bytes();
    let mut out = Extracted::default();
    let name = path.rsplit('/').next().unwrap_or(path).to_string();
    out.entities.push(Entity {
        id: entity_id(path, Kind::File, None, &name, 0),
        kind: Kind::File,
        name,
        file: path.to_string(),
        line: 1,
        container: None,
        parent_id: None,
    });
    walk(src, path, tree.root_node(), None, None, &mut out);
    Some(out)
}

fn text<'a>(src: &'a [u8], node: Node<'_>) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

fn line_of(node: Node<'_>) -> u32 {
    node.start_position().row as u32 + 1
}

fn trim_quotes(value: &str) -> &str {
    value.trim_matches(|c| c == '"' || c == '\'' || c == '`')
}

fn role(callee: &str, index: usize) -> String {
    let base = callee.rsplit('.').next().unwrap_or(callee);
    let known = matches!(base, "serve" | "createServer" | "listen" | "handle" | "on");
    if index == 0 && known {
        "handler".to_string()
    } else {
        (index + 1).to_string()
    }
}

fn contains_jsx(node: Node<'_>) -> bool {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind().starts_with("jsx_") {
            return true;
        }
        let mut cursor = current.walk();
        for child in current.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    false
}

fn fn_kind(_src: &[u8], node: Node<'_>, name: &str) -> Kind {
    let upper = name.chars().next().is_some_and(|c| c.is_ascii_uppercase());
    if upper {
        if let Some(body) = node.child_by_field_name("body") {
            if contains_jsx(body) {
                return Kind::Component;
            }
        }
    }
    Kind::Function
}

fn entity_id(path: &str, kind: Kind, container: Option<&str>, name: &str, pos: usize) -> String {
    let mut hasher = Sha256::new();
    for part in [path, kind.as_str(), container.unwrap_or(""), name] {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    hasher.update(pos.to_string().as_bytes());
    let digest = hasher.finalize();
    let bytes: &[u8] = &digest;
    let mut out = String::with_capacity(16);
    for &byte in bytes.iter().take(8) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn push(
    out: &mut Extracted,
    path: &str,
    kind: Kind,
    name: &str,
    node: Node<'_>,
    container: Option<&Ctx>,
) -> Ctx {
    let container_name = container.map(|c| c.name.as_str());
    let name = &if kind == Kind::Callback {
        format!("{name}@L{}", line_of(node))
    } else {
        name.to_string()
    };
    let id = entity_id(path, kind, container_name, name, node.start_byte());
    out.entities.push(Entity {
        id: id.clone(),
        kind,
        name: name.clone(),
        file: path.to_string(),
        line: line_of(node),
        container: container_name.map(str::to_string),
        parent_id: container.map(|c| c.id.clone()),
    });
    Ctx {
        name: name.to_string(),
        id,
    }
}

fn is_function_node(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "arrow_function" | "function_expression" | "function"
    )
}

fn compact(value: &str) -> String {
    let mut out: String = value.chars().filter(|c| !c.is_whitespace()).collect();
    out = out.replace("?.", ".");
    if out.chars().count() > 80 {
        out = out.chars().take(80).collect();
    }
    out
}

fn is_pure_chain(node: Node<'_>) -> bool {
    match node.kind() {
        "identifier" | "this" | "super" => true,
        "member_expression" => node
            .child_by_field_name("object")
            .is_some_and(is_pure_chain),
        _ => false,
    }
}

/// Callee text and shape (see `RawCall::shape`).
fn classify(src: &[u8], callee: Node<'_>) -> (String, &'static str) {
    match callee.kind() {
        "identifier" => (text(src, callee).to_string(), "ident"),
        "member_expression" if is_pure_chain(callee) => (compact(text(src, callee)), "member"),
        "member_expression" => {
            let prop = callee
                .child_by_field_name("property")
                .map(|n| text(src, n))
                .unwrap_or("");
            (format!("(expr).{prop}"), "other")
        }
        "subscript_expression" => (compact(text(src, callee)), "subscript"),
        "import" => ("import".to_string(), "dynamic"),
        _ => (compact(text(src, callee)), "other"),
    }
}

fn callee_line(callee: Node<'_>, call: Node<'_>) -> u32 {
    if callee.kind() == "member_expression" {
        if let Some(property) = callee.child_by_field_name("property") {
            return line_of(property);
        }
    }
    line_of(call)
}

fn record(
    out: &mut Extracted,
    name: String,
    shape: &'static str,
    line: u32,
    owner: Option<&Ctx>,
    class: Option<&str>,
) {
    out.calls.push(RawCall {
        name,
        line,
        owner: owner.map(|c| c.name.clone()),
        owner_id: owner.map(|c| c.id.clone()),
        shape,
        class: class.map(str::to_string),
    });
}

const WRAPPERS: &[&str] = &["memo", "forwardRef", "useCallback", "observer"];

/// `memo(forwardRef(inner))`: the innermost value, when every layer is a known wrapper.
fn wrapped_inner<'t>(src: &[u8], value: Node<'t>) -> Option<Node<'t>> {
    let mut current = value;
    while current.kind() == "call_expression" {
        let callee = current.child_by_field_name("function")?;
        let name = text(src, callee);
        let base = name.rsplit('.').next().unwrap_or(name);
        if !WRAPPERS.contains(&base) {
            return None;
        }
        let arguments = current.child_by_field_name("arguments")?;
        current = strip_expr(arguments.named_child(0)?);
    }
    (current != value).then_some(current)
}

/// `memo(forwardRef((props) => ...))`: the innermost function.
fn wrapped_function<'t>(src: &[u8], value: Node<'t>) -> Option<Node<'t>> {
    wrapped_inner(src, value).filter(|n| is_function_node(*n))
}

/// Looks through parentheses and TypeScript assertions.
fn strip_expr(node: Node<'_>) -> Node<'_> {
    let mut current = node;
    while matches!(
        current.kind(),
        "parenthesized_expression"
            | "as_expression"
            | "satisfies_expression"
            | "non_null_expression"
            | "type_assertion"
    ) {
        match current.named_child(0) {
            Some(inner) => current = inner,
            None => break,
        }
    }
    current
}

/// Names bound by a parameter or destructuring pattern.
fn pattern_names(src: &[u8], node: Node<'_>, names: &mut Vec<String>) {
    match node.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            names.push(text(src, node).to_string())
        }
        "pair_pattern" => {
            if let Some(value) = node.child_by_field_name("value") {
                pattern_names(src, value, names);
            }
        }
        "assignment_pattern" | "object_assignment_pattern" => {
            if let Some(left) = node.child_by_field_name("left") {
                pattern_names(src, left, names);
            }
        }
        "required_parameter" | "optional_parameter" => {
            if let Some(pattern) = node.child_by_field_name("pattern") {
                pattern_names(src, pattern, names);
            }
        }
        "type_annotation" | "comment" => {}
        _ => {
            let mut cursor = node.walk();
            let children: Vec<Node> = node.named_children(&mut cursor).collect();
            for child in children {
                pattern_names(src, child, names);
            }
        }
    }
}

fn record_locals(src: &[u8], pattern: Node<'_>, scope: &Ctx, out: &mut Extracted) {
    let mut names = Vec::new();
    pattern_names(src, pattern, &mut names);
    for name in names {
        out.locals.push(Local {
            scope_id: scope.id.clone(),
            name,
        });
    }
}

fn record_params(src: &[u8], function: Node<'_>, scope: &Ctx, out: &mut Extracted) {
    if let Some(params) = function.child_by_field_name("parameters") {
        let mut cursor = params.walk();
        let items: Vec<Node> = params.named_children(&mut cursor).collect();
        for item in items {
            record_locals(src, item, scope, out);
        }
    } else if let Some(param) = function.child_by_field_name("parameter") {
        record_locals(src, param, scope, out);
    }
}

/// Names declared by `export <declaration>`.
fn declared_names(src: &[u8], declaration: Node<'_>) -> Vec<String> {
    match declaration.kind() {
        "function_declaration"
        | "generator_function_declaration"
        | "class_declaration"
        | "abstract_class_declaration" => declaration
            .child_by_field_name("name")
            .map(|n| vec![text(src, n).to_string()])
            .unwrap_or_default(),
        "lexical_declaration" | "variable_declaration" => {
            let mut cursor = declaration.walk();
            let declarators: Vec<Node> = declaration
                .named_children(&mut cursor)
                .filter(|n| n.kind() == "variable_declarator")
                .collect();
            let mut names = Vec::new();
            for declarator in declarators {
                if let Some(name) = declarator
                    .child_by_field_name("name")
                    .filter(|n| n.kind() == "identifier")
                {
                    names.push(text(src, name).to_string());
                }
            }
            names
        }
        _ => Vec::new(),
    }
}

fn collect_import_names(src: &[u8], node: Node<'_>, names: &mut Vec<String>) {
    let mut cursor = node.walk();
    let children: Vec<Node> = node.named_children(&mut cursor).collect();
    for child in children {
        match child.kind() {
            "import_specifier" => {
                if let Some(name) = child.child_by_field_name("name") {
                    names.push(text(src, name).to_string());
                }
            }
            "identifier" | "property_identifier" => names.push(text(src, child).to_string()),
            "string" => {}
            _ => collect_import_names(src, child, names),
        }
    }
}

fn collect_bindings(src: &[u8], node: Node<'_>) -> Vec<Binding> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    let clauses: Vec<Node> = node
        .named_children(&mut cursor)
        .filter(|n| n.kind() == "import_clause")
        .collect();
    for clause in clauses {
        let mut inner = clause.walk();
        let parts: Vec<Node> = clause.named_children(&mut inner).collect();
        for part in parts {
            match part.kind() {
                "identifier" => out.push(Binding {
                    local: text(src, part).to_string(),
                    imported: "default".to_string(),
                    kind: "default",
                }),
                "namespace_import" => {
                    let mut c = part.walk();
                    let local = part
                        .named_children(&mut c)
                        .find(|n| n.kind() == "identifier")
                        .map(|n| text(src, n).to_string());
                    if let Some(local) = local {
                        out.push(Binding {
                            local,
                            imported: "*".to_string(),
                            kind: "namespace",
                        });
                    }
                }
                "named_imports" => {
                    let mut c = part.walk();
                    let specs: Vec<Node> = part
                        .named_children(&mut c)
                        .filter(|n| n.kind() == "import_specifier")
                        .collect();
                    for spec in specs {
                        let Some(name) = spec.child_by_field_name("name") else {
                            continue;
                        };
                        let imported = trim_quotes(text(src, name)).to_string();
                        let local = spec
                            .child_by_field_name("alias")
                            .map(|n| text(src, n).to_string())
                            .unwrap_or_else(|| imported.clone());
                        out.push(Binding {
                            local,
                            imported,
                            kind: "named",
                        });
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn collect_exports(src: &[u8], node: Node<'_>, out: &mut Extracted) {
    let spec = node
        .child_by_field_name("source")
        .map(|n| trim_quotes(text(src, n)).to_string());
    let line = line_of(node);
    let mut cursor = node.walk();
    let children: Vec<Node> = node.named_children(&mut cursor).collect();
    let mut has_clause = false;
    for child in children {
        match child.kind() {
            "export_clause" => {
                has_clause = true;
                let mut c = child.walk();
                let specs: Vec<Node> = child
                    .named_children(&mut c)
                    .filter(|n| n.kind() == "export_specifier")
                    .collect();
                for item in specs {
                    let Some(name) = item.child_by_field_name("name") else {
                        continue;
                    };
                    let local = trim_quotes(text(src, name)).to_string();
                    let exported = item
                        .child_by_field_name("alias")
                        .map(|n| trim_quotes(text(src, n)).to_string())
                        .unwrap_or_else(|| local.clone());
                    out.exports.push(RawExport {
                        exported,
                        local: Some(local),
                        spec: spec.clone(),
                        kind: if spec.is_some() { "reexport" } else { "named" },
                        line,
                    });
                }
            }
            "namespace_export" => {
                has_clause = true;
                let mut c = child.walk();
                let name = child
                    .named_children(&mut c)
                    .find(|n| n.kind() == "identifier")
                    .map(|n| text(src, n).to_string());
                if let (Some(name), Some(spec)) = (name, spec.clone()) {
                    out.exports.push(RawExport {
                        exported: name,
                        local: None,
                        spec: Some(spec),
                        kind: "starns",
                        line,
                    });
                }
            }
            _ => {}
        }
    }
    if spec.is_some() && !has_clause {
        out.exports.push(RawExport {
            exported: "*".to_string(),
            local: None,
            spec,
            kind: "star",
            line,
        });
    }
}

/// Records the wrapper calls of `memo(forwardRef(fn))` that sit outside the wrapped function.
fn wrapper_calls(
    src: &[u8],
    value: Node<'_>,
    owner: Option<&Ctx>,
    class: Option<&str>,
    out: &mut Extracted,
) {
    let mut current = value;
    while current.kind() == "call_expression" {
        let Some(callee) = current.child_by_field_name("function") else {
            return;
        };
        let (name, shape) = classify(src, callee);
        record(out, name, shape, callee_line(callee, current), owner, class);
        let Some(first) = current
            .child_by_field_name("arguments")
            .and_then(|a| a.named_child(0))
        else {
            return;
        };
        current = first;
    }
}

fn walk(
    src: &[u8],
    path: &str,
    node: Node<'_>,
    owner: Option<&Ctx>,
    class: Option<&str>,
    out: &mut Extracted,
) {
    match node.kind() {
        "class_declaration" | "class" => {
            if let Some(name) = node
                .child_by_field_name("name")
                .map(|n| text(src, n).to_string())
            {
                let ctx = push(out, path, Kind::Class, &name, node, owner);
                let mut cursor = node.walk();
                let children: Vec<Node> = node.named_children(&mut cursor).collect();
                for child in children {
                    walk(src, path, child, Some(&ctx), Some(&name), out);
                }
                return;
            }
        }
        "function_declaration" | "generator_function_declaration" => {
            if let Some(name) = node
                .child_by_field_name("name")
                .map(|n| text(src, n).to_string())
            {
                let kind = fn_kind(src, node, &name);
                let ctx = push(out, path, kind, &name, node, owner);
                record_params(src, node, &ctx, out);
                if let Some(body) = node.child_by_field_name("body") {
                    walk(src, path, body, Some(&ctx), class, out);
                }
                return;
            }
        }
        "method_definition" => {
            if let Some(name) = node
                .child_by_field_name("name")
                .map(|n| text(src, n).to_string())
            {
                let ctx = push(out, path, Kind::Method, &name, node, owner);
                record_params(src, node, &ctx, out);
                if let Some(body) = node.child_by_field_name("body") {
                    walk(src, path, body, Some(&ctx), class, out);
                }
                return;
            }
        }
        "lexical_declaration" | "variable_declaration" => {
            let mut cursor = node.walk();
            let declarators: Vec<Node> = node
                .named_children(&mut cursor)
                .filter(|n| n.kind() == "variable_declarator")
                .collect();
            for declarator in declarators {
                let name_node = declarator.child_by_field_name("name");
                let name = name_node
                    .filter(|n| n.kind() == "identifier")
                    .map(|n| text(src, n).to_string());
                let value = declarator.child_by_field_name("value");
                match (name, value) {
                    (Some(name), Some(value)) if is_function_node(value) => {
                        let kind = fn_kind(src, value, &name);
                        let ctx = push(out, path, kind, &name, declarator, owner);
                        record_params(src, value, &ctx, out);
                        if let Some(body) = value.child_by_field_name("body") {
                            walk(src, path, body, Some(&ctx), class, out);
                        }
                    }
                    (Some(name), Some(value)) if wrapped_function(src, value).is_some() => {
                        let function = wrapped_function(src, value).unwrap_or(value);
                        let kind = fn_kind(src, function, &name);
                        let ctx = push(out, path, kind, &name, declarator, owner);
                        wrapper_calls(src, value, owner, class, out);
                        record_params(src, function, &ctx, out);
                        if let Some(body) = function.child_by_field_name("body") {
                            walk(src, path, body, Some(&ctx), class, out);
                        }
                    }
                    (_, value) => {
                        if let (Some(scope), Some(pattern)) = (owner, name_node) {
                            record_locals(src, pattern, scope, out);
                        }
                        if let Some(value) = value {
                            walk(src, path, value, owner, class, out);
                        }
                    }
                }
            }
            return;
        }
        "call_expression" | "new_expression" => {
            let field = if node.kind() == "new_expression" {
                "constructor"
            } else {
                "function"
            };
            let callee = node.child_by_field_name(field);
            let (callee_name, shape) = callee
                .map(|n| classify(src, n))
                .unwrap_or((String::new(), "other"));
            let line = callee.map_or(line_of(node), |n| callee_line(n, node));
            record(out, callee_name.clone(), shape, line, owner, class);
            if let Some(callee) = callee {
                if !matches!(callee.kind(), "identifier" | "import") && !is_pure_chain(callee) {
                    walk(src, path, callee, owner, class, out);
                }
            }
            if let Some(arguments) = node.child_by_field_name("arguments") {
                let mut cursor = arguments.walk();
                let items: Vec<Node> = arguments.named_children(&mut cursor).collect();
                for (index, item) in items.into_iter().enumerate() {
                    if is_function_node(item) {
                        let name = format!("{callee_name}#{}", role(&callee_name, index));
                        let ctx = push(out, path, Kind::Callback, &name, item, owner);
                        record_params(src, item, &ctx, out);
                        if let Some(body) = item.child_by_field_name("body") {
                            walk(src, path, body, Some(&ctx), class, out);
                        }
                    } else if item.kind() == "identifier" {
                        record(
                            out,
                            text(src, item).to_string(),
                            "ref",
                            line_of(item),
                            owner,
                            class,
                        );
                    } else {
                        walk(src, path, item, owner, class, out);
                    }
                }
            }
            return;
        }
        "jsx_opening_element" | "jsx_self_closing_element" => {
            if let Some(name) = node.child_by_field_name("name") {
                let upper = text(src, name)
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_uppercase());
                let component = match name.kind() {
                    "identifier" => upper,
                    "member_expression" => is_pure_chain(name),
                    _ => false,
                };
                if component {
                    record(
                        out,
                        compact(text(src, name)),
                        "jsx",
                        line_of(name),
                        owner,
                        class,
                    );
                }
            }
        }
        "jsx_attribute" => {
            let mut cursor = node.walk();
            let children: Vec<Node> = node.named_children(&mut cursor).collect();
            let attr = children.first().map(|n| text(src, *n).to_string());
            let value = children
                .iter()
                .find(|n| n.kind() == "jsx_expression")
                .and_then(|n| n.named_child(0));
            match (attr, value) {
                (Some(attr), Some(value)) if is_function_node(value) => {
                    let name = format!("{attr}#handler");
                    let ctx = push(out, path, Kind::Callback, &name, value, owner);
                    record_params(src, value, &ctx, out);
                    if let Some(body) = value.child_by_field_name("body") {
                        walk(src, path, body, Some(&ctx), class, out);
                    }
                }
                (_, Some(value)) if value.kind() == "identifier" => record(
                    out,
                    text(src, value).to_string(),
                    "ref",
                    line_of(value),
                    owner,
                    class,
                ),
                (_, Some(value)) => walk(src, path, value, owner, class, out),
                _ => {}
            }
            return;
        }
        "import_statement" => {
            if let Some(source) = node.child_by_field_name("source") {
                let spec = trim_quotes(text(src, source)).to_string();
                let mut names = Vec::new();
                collect_import_names(src, node, &mut names);
                out.imports.push(RawImport {
                    spec,
                    names,
                    bindings: collect_bindings(src, node),
                    line: line_of(node),
                });
            }
            return;
        }
        "export_statement" => {
            collect_exports(src, node, out);
            let is_default = {
                let mut cursor = node.walk();
                let found = node.children(&mut cursor).any(|c| c.kind() == "default");
                found
            };
            if !is_default {
                if let Some(declaration) = node.child_by_field_name("declaration") {
                    let line = line_of(node);
                    for name in declared_names(src, declaration) {
                        out.exports.push(RawExport {
                            exported: name.clone(),
                            local: Some(name),
                            spec: None,
                            kind: "named",
                            line,
                        });
                    }
                }
            }
            if is_default {
                let target = node
                    .child_by_field_name("declaration")
                    .or_else(|| node.child_by_field_name("value"))
                    .map(strip_expr);
                if let Some(target) = target {
                    let line = line_of(node);
                    let inner = wrapped_inner(src, target);
                    if let Some(inner) = inner.filter(|n| n.kind() == "identifier") {
                        out.exports.push(RawExport {
                            exported: "default".to_string(),
                            local: Some(text(src, inner).to_string()),
                            spec: None,
                            kind: "default",
                            line,
                        });
                        wrapper_calls(src, target, owner, class, out);
                        return;
                    }
                    let function = inner.filter(|n| is_function_node(*n)).unwrap_or(target);
                    let named = function.child_by_field_name("name").map(|n| text(src, n));
                    let anonymous_fn = is_function_node(function)
                        || (function.kind() == "function_declaration" && named.is_none());
                    if anonymous_fn {
                        if function != target {
                            wrapper_calls(src, target, owner, class, out);
                        }
                        let ctx = push(
                            out,
                            path,
                            Kind::Callback,
                            "default#handler",
                            function,
                            owner,
                        );
                        out.exports.push(RawExport {
                            exported: "default".to_string(),
                            local: Some(ctx.name.clone()),
                            spec: None,
                            kind: "default",
                            line,
                        });
                        record_params(src, function, &ctx, out);
                        if let Some(body) = function.child_by_field_name("body") {
                            walk(src, path, body, Some(&ctx), class, out);
                        }
                        return;
                    }
                    let local = match target.kind() {
                        "identifier" => Some(text(src, target).to_string()),
                        _ => named.map(str::to_string),
                    };
                    out.exports.push(RawExport {
                        exported: "default".to_string(),
                        local,
                        spec: None,
                        kind: "default",
                        line,
                    });
                }
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    let children: Vec<Node> = node.named_children(&mut cursor).collect();
    for child in children {
        walk(src, path, child, owner, class, out);
    }
}
