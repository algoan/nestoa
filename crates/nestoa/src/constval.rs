//! Static evaluation of the string expressions decorators take as paths and
//! versions: literals, template literals and `+` concatenations of them,
//! variables (`BASE`), properties of object constants (`ROUTES.LIST`) and
//! string enum members (`AggregatorAlias.LINXO`), followed across modules.

use std::path::Path;

use swc_ecma_ast::{BinaryOp, Expr, Lit, MemberProp, ObjectLit, Prop, PropName, PropOrSpread};

use crate::symbols::{EnumValue, Project, Symbol, enum_member_values};

/// How many variables an expression may go through, which also stops a
/// constant defined in terms of itself.
const MAX_DEPTH: usize = 16;

/// The strings `expr` evaluates to: one, or each element of an array.
pub fn strings(project: &Project, file: &Path, expr: &Expr) -> Option<Vec<String>> {
    match unwrap(expr) {
        Expr::Array(a) => a
            .elems
            .iter()
            .map(|el| {
                let el = el.as_ref().filter(|el| el.spread.is_none())?;
                string(project, file, &el.expr)
            })
            .collect(),
        Expr::Ident(i) => {
            let (decl_file, c) = project.resolve_const(file, &i.sym)?;
            match unwrap(c.init()) {
                init @ Expr::Array(_) => strings(project, &decl_file, init),
                _ => string(project, file, expr).map(|s| vec![s]),
            }
        }
        _ => string(project, file, expr).map(|s| vec![s]),
    }
}

/// The string `expr` evaluates to.
pub fn string(project: &Project, file: &Path, expr: &Expr) -> Option<String> {
    eval(project, file, expr, 0).map(text)
}

/// A value as JavaScript turns it into a string.
fn text(value: EnumValue) -> String {
    match value {
        EnumValue::Str(s) => s,
        EnumValue::Num(n) => n.to_string(),
    }
}

fn eval(project: &Project, file: &Path, expr: &Expr, depth: usize) -> Option<EnumValue> {
    if depth > MAX_DEPTH {
        return None;
    }
    match unwrap(expr) {
        Expr::Lit(Lit::Str(s)) => Some(EnumValue::Str(s.value.to_string_lossy().into_owned())),
        Expr::Lit(Lit::Num(n)) => Some(EnumValue::Num(n.value)),
        Expr::Tpl(t) => {
            let mut out = String::new();
            for (i, quasi) in t.quasis.iter().enumerate() {
                out.push_str(&quasi.cooked.as_ref()?.to_string_lossy());
                if let Some(e) = t.exprs.get(i) {
                    out.push_str(&text(eval(project, file, e, depth)?));
                }
            }
            Some(EnumValue::Str(out))
        }
        // `+` adds two numbers and concatenates anything else.
        Expr::Bin(b) if b.op == BinaryOp::Add => {
            match (
                eval(project, file, &b.left, depth)?,
                eval(project, file, &b.right, depth)?,
            ) {
                (EnumValue::Num(l), EnumValue::Num(r)) => Some(EnumValue::Num(l + r)),
                (l, r) => Some(EnumValue::Str(format!("{}{}", text(l), text(r)))),
            }
        }
        Expr::Ident(i) => {
            let (decl_file, c) = project.resolve_const(file, &i.sym)?;
            eval(project, &decl_file, c.init(), depth + 1)
        }
        Expr::Member(_) => member(project, file, expr, depth),
        _ => None,
    }
}

/// `Enum.MEMBER` (possibly namespace-qualified) or `CONST.a.b`.
fn member(project: &Project, file: &Path, expr: &Expr, depth: usize) -> Option<EnumValue> {
    let (root, props) = member_chain(expr)?;
    let (last, parents) = props.split_last()?;
    let owner = std::iter::once(root.as_str())
        .chain(parents.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(".");
    if let Some((_, Symbol::Enum(e))) = project.resolve(file, &owner) {
        return enum_member_values(&e)
            .into_iter()
            .find(|(n, _)| n == last)?
            .1;
    }
    let (decl_file, c) = project.resolve_const(file, &root)?;
    walk(project, &decl_file, c.init(), &props, depth + 1)
}

/// The value at `props` inside `value`, written in `file`: object literals,
/// shorthand properties (`{ LIST }`) and constants naming other objects are
/// followed.
fn walk(
    project: &Project,
    file: &Path,
    value: &Expr,
    props: &[String],
    depth: usize,
) -> Option<EnumValue> {
    let Some((first, rest)) = props.split_first() else {
        return eval(project, file, value, depth);
    };
    if depth > MAX_DEPTH {
        return None;
    }
    match unwrap(value) {
        Expr::Object(o) => match object_prop(o, first)? {
            Property::Value(v) => walk(project, file, v, rest, depth),
            Property::Shorthand(name) => {
                let (decl_file, c) = project.resolve_const(file, name)?;
                walk(project, &decl_file, c.init(), rest, depth + 1)
            }
        },
        Expr::Ident(i) => {
            let (decl_file, c) = project.resolve_const(file, &i.sym)?;
            walk(project, &decl_file, c.init(), props, depth + 1)
        }
        _ => None,
    }
}

/// An object literal property's value: an expression, or the variable a
/// shorthand property (`{ LIST }`) names.
enum Property<'a> {
    Value(&'a Expr),
    Shorthand(&'a str),
}

/// `a.b.c` as `("a", ["b", "c"])`.
fn member_chain(expr: &Expr) -> Option<(String, Vec<String>)> {
    match unwrap(expr) {
        Expr::Ident(i) => Some((i.sym.to_string(), Vec::new())),
        Expr::Member(m) => {
            let MemberProp::Ident(prop) = &m.prop else {
                return None;
            };
            let (root, mut props) = member_chain(&m.obj)?;
            props.push(prop.sym.to_string());
            Some((root, props))
        }
        _ => None,
    }
}

fn object_prop<'a>(o: &'a ObjectLit, name: &str) -> Option<Property<'a>> {
    o.props.iter().rev().find_map(|p| {
        let PropOrSpread::Prop(p) = p else {
            return None;
        };
        match p.as_ref() {
            Prop::KeyValue(kv) => {
                let key = match &kv.key {
                    PropName::Ident(i) => i.sym.to_string(),
                    PropName::Str(s) => s.value.to_string_lossy().into_owned(),
                    _ => return None,
                };
                (key == name).then_some(Property::Value(&kv.value))
            }
            Prop::Shorthand(i) => (i.sym == name).then_some(Property::Shorthand(&i.sym)),
            _ => None,
        }
    })
}

/// `a.b.c` for an identifier or a chain of property accesses.
pub fn dotted_name(expr: &Expr) -> Option<String> {
    let (root, props) = member_chain(expr)?;
    Some(
        std::iter::once(root)
            .chain(props)
            .collect::<Vec<_>>()
            .join("."),
    )
}

/// `expr` without the wrappers that don't change its value: parentheses,
/// `as const`, `as T`, `satisfies T`.
pub fn unwrap(expr: &Expr) -> &Expr {
    match expr {
        Expr::Paren(p) => unwrap(&p.expr),
        Expr::TsConstAssertion(c) => unwrap(&c.expr),
        Expr::TsAs(a) => unwrap(&a.expr),
        Expr::TsSatisfies(s) => unwrap(&s.expr),
        _ => expr,
    }
}
