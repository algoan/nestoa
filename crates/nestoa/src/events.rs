//! Message events of a NestJS service: the `@EventPattern`/`@MessagePattern`
//! handlers of its controllers, which receive, and the configured publish
//! calls of any class, which send. `asyncapi.rs` assembles them into a
//! document.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use swc_common::Spanned;
use swc_ecma_ast::{
    CallExpr, Callee, Class, ClassMember, Expr, Function, Lit, MemberProp, ParamOrTsParamProp, Pat,
    Prop, PropOrSpread, TsEntityName, TsParamPropParam, TsType, VarDeclarator,
};
use swc_ecma_visit::{Visit, VisitWith};

use crate::config::{AsyncApi, Config, Publisher};
use crate::routes::{first_arg, module_classes, prop_name_text, strip_suffix_ci};
use crate::schema::{ComponentSchemas, SchemaCtx, decorator_name};
use crate::symbols::{Project, Symbol};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    Send,
    Receive,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::Receive => "receive",
        }
    }
}

/// One message a handler receives or a method sends on a channel.
#[derive(Clone, Debug)]
pub struct EventOperation {
    pub action: Action,
    /// The pattern, which is the channel's address.
    pub channel: String,
    /// The payload schema; `null` when the message carries none.
    pub payload: Value,
    /// `Class.method`.
    pub handler: String,
    pub tag: String,
    pub summary: Option<String>,
    pub description: Option<String>,
}

/// How deep an expression's type is followed through the locals it reads.
const MAX_INFERENCE_DEPTH: usize = 8;

pub struct EventExtractor<'a> {
    pub config: &'a Config,
    pub asyncapi: &'a AsyncApi,
    pub project: &'a Project,
    pub entry_files: &'a [PathBuf],
    pub components: &'a mut ComponentSchemas,
}

impl<'a> EventExtractor<'a> {
    pub fn extract_all(&mut self) -> Vec<EventOperation> {
        let mut out = Vec::new();
        let project = self.project;
        for file in self.entry_files {
            let Some(parsed) = project.files.get(file) else {
                continue;
            };
            for (name, class) in module_classes(file, &parsed.module) {
                if class
                    .decorators
                    .iter()
                    .any(|d| decorator_name(d) == "Controller")
                {
                    self.consumers(file, &name, class, &mut out);
                }
                self.publishers(file, &name, class, &mut out);
            }
        }
        out
    }

    fn schema_ctx(&mut self) -> SchemaCtx<'_> {
        SchemaCtx {
            project: self.project,
            components: self.components,
            type_env: None,
            inline_generics: false,
            current_owner: None,
            instantiating: Vec::new(),
            passthrough_types: &self.config.passthrough_types,
        }
    }

    /// The handlers of a controller, its inherited ones included.
    fn consumers(
        &mut self,
        file: &Path,
        class_name: &str,
        class: &Class,
        out: &mut Vec<EventOperation>,
    ) {
        let tag = strip_suffix_ci(class_name, "Controller").to_string();
        let mut seen = std::collections::HashSet::new();
        let mut owner = (file.to_path_buf(), None::<crate::symbols::Shared<Class>>);
        let mut walked = std::collections::HashSet::new();
        loop {
            let current: &Class = owner.1.as_deref().unwrap_or(class);
            if !walked.insert((owner.0.clone(), current.span.lo)) {
                break;
            }
            let mut declared = Vec::new();
            for member in &current.body {
                let ClassMember::Method(m) = member else {
                    continue;
                };
                let name = prop_name_text(&m.key);
                if m.is_static || m.function.body.is_none() {
                    continue;
                }
                if name.is_empty() || !seen.contains(&name) {
                    let handler = format!("{class_name}.{name}");
                    self.consumer(&owner.0, &handler, &tag, m, out);
                }
                if !name.is_empty() {
                    declared.push(name);
                }
            }
            seen.extend(declared);
            let Some(base) = current
                .super_class
                .as_deref()
                .and_then(crate::constval::dotted_name)
                .and_then(|n| project_class(self.project, &owner.0, &n))
            else {
                break;
            };
            owner = (base.0, Some(base.1));
        }
    }

    fn consumer(
        &mut self,
        file: &Path,
        handler: &str,
        tag: &str,
        m: &swc_ecma_ast::ClassMethod,
        out: &mut Vec<EventOperation>,
    ) {
        let Some(pattern) = m.function.decorators.iter().find(|d| {
            matches!(
                decorator_name(d).as_str(),
                "EventPattern" | "MessagePattern"
            )
        }) else {
            return;
        };
        let Some(channels) = first_arg(pattern).and_then(|e| {
            object_pattern(self.project, file, e)
                .map(|p| vec![p])
                .or_else(|| crate::constval::strings(self.project, file, e))
        }) else {
            crate::diag::warn(format!(
                "{handler}: cannot evaluate its {} pattern; it is left out",
                decorator_name(pattern)
            ));
            return;
        };
        let payload = self.consumer_payload(file, &m.function);
        let doc = self
            .project
            .files
            .get(file)
            .and_then(|f| f.docs.jsdoc(m.span.lo))
            .unwrap_or_default();
        for channel in channels {
            out.push(EventOperation {
                action: Action::Receive,
                channel,
                payload: payload.clone(),
                handler: handler.to_string(),
                tag: tag.to_string(),
                summary: doc.summary.clone(),
                description: doc.description.clone(),
            });
        }
    }

    /// The type of the `@Payload()` parameter, or the object of the
    /// properties `@Payload('key')` parameters read. Without either, NestJS
    /// passes the payload as the first undecorated parameter.
    fn consumer_payload(&mut self, file: &Path, function: &Function) -> Value {
        let mut whole: Option<Value> = None;
        let mut properties = Map::new();
        let mut required = Vec::new();
        for p in &function.params {
            let Some(d) = p.decorators.iter().find(|d| decorator_name(d) == "Payload") else {
                continue;
            };
            let key = first_arg(d).and_then(|e| crate::constval::string(self.project, file, e));
            let schema = self.param_schema(file, &p.pat);
            match key {
                Some(key) => {
                    if !matches!(&p.pat, Pat::Ident(i) if i.optional) {
                        required.push(Value::String(key.clone()));
                    }
                    properties.insert(key, schema);
                }
                None => whole = Some(schema),
            }
        }
        if let Some(schema) = whole {
            return schema;
        }
        if !properties.is_empty() {
            let mut object = Map::new();
            object.insert("type".into(), json!("object"));
            object.insert("properties".into(), Value::Object(properties));
            if !required.is_empty() {
                object.insert("required".into(), Value::Array(required));
            }
            return Value::Object(object);
        }
        match function.params.first() {
            Some(p) if p.decorators.is_empty() => self.param_schema(file, &p.pat),
            _ => Value::Null,
        }
    }

    fn param_schema(&mut self, file: &Path, pat: &Pat) -> Value {
        let ty = match pat {
            Pat::Ident(i) => i.type_ann.as_deref(),
            Pat::Object(o) => o.type_ann.as_deref(),
            Pat::Assign(a) => match &*a.left {
                Pat::Ident(i) => i.type_ann.as_deref(),
                _ => None,
            },
            _ => None,
        };
        match ty {
            Some(t) => self.payload_type_schema(file, &t.type_ann),
            None => json!({ "x-unresolved": "payload" }),
        }
    }

    /// `ty` lowered, unwrapped from the `payloadWrappers` around it.
    fn payload_type_schema(&mut self, file: &Path, ty: &TsType) -> Value {
        let ty = unwrap_wrappers(ty, &self.asyncapi.payload_wrappers);
        self.schema_ctx().ts_type_to_schema(file, ty)
    }

    /// The configured publish calls in the members of a class.
    fn publishers(
        &mut self,
        file: &Path,
        class_name: &str,
        class: &Class,
        out: &mut Vec<EventOperation>,
    ) {
        if self.asyncapi.publishers.is_empty() {
            return;
        }
        let fields = class_fields(class);
        let methods = class_methods(class);
        let tag = strip_suffix_ci(class_name, "Controller").to_string();
        for member in &class.body {
            let (name, function, value): (String, Option<&Function>, Option<&Expr>) = match member {
                ClassMember::Method(m) => (prop_name_text(&m.key), Some(&m.function), None),
                ClassMember::PrivateMethod(m) => {
                    (format!("#{}", m.key.name), Some(&m.function), None)
                }
                ClassMember::ClassProp(p) => (prop_name_text(&p.key), None, p.value.as_deref()),
                ClassMember::Constructor(_) => ("constructor".to_string(), None, None),
                _ => continue,
            };
            let handler = format!("{class_name}.{name}");
            let mut calls = CallCollector::default();
            let mut locals = Locals::default();
            match (member, function, value) {
                (ClassMember::Constructor(c), _, _) => {
                    for p in &c.params {
                        if let ParamOrTsParamProp::Param(p) = p {
                            locals.bind_pat(&p.pat);
                        }
                    }
                    c.body.visit_with(&mut calls);
                    c.body.visit_with(&mut locals);
                }
                (_, Some(f), _) => {
                    for p in &f.params {
                        locals.bind_pat(&p.pat);
                    }
                    f.body.visit_with(&mut calls);
                    f.body.visit_with(&mut locals);
                }
                (_, _, Some(v)) => {
                    v.visit_with(&mut calls);
                    v.visit_with(&mut locals);
                }
                _ => continue,
            }
            if calls.calls.is_empty() {
                continue;
            }
            let doc = function
                .and_then(|_| self.project.files.get(file))
                .and_then(|f| f.docs.jsdoc(member.span().lo))
                .unwrap_or_default();
            let scope = Scope {
                locals: &locals.bindings,
                fields: &fields,
                methods: &methods,
            };
            for call in &calls.calls {
                let Some(publisher) = self.publisher_of(call, &fields) else {
                    continue;
                };
                let Some(arg) = call.args.get(publisher.channel) else {
                    continue;
                };
                let Some(channel) = crate::constval::string(self.project, file, &arg.expr) else {
                    crate::diag::warn(format!(
                        "{handler}: cannot evaluate the channel of a `{}` call; it is left out",
                        publisher.method
                    ));
                    continue;
                };
                let payload = self.publisher_payload(file, call, publisher, &scope);
                out.push(EventOperation {
                    action: Action::Send,
                    channel,
                    payload,
                    handler: handler.clone(),
                    tag: tag.clone(),
                    summary: doc.summary.clone(),
                    description: doc.description.clone(),
                });
            }
        }
    }

    /// The publisher a call matches: `<receiver>.<method>(…)`, the receiver
    /// being a `this.<field>` declared with one of its `receiverTypes`.
    fn publisher_of(
        &self,
        call: &CallExpr,
        fields: &HashMap<String, Option<TsType>>,
    ) -> Option<&'a Publisher> {
        let Callee::Expr(callee) = &call.callee else {
            return None;
        };
        let Expr::Member(member) = crate::constval::unwrap(callee) else {
            return None;
        };
        let MemberProp::Ident(method) = &member.prop else {
            return None;
        };
        let receiver_type = || {
            let Expr::Member(field) = crate::constval::unwrap(&member.obj) else {
                return None;
            };
            if !matches!(&*field.obj, Expr::This(_)) {
                return None;
            }
            let MemberProp::Ident(name) = &field.prop else {
                return None;
            };
            match fields.get(name.sym.as_ref())? {
                Some(TsType::TsTypeRef(r)) => Some(entity_last(&r.type_name)),
                _ => None,
            }
        };
        self.asyncapi.publishers.iter().find(|p| {
            p.method == method.sym.as_ref()
                && (p.receiver_types.is_empty()
                    || receiver_type().is_some_and(|t| p.receiver_types.contains(&t)))
        })
    }

    /// The call's payload type argument, else the type of its payload
    /// argument (or of `payloadProperty` in it).
    fn publisher_payload(
        &mut self,
        file: &Path,
        call: &CallExpr,
        publisher: &Publisher,
        scope: &Scope,
    ) -> Value {
        if let Some(ty) = publisher
            .type_argument
            .and_then(|i| call.type_args.as_ref()?.params.get(i))
        {
            return self.payload_type_schema(file, ty);
        }
        let Some(arg) = call.args.get(publisher.payload) else {
            return Value::Null;
        };
        let expr = match &publisher.payload_property {
            None => (*arg.expr).clone(),
            Some(key) => match crate::constval::unwrap(&arg.expr) {
                Expr::Object(o) => match object_member(o, key) {
                    Some(e) => e,
                    None => return Value::Null,
                },
                other => return self.unresolved_expr(file, other),
            },
        };
        self.expr_schema(file, &expr, scope, 0)
    }

    /// The type TypeScript infers for `expr`, as far as declared types reach:
    /// literals, `as` assertions, `new` expressions, locals, parameters and
    /// fields with a type, and object and array literals of those.
    fn expr_schema(&mut self, file: &Path, expr: &Expr, scope: &Scope, depth: usize) -> Value {
        if depth > MAX_INFERENCE_DEPTH {
            return self.unresolved_expr(file, expr);
        }
        match expr {
            Expr::Paren(p) => self.expr_schema(file, &p.expr, scope, depth),
            Expr::Await(a) => self.expr_schema(file, &a.arg, scope, depth),
            Expr::TsNonNull(n) => self.expr_schema(file, &n.expr, scope, depth),
            Expr::TsSatisfies(s) => self.expr_schema(file, &s.expr, scope, depth),
            Expr::TsAs(a) => self.payload_type_schema(file, &a.type_ann),
            Expr::TsTypeAssertion(a) => self.payload_type_schema(file, &a.type_ann),
            Expr::Lit(Lit::Null(_)) => json!({ "type": "null" }),
            Expr::Lit(_) | Expr::Tpl(_) | Expr::Unary(_) => {
                self.schema_ctx().initializer_schema(file, expr)
            }
            Expr::Ident(i) if i.sym == "undefined" => Value::Null,
            Expr::Ident(i) => match scope.locals.get(i.sym.as_ref()) {
                Some(Some(Binding::Type(ty))) => self.payload_type_schema(file, ty),
                Some(Some(Binding::Init(init))) => self.expr_schema(file, init, scope, depth + 1),
                _ => self.unresolved_expr(file, expr),
            },
            Expr::Member(m) if matches!(&*m.obj, Expr::This(_)) => {
                let ty = match &m.prop {
                    MemberProp::Ident(name) => scope.fields.get(name.sym.as_ref()),
                    _ => None,
                };
                match ty {
                    Some(Some(ty)) => self.payload_type_schema(file, ty),
                    _ => self.unresolved_expr(file, expr),
                }
            }
            Expr::Member(m) => self.member_schema(file, expr, &m.obj, &m.prop, scope, depth),
            Expr::OptChain(o) => match &*o.base {
                swc_ecma_ast::OptChainBase::Member(m) => {
                    self.member_schema(file, expr, &m.obj, &m.prop, scope, depth)
                }
                swc_ecma_ast::OptChainBase::Call(_) => self.unresolved_expr(file, expr),
            },
            Expr::New(n) => match crate::constval::dotted_name(&n.callee) {
                Some(name) if name == "Date" => self.schema_ctx().initializer_schema(file, expr),
                Some(name) => self.schema_ctx().named_type_to_schema(file, &name),
                None => self.unresolved_expr(file, expr),
            },
            Expr::Call(c) => {
                let method = match &c.callee {
                    Callee::Expr(callee) => match crate::constval::unwrap(callee) {
                        Expr::Member(m) if matches!(&*m.obj, Expr::This(_)) => match &m.prop {
                            MemberProp::Ident(name) => scope.methods.get(name.sym.as_ref()),
                            _ => None,
                        },
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(Some(ty)) = method {
                    return self.payload_type_schema(file, ty);
                }
                match self.function_return(file, &c.callee, scope) {
                    Some((decl_file, ty)) => self.payload_type_schema(&decl_file, &ty),
                    None => self.unresolved_expr(file, expr),
                }
            }
            Expr::Object(o) => self.object_schema(file, o, scope, depth),
            Expr::Array(a) => {
                let mut items: Vec<Value> = Vec::new();
                for el in a.elems.iter().flatten() {
                    if el.spread.is_some() {
                        return self.unresolved_expr(file, expr);
                    }
                    let schema = self.expr_schema(file, &el.expr, scope, depth + 1);
                    if !items.contains(&schema) {
                        items.push(schema);
                    }
                }
                match items.len() {
                    0 => json!({ "type": "array" }),
                    1 => json!({ "type": "array", "items": items.remove(0) }),
                    _ => json!({ "type": "array", "items": { "anyOf": items } }),
                }
            }
            Expr::Cond(c) => {
                let mut branches = Vec::new();
                for e in [&c.cons, &c.alt] {
                    let schema = self.expr_schema(file, e, scope, depth + 1);
                    if !schema.is_null() && !branches.contains(&schema) {
                        branches.push(schema);
                    }
                }
                match branches.len() {
                    0 => Value::Null,
                    1 => branches.remove(0),
                    _ => json!({ "anyOf": branches }),
                }
            }
            _ => self.unresolved_expr(file, expr),
        }
    }

    /// The declared return type of the function a call names, with the file
    /// declaring it: a function declaration, or a `const` arrow function.
    fn function_return(
        &self,
        file: &Path,
        callee: &Callee,
        scope: &Scope,
    ) -> Option<(PathBuf, TsType)> {
        let Callee::Expr(callee) = callee else {
            return None;
        };
        let Expr::Ident(name) = crate::constval::unwrap(callee) else {
            return None;
        };
        if scope.locals.contains_key(name.sym.as_ref()) {
            return None;
        }
        if let Some((decl_file, f)) = self.project.resolve_function(file, &name.sym) {
            return Some((decl_file, f.return_type()?.clone()));
        }
        let (decl_file, c) = self.project.resolve_const(file, &name.sym)?;
        let ty = match crate::constval::unwrap(c.init()) {
            Expr::Arrow(a) => a.return_type.as_deref(),
            Expr::Fn(f) => f.function.return_type.as_deref(),
            _ => None,
        }?;
        Some((decl_file, (*ty.type_ann).clone()))
    }

    /// `obj.prop`: the property's schema in the object `obj` has.
    fn member_schema(
        &mut self,
        file: &Path,
        expr: &Expr,
        obj: &Expr,
        prop: &MemberProp,
        scope: &Scope,
        depth: usize,
    ) -> Value {
        let key = match prop {
            MemberProp::Ident(i) => Some(i.sym.to_string()),
            MemberProp::Computed(c) => match crate::constval::unwrap(&c.expr) {
                Expr::Lit(Lit::Str(s)) => Some(s.value.to_string_lossy().into_owned()),
                _ => None,
            },
            MemberProp::PrivateName(_) => None,
        };
        // An enum member in a mutable position widens to its enum.
        if let Some(owner) = crate::constval::dotted_name(obj)
            && !scope
                .locals
                .contains_key(owner.split('.').next().unwrap_or_default())
            && let Some((_, Symbol::Enum(_))) = self.project.resolve(file, &owner)
        {
            return self.schema_ctx().named_type_to_schema(file, &owner);
        }
        let object = self.expr_schema(file, obj, scope, depth + 1);
        key.and_then(|k| self.components.property(&object, &k))
            .unwrap_or_else(|| self.unresolved_expr(file, expr))
    }

    fn object_schema(
        &mut self,
        file: &Path,
        o: &swc_ecma_ast::ObjectLit,
        scope: &Scope,
        depth: usize,
    ) -> Value {
        let mut properties = Map::new();
        let mut required = Vec::new();
        let mut spreads = Vec::new();
        for prop in &o.props {
            let (key, schema) = match prop {
                PropOrSpread::Spread(s) => {
                    spreads.push(self.expr_schema(file, &s.expr, scope, depth + 1));
                    continue;
                }
                PropOrSpread::Prop(p) => match &**p {
                    Prop::KeyValue(kv) => (
                        prop_name_text(&kv.key),
                        self.expr_schema(file, &kv.value, scope, depth + 1),
                    ),
                    Prop::Shorthand(id) => (
                        id.sym.to_string(),
                        self.expr_schema(file, &Expr::Ident(id.clone()), scope, depth + 1),
                    ),
                    _ => continue,
                },
            };
            if key.is_empty() || schema.is_null() {
                continue;
            }
            required.push(Value::String(key.clone()));
            properties.insert(key, schema);
        }
        let mut object = Map::new();
        object.insert("type".into(), json!("object"));
        if !properties.is_empty() {
            object.insert("properties".into(), Value::Object(properties));
        }
        if !required.is_empty() {
            object.insert("required".into(), Value::Array(required));
        }
        if spreads.is_empty() {
            return Value::Object(object);
        }
        let mut branches = spreads;
        if object.len() > 1 {
            branches.push(Value::Object(object));
        }
        json!({ "allOf": branches })
    }

    /// An expression whose type is not inferred, marked with its source text.
    fn unresolved_expr(&self, file: &Path, expr: &Expr) -> Value {
        const MAX_EXCERPT: usize = 80;
        let text = self
            .project
            .files
            .get(file)
            .and_then(|f| f.text(expr.span()))
            .unwrap_or_else(|| "expression".to_string());
        let text = match text.char_indices().nth(MAX_EXCERPT) {
            Some((end, _)) => format!("{}…", &text[..end]),
            None => text,
        };
        json!({ "x-unresolved": text })
    }
}

/// The route NestJS matches an object pattern by: its keys sorted, values
/// written as JSON (`{ cmd: 'sum' }` is `{"cmd":"sum"}`).
fn object_pattern(project: &Project, file: &Path, expr: &Expr) -> Option<String> {
    let Expr::Object(o) = crate::constval::unwrap(expr) else {
        return None;
    };
    let mut entries = Vec::new();
    for prop in &o.props {
        let PropOrSpread::Prop(p) = prop else {
            return None;
        };
        let Prop::KeyValue(kv) = &**p else {
            return None;
        };
        let key = prop_name_text(&kv.key);
        let value = match object_pattern(project, file, &kv.value) {
            Some(nested) => nested,
            None => match crate::constval::value(project, file, &kv.value)? {
                crate::symbols::EnumValue::Str(s) => format!("\"{s}\""),
                crate::symbols::EnumValue::Num(_) => {
                    crate::constval::string(project, file, &kv.value)?
                }
            },
        };
        entries.push((key, value));
    }
    entries.sort();
    let fields: Vec<String> = entries
        .into_iter()
        .map(|(k, v)| format!("\"{k}\":{v}"))
        .collect();
    Some(format!("{{{}}}", fields.join(",")))
}

/// `ty` with every `wrappers` envelope around it replaced by its first type
/// argument.
fn unwrap_wrappers<'t>(mut ty: &'t TsType, wrappers: &[String]) -> &'t TsType {
    loop {
        match ty {
            TsType::TsParenthesizedType(p) => ty = &p.type_ann,
            TsType::TsTypeRef(r) if wrappers.contains(&entity_last(&r.type_name)) => {
                match r.type_params.as_ref().and_then(|a| a.params.first()) {
                    Some(arg) => ty = arg,
                    None => return ty,
                }
            }
            _ => return ty,
        }
    }
}

/// The last segment of `Ns.Name`.
fn entity_last(name: &TsEntityName) -> String {
    match name {
        TsEntityName::Ident(i) => i.sym.to_string(),
        TsEntityName::TsQualifiedName(q) => q.right.sym.to_string(),
    }
}

fn project_class(
    project: &Project,
    file: &Path,
    name: &str,
) -> Option<(PathBuf, crate::symbols::Shared<Class>)> {
    match project.resolve(file, name)? {
        (base_file, Symbol::Class(c)) => Some((base_file, c)),
        _ => None,
    }
}

/// The value of `key` in an object literal, a shorthand property included.
fn object_member(o: &swc_ecma_ast::ObjectLit, key: &str) -> Option<Expr> {
    o.props.iter().rev().find_map(|p| match p {
        PropOrSpread::Prop(p) => match &**p {
            Prop::KeyValue(kv) if prop_name_text(&kv.key) == key => Some((*kv.value).clone()),
            Prop::Shorthand(id) if id.sym == key => Some(Expr::Ident(id.clone())),
            _ => None,
        },
        PropOrSpread::Spread(_) => None,
    })
}

fn class_methods(class: &Class) -> HashMap<String, Option<TsType>> {
    let mut methods: HashMap<String, Option<TsType>> = HashMap::new();
    for member in &class.body {
        if let ClassMember::Method(m) = member {
            let ty = m
                .function
                .return_type
                .as_ref()
                .map(|t| (*t.type_ann).clone());
            // An overload signature and its implementation share the name.
            methods
                .entry(prop_name_text(&m.key))
                .and_modify(|t| {
                    if t.is_none() {
                        t.clone_from(&ty)
                    }
                })
                .or_insert(ty);
        }
    }
    methods
}

/// The declared type of each field of a class, constructor parameter
/// properties included; `None` for a field declared without one.
fn class_fields(class: &Class) -> HashMap<String, Option<TsType>> {
    let mut fields = HashMap::new();
    for member in &class.body {
        match member {
            ClassMember::ClassProp(p) => {
                fields.insert(
                    prop_name_text(&p.key),
                    p.type_ann.as_ref().map(|t| (*t.type_ann).clone()),
                );
            }
            ClassMember::Constructor(c) => {
                for p in &c.params {
                    if let ParamOrTsParamProp::TsParamProp(p) = p
                        && let TsParamPropParam::Ident(i) = &p.param
                    {
                        fields.insert(
                            i.id.sym.to_string(),
                            i.type_ann.as_ref().map(|t| (*t.type_ann).clone()),
                        );
                    }
                }
            }
            _ => {}
        }
    }
    fields
}

struct Scope<'s> {
    /// `None` for a name bound more than once in the member.
    locals: &'s HashMap<String, Option<Binding>>,
    fields: &'s HashMap<String, Option<TsType>>,
    /// The declared return type of each method of the class.
    methods: &'s HashMap<String, Option<TsType>>,
}

#[derive(Clone)]
enum Binding {
    Type(TsType),
    Init(Expr),
}

/// The names a member binds — parameters and variables, of nested functions
/// too — with what they are declared with. Scopes are not told apart: a name
/// bound twice is ambiguous.
#[derive(Default)]
struct Locals {
    bindings: HashMap<String, Option<Binding>>,
}

impl Locals {
    fn bind(&mut self, name: String, binding: Option<Binding>) {
        self.bindings
            .entry(name)
            .and_modify(|b| *b = None)
            .or_insert(binding);
    }

    fn bind_pat(&mut self, pat: &Pat) {
        match pat {
            Pat::Ident(i) => self.bind(
                i.id.sym.to_string(),
                i.type_ann
                    .as_ref()
                    .map(|t| Binding::Type((*t.type_ann).clone())),
            ),
            Pat::Assign(a) => self.bind_pat(&a.left),
            _ => {}
        }
    }
}

impl Visit for Locals {
    fn visit_var_declarator(&mut self, d: &VarDeclarator) {
        if let Pat::Ident(i) = &d.name {
            let binding = match (&i.type_ann, &d.init) {
                (Some(t), _) => Some(Binding::Type((*t.type_ann).clone())),
                (None, Some(init)) => Some(Binding::Init((**init).clone())),
                (None, None) => None,
            };
            self.bind(i.id.sym.to_string(), binding);
        }
        d.visit_children_with(self);
    }

    fn visit_param(&mut self, p: &swc_ecma_ast::Param) {
        self.bind_pat(&p.pat);
        p.visit_children_with(self);
    }

    fn visit_arrow_expr(&mut self, a: &swc_ecma_ast::ArrowExpr) {
        for p in &a.params {
            self.bind_pat(p);
        }
        a.body.visit_with(self);
    }
}

#[derive(Default)]
struct CallCollector {
    calls: Vec<CallExpr>,
}

impl Visit for CallCollector {
    fn visit_call_expr(&mut self, c: &CallExpr) {
        self.calls.push(c.clone());
        c.visit_children_with(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ParsingContext;

    /// The operations of a project made of `files`, all of them entry files,
    /// with `asyncapi` as the config section.
    pub(crate) fn extract(
        name: &str,
        files: &[(&str, &str)],
        asyncapi: AsyncApi,
    ) -> (Vec<EventOperation>, ComponentSchemas) {
        let dir = std::env::temp_dir().join(format!("nestoa-events-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let parser = ParsingContext::default();
        let mut project = Project::default();
        let mut entries = Vec::new();
        for (rel, body) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
            project.add(parser.parse_file(&path).unwrap());
            entries.push(path);
        }
        let config = Config::default();
        let mut components = ComponentSchemas::default();
        let events = EventExtractor {
            config: &config,
            asyncapi: &asyncapi,
            project: &project,
            entry_files: &entries,
            components: &mut components,
        }
        .extract_all();
        (events, components)
    }

    fn publisher(method: &str) -> Publisher {
        Publisher {
            method: method.into(),
            receiver_types: Vec::new(),
            channel: 0,
            payload: 1,
            payload_property: Some("data".into()),
            type_argument: Some(0),
        }
    }

    const EVENTS: &str = "export enum EventName { BOOK_PUBLISHED = 'book_published' }\n\
        export interface Envelope<T> { id: string; payload: T; ack(): void; }\n\
        export interface EventPayload<P> { data: P; time?: number; }\n\
        export class BookDto { id: string; title: string; }";

    #[test]
    fn handlers_receive_their_pattern_with_the_unwrapped_payload() {
        let controller = "import { EventName, Envelope, EventPayload, BookDto } from './events';\n\
            @Controller()\n\
            export class BookListenerController {\n\
              /** Index a book. */\n\
              @EventPattern(EventName.BOOK_PUBLISHED)\n\
              onPublished(@Payload() message: Envelope<EventPayload<BookDto>>) {}\n\
              @MessagePattern({ role: 'book', cmd: 'count' })\n\
              count(data: { shelf: string }, @Ctx() ctx: unknown) {}\n\
              @EventPattern('book_archived')\n\
              onArchived(@Payload('id') id: string, @Payload('reason') reason?: string) {}\n\
              notAHandler() {}\n\
            }\n\
            export class NotAController {\n\
              @EventPattern('ignored') on(@Payload() p: string) {}\n\
            }";
        let asyncapi = AsyncApi {
            payload_wrappers: vec!["Envelope".into(), "EventPayload".into()],
            ..AsyncApi::default()
        };
        let (events, _) = extract(
            "consumers",
            &[("events.ts", EVENTS), ("books.controller.ts", controller)],
            asyncapi,
        );
        let ops: Vec<(Action, &str, &str, &Value)> = events
            .iter()
            .map(|e| (e.action, e.channel.as_str(), e.handler.as_str(), &e.payload))
            .collect();
        assert_eq!(
            ops,
            [
                (
                    Action::Receive,
                    "book_published",
                    "BookListenerController.onPublished",
                    &json!({ "$ref": "#/components/schemas/BookDto" })
                ),
                (
                    Action::Receive,
                    r#"{"cmd":"count","role":"book"}"#,
                    "BookListenerController.count",
                    &json!({
                        "type": "object",
                        "properties": { "shelf": { "type": "string" } },
                        "required": ["shelf"]
                    })
                ),
                (
                    Action::Receive,
                    "book_archived",
                    "BookListenerController.onArchived",
                    &json!({
                        "type": "object",
                        "properties": { "id": { "type": "string" }, "reason": { "type": "string" } },
                        "required": ["id"]
                    })
                ),
            ]
        );
        assert_eq!(events[0].summary.as_deref(), Some("Index a book."));
        assert_eq!(events[0].tag, "BookListener");
    }

    #[test]
    fn an_unevaluable_pattern_warns_and_is_left_out() {
        crate::diag::take_warnings();
        let controller = "@Controller()\n\
            export class Listener {\n\
              @EventPattern(dynamicName()) on(@Payload() p: string) {}\n\
            }";
        let (events, _) = extract("bad-pattern", &[("a.ts", controller)], AsyncApi::default());
        assert!(events.is_empty());
        assert_eq!(
            crate::diag::take_warnings(),
            ["Listener.on: cannot evaluate its EventPattern pattern; it is left out"]
        );
    }

    #[test]
    fn publish_calls_send_with_the_type_argument_or_the_inferred_payload() {
        crate::diag::take_warnings();
        let service = "import { EventName, BookDto } from './events';\n\
            import { toBook } from './mappers';\n\
            export class EventsClient { publish(topic: string, options: object): void {} }\n\
            export class BooksService {\n\
              constructor(private readonly events: EventsClient, private readonly other: Other) {}\n\
              private label: string;\n\
              create(book: BookDto, count: number) {\n\
                this.events.publish<BookDto>(EventName.BOOK_PUBLISHED, { data: build() });\n\
                const summary = { id: book.id, count, label: this.label, kind: EventName.BOOK_PUBLISHED };\n\
                this.events.publish('book_summary', { data: summary });\n\
                this.events.publish('book_mapped', { data: toBook(book) });\n\
                this.events.publish('book_own', { data: this.own() });\n\
                this.events.publish('book_cast', { data: unknownCall() as BookDto });\n\
                this.events.publish('book_list', { data: [book, book] });\n\
                this.events.publish('book_empty', { data: undefined });\n\
                this.events.publish('book_untyped', { data: unknownCall(book) });\n\
                this.events.publish(dynamicName(), { data: book });\n\
                this.other.publish('not_a_publisher', { data: book });\n\
              }\n\
              own(): Promise<BookDto> { return null as any; }\n\
            }";
        let mappers = "import { BookDto } from './events';\n\
            export function toBook(b: unknown): BookDto;\n\
            export function toBook(b: unknown): BookDto { return b as BookDto; }";
        let asyncapi = AsyncApi {
            publishers: vec![Publisher {
                receiver_types: vec!["EventsClient".into()],
                ..publisher("publish")
            }],
            ..AsyncApi::default()
        };
        let (events, _) = extract(
            "publishers",
            &[
                ("events.ts", EVENTS),
                ("mappers.ts", mappers),
                ("books.service.ts", service),
            ],
            asyncapi,
        );
        let book = json!({ "$ref": "#/components/schemas/BookDto" });
        let ops: Vec<(&str, &Value)> = events
            .iter()
            .inspect(|e| {
                assert_eq!(e.action, Action::Send);
                assert_eq!(e.handler, "BooksService.create");
            })
            .map(|e| (e.channel.as_str(), &e.payload))
            .collect();
        assert_eq!(
            ops,
            [
                ("book_published", &book),
                (
                    "book_summary",
                    &json!({
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "count": { "type": "number" },
                            "label": { "type": "string" },
                            "kind": { "$ref": "#/components/schemas/EventName" }
                        },
                        "required": ["id", "count", "label", "kind"]
                    })
                ),
                ("book_mapped", &book),
                ("book_own", &book),
                ("book_cast", &book),
                ("book_list", &json!({ "type": "array", "items": book })),
                ("book_empty", &Value::Null),
                (
                    "book_untyped",
                    &json!({ "x-unresolved": "unknownCall(book)" })
                ),
            ]
        );
        assert_eq!(
            crate::diag::take_warnings(),
            [
                "BooksService.create: cannot evaluate the channel of a `publish` call; it is left out"
            ]
        );
    }

    #[test]
    fn nest_client_proxy_calls_are_publishers_by_default() {
        let service = "import { BookDto } from './events';\n\
            export class BooksService {\n\
              constructor(@Inject('BROKER') private readonly client: ClientProxy, private bus: EventEmitter) {}\n\
              create(book: BookDto) {\n\
                this.client.emit<void, BookDto>('book_created', book);\n\
                this.client.send('book_count', { shelf: 'a' });\n\
                this.bus.emit('in_process', book);\n\
              }\n\
            }";
        let (events, _) = extract(
            "client-proxy",
            &[("events.ts", EVENTS), ("books.service.ts", service)],
            AsyncApi::default(),
        );
        let ops: Vec<(&str, &Value)> = events
            .iter()
            .map(|e| (e.channel.as_str(), &e.payload))
            .collect();
        assert_eq!(
            ops,
            [
                (
                    "book_created",
                    &json!({ "$ref": "#/components/schemas/BookDto" })
                ),
                (
                    "book_count",
                    &json!({
                        "type": "object",
                        "properties": { "shelf": { "type": "string" } },
                        "required": ["shelf"]
                    })
                ),
            ]
        );
    }
}
