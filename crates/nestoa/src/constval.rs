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
    eval(project, file, expr, 0)
}

fn eval(project: &Project, file: &Path, expr: &Expr, depth: usize) -> Option<String> {
    if depth > MAX_DEPTH {
        return None;
    }
    match unwrap(expr) {
        Expr::Lit(Lit::Str(s)) => Some(s.value.to_string_lossy().into_owned()),
        Expr::Lit(Lit::Num(n)) => Some(n.value.to_string()),
        Expr::Tpl(t) => {
            let mut out = String::new();
            for (i, quasi) in t.quasis.iter().enumerate() {
                out.push_str(&quasi.cooked.as_ref()?.to_string_lossy());
                if let Some(e) = t.exprs.get(i) {
                    out.push_str(&eval(project, file, e, depth)?);
                }
            }
            Some(out)
        }
        Expr::Bin(b) if b.op == BinaryOp::Add => {
            let mut out = eval(project, file, &b.left, depth)?;
            out.push_str(&eval(project, file, &b.right, depth)?);
            Some(out)
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
fn member(project: &Project, file: &Path, expr: &Expr, depth: usize) -> Option<String> {
    let (root, props) = member_chain(expr)?;
    let (last, parents) = props.split_last()?;
    let owner = std::iter::once(root.as_str())
        .chain(parents.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(".");
    if let Some((_, Symbol::Enum(e))) = project.resolve(file, &owner) {
        return match enum_member_values(&e)
            .into_iter()
            .find(|(n, _)| n == last)?
        {
            (_, Some(EnumValue::Str(s))) => Some(s),
            (_, Some(EnumValue::Num(n))) => Some(n.to_string()),
            (_, None) => None,
        };
    }
    let (decl_file, c) = project.resolve_const(file, &root)?;
    let mut value = unwrap(c.init());
    for prop in &props {
        let Expr::Object(o) = value else { return None };
        value = unwrap(object_prop(o, prop)?);
    }
    eval(project, &decl_file, value, depth + 1)
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

fn object_prop<'a>(o: &'a ObjectLit, name: &str) -> Option<&'a Expr> {
    o.props.iter().rev().find_map(|p| {
        let PropOrSpread::Prop(p) = p else {
            return None;
        };
        let Prop::KeyValue(kv) = p.as_ref() else {
            return None;
        };
        let key = match &kv.key {
            PropName::Ident(i) => i.sym.to_string(),
            PropName::Str(s) => s.value.to_string_lossy().into_owned(),
            _ => return None,
        };
        (key == name).then_some(&*kv.value)
    })
}

/// `expr` without the wrappers that don't change its value: parentheses,
/// `as const`, `as T`, `satisfies T`.
fn unwrap(expr: &Expr) -> &Expr {
    match expr {
        Expr::Paren(p) => unwrap(&p.expr),
        Expr::TsConstAssertion(c) => unwrap(&c.expr),
        Expr::TsAs(a) => unwrap(&a.expr),
        Expr::TsSatisfies(s) => unwrap(&s.expr),
        _ => expr,
    }
}
