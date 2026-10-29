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
    /// The file of the handler's class.
    pub file: PathBuf,
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
                    self.consumer(&owner.0, file, &handler, &tag, m, out);
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

    /// The operation of a handler declared in `file`, of a class in
    /// `class_file`.
    fn consumer(
        &mut self,
        file: &Path,
        class_file: &Path,
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
            object_pattern(self.project, file, e, 0)
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
                file: class_file.to_path_buf(),
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
            match key {
                Some(key) => {
                    let required_key = Value::String(key.clone());
                    if !matches!(&p.pat, Pat::Ident(i) if i.optional)
                        && !matches!(&p.pat, Pat::Assign(_))
                        && !self.pat_admits_undefined(file, &p.pat)
                        && !required.contains(&required_key)
                    {
                        required.push(required_key);
                    }
                    let schema = self.param_schema(file, &p.pat, false);
                    properties.insert(key, schema);
                }
                None => whole = Some(self.param_schema(file, &p.pat, true)),
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
            Some(p) if p.decorators.is_empty() => self.param_schema(file, &p.pat, true),
            _ => Value::Null,
        }
    }

    /// Whether a parameter's declared type admits `undefined`.
    fn pat_admits_undefined(&mut self, file: &Path, pat: &Pat) -> bool {
        let Pat::Ident(i) = pat else {
            return false;
        };
        i.type_ann
            .as_ref()
            .is_some_and(|t| self.schema_ctx().admits_undefined(file, &t.type_ann, 0))
    }

    fn param_schema(&mut self, file: &Path, pat: &Pat, root: bool) -> Value {
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
            Some(t) => self.type_schema(file, &t.type_ann, root),
            None => json!({ "x-unresolved": "payload" }),
        }
    }

    /// `ty` lowered; at the `root` of a message payload, unwrapped from the
    /// `payloadWrappers` around it.
    fn type_schema(&mut self, file: &Path, ty: &TsType, root: bool) -> Value {
        let ty = if root {
            unwrap_wrappers(ty, &self.asyncapi.payload_wrappers)
        } else {
            ty
        };
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
        let chain = class_chain(self.project, file, class);
        let fields = class_fields(&chain);
        let methods = class_methods(&chain);
        let tag = strip_suffix_ci(class_name, "Controller").to_string();
        for member in &class.body {
            let mut scan = MemberScan::default();
            let (name, documented) = match member {
                ClassMember::Method(m) => {
                    m.function.params.visit_with(&mut scan);
                    m.function.body.visit_with(&mut scan);
                    (prop_name_text(&m.key), true)
                }
                ClassMember::PrivateMethod(m) => {
                    m.function.params.visit_with(&mut scan);
                    m.function.body.visit_with(&mut scan);
                    (format!("#{}", m.key.name), true)
                }
                ClassMember::ClassProp(p) => {
                    p.value.visit_with(&mut scan);
                    (prop_name_text(&p.key), false)
                }
                ClassMember::PrivateProp(p) => {
                    p.value.visit_with(&mut scan);
                    (format!("#{}", p.key.name), false)
                }
                ClassMember::Constructor(c) => {
                    c.params.visit_with(&mut scan);
                    c.body.visit_with(&mut scan);
                    ("constructor".to_string(), true)
                }
                ClassMember::StaticBlock(b) => {
                    b.body.visit_with(&mut scan);
                    ("static".to_string(), false)
                }
                _ => continue,
            };
            let handler = format!("{class_name}.{name}");
            if scan.calls.is_empty() {
                continue;
            }
            let doc = documented
                .then(|| self.project.files.get(file))
                .flatten()
                .and_then(|f| f.docs.jsdoc(member.span().lo))
                .unwrap_or_default();
            let scope = Scope {
                locals: &scan.bindings,
                fields: &fields,
                methods: &methods,
            };
            for call in &scan.calls {
                let Some(publisher) = self.publisher_of(call, &fields) else {
                    continue;
                };
                let Some(arg) = call.args.get(publisher.channel) else {
                    continue;
                };
                let Some(channel) = (!reads_any(&arg.expr, scope.locals))
                    .then(|| {
                        object_pattern(self.project, file, &arg.expr, 0)
                            .or_else(|| crate::constval::string(self.project, file, &arg.expr))
                    })
                    .flatten()
                else {
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
                    file: file.to_path_buf(),
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
        fields: &HashMap<String, Declared>,
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
                Some((_, TsType::TsTypeRef(r))) => Some(entity_last(&r.type_name)),
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
            return self.type_schema(file, ty, true);
        }
        let Some(arg) = call.args.get(publisher.payload) else {
            return Value::Null;
        };
        let expr = match &publisher.payload_property {
            None => (*arg.expr).clone(),
            Some(key) => match crate::constval::unwrap(&arg.expr) {
                // A spread may supply or override the property.
                Expr::Object(o) if o.props.iter().any(|p| matches!(p, PropOrSpread::Spread(_))) => {
                    return self.unresolved_expr(file, &arg.expr);
                }
                Expr::Object(o) => match object_member(o, key) {
                    Some(e) => e,
                    None => return Value::Null,
                },
                other => return self.unresolved_expr(file, other),
            },
        };
        self.expr_schema(file, &expr, scope, 0, true)
    }

    /// The type TypeScript infers for `expr`, as far as declared types reach:
    /// literals, `as` assertions, `new` expressions, locals, parameters and
    /// fields with a type, and object and array literals of those. `root`: the
    /// expression is the whole message payload.
    fn expr_schema(
        &mut self,
        file: &Path,
        expr: &Expr,
        scope: &Scope,
        depth: usize,
        root: bool,
    ) -> Value {
        if depth > MAX_INFERENCE_DEPTH {
            return self.unresolved_expr(file, expr);
        }
        match expr {
            Expr::Paren(p) => self.expr_schema(file, &p.expr, scope, depth, root),
            Expr::Await(a) => self.expr_schema(file, &a.arg, scope, depth, root),
            Expr::TsNonNull(n) => self.expr_schema(file, &n.expr, scope, depth, root),
            Expr::TsSatisfies(s) => self.expr_schema(file, &s.expr, scope, depth, root),
            Expr::TsAs(a) => self.type_schema(file, &a.type_ann, root),
            Expr::TsTypeAssertion(a) => self.type_schema(file, &a.type_ann, root),
            Expr::Lit(Lit::Null(_)) => json!({ "type": "null" }),
            Expr::Lit(_) | Expr::Tpl(_) | Expr::Unary(_) => {
                self.schema_ctx().initializer_schema(file, expr)
            }
            Expr::Ident(i) if i.sym == "undefined" => Value::Null,
            Expr::Ident(i) => match scope.locals.get(i.sym.as_ref()) {
                Some(Some(Binding::Type(ty))) => self.type_schema(file, ty, root),
                Some(Some(Binding::Init(init))) => {
                    self.expr_schema(file, init, scope, depth + 1, root)
                }
                _ => self.unresolved_expr(file, expr),
            },
            Expr::Member(m) if matches!(&*m.obj, Expr::This(_)) => {
                let ty = match &m.prop {
                    MemberProp::Ident(name) => scope.fields.get(name.sym.as_ref()),
                    _ => None,
                };
                match ty {
                    Some(Some((decl_file, ty))) => self.type_schema(decl_file, ty, root),
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
                if let Some(Some((decl_file, ty))) = method {
                    return self.type_schema(decl_file, ty, root);
                }
                match self.function_return(file, &c.callee, scope) {
                    Some((decl_file, ty)) => self.type_schema(&decl_file, &ty, root),
                    None => self.unresolved_expr(file, expr),
                }
            }
            Expr::Object(o) => self.object_schema(file, o, scope, depth),
            Expr::Array(a) => {
                let mut items: Vec<Value> = Vec::new();
                for el in &a.elems {
                    let schema = match el {
                        Some(el) if el.spread.is_some() => return self.unresolved_expr(file, expr),
                        Some(el) => self.expr_schema(file, &el.expr, scope, depth + 1, false),
                        None => Value::Null,
                    };
                    // JSON writes a hole or `undefined` element as `null`.
                    let schema = if schema.is_null() {
                        json!({ "type": "null" })
                    } else {
                        schema
                    };
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
                    let schema = self.expr_schema(file, e, scope, depth + 1, root);
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
        let object = self.expr_schema(file, obj, scope, depth + 1, false);
        key.and_then(|k| self.components.property(&object, &k))
            .unwrap_or_else(|| self.unresolved_expr(file, expr))
    }

    /// An object literal, its properties in source order: a later property
    /// or spread replaces an earlier one of the same name.
    fn object_schema(
        &mut self,
        file: &Path,
        o: &swc_ecma_ast::ObjectLit,
        scope: &Scope,
        depth: usize,
    ) -> Value {
        // `{ ...x }` copies an object `x`, whatever its shape.
        if let [PropOrSpread::Spread(s)] = o.props.as_slice() {
            let spread = self.expr_schema(file, &s.expr, scope, depth + 1, false);
            if spread.is_null() {
                return json!({ "type": "object" });
            }
            let objects = match spread.get("anyOf").and_then(Value::as_array) {
                Some(branches) => branches
                    .iter()
                    .all(|b| self.components.flatten(b).is_some()),
                None => self.components.flatten(&spread).is_some(),
            };
            return if objects {
                spread
            } else {
                self.unresolved_expr(file, &Expr::Object(o.clone()))
            };
        }
        let mut properties = Map::new();
        let mut required: Vec<String> = Vec::new();
        for prop in &o.props {
            let (key, schema, absent) = match prop {
                PropOrSpread::Spread(s) => {
                    let spread = self.expr_schema(file, &s.expr, scope, depth + 1, false);
                    if spread.is_null() {
                        continue;
                    }
                    let Some(spread) = self.components.flatten(&spread) else {
                        return self.unresolved_expr(file, &Expr::Object(o.clone()));
                    };
                    let spread_required: Vec<&str> = spread
                        .get("required")
                        .and_then(Value::as_array)
                        .map(|r| r.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    if let Some(Value::Object(props)) = spread.get("properties") {
                        for (key, schema) in props {
                            if spread_required.contains(&key.as_str()) {
                                properties.insert(key.clone(), schema.clone());
                                if !required.contains(key) {
                                    required.push(key.clone());
                                }
                                continue;
                            }
                            // An optional property may leave the earlier one.
                            let merged = match properties.get(key) {
                                Some(earlier) if earlier != schema => {
                                    json!({ "anyOf": [earlier, schema] })
                                }
                                _ => schema.clone(),
                            };
                            properties.insert(key.clone(), merged);
                        }
                    }
                    continue;
                }
                PropOrSpread::Prop(p) => match &**p {
                    Prop::KeyValue(kv) => (
                        prop_name_text(&kv.key),
                        self.expr_schema(file, &kv.value, scope, depth + 1, false),
                        self.may_be_undefined(file, &kv.value, scope, depth + 1),
                    ),
                    Prop::Shorthand(id) => {
                        let value = Expr::Ident(id.clone());
                        (
                            id.sym.to_string(),
                            self.expr_schema(file, &value, scope, depth + 1, false),
                            self.may_be_undefined(file, &value, scope, depth + 1),
                        )
                    }
                    _ => continue,
                },
            };
            if key.is_empty() {
                continue;
            }
            required.retain(|k| *k != key);
            if schema.is_null() {
                properties.shift_remove(&key);
                continue;
            }
            if !absent {
                required.push(key.clone());
            }
            properties.insert(key, schema);
        }
        // Required in the order the properties come.
        required.sort_by_key(|k| properties.keys().position(|p| p == k));
        let mut object = Map::new();
        object.insert("type".into(), json!("object"));
        if !properties.is_empty() {
            object.insert("properties".into(), Value::Object(properties));
        }
        if !required.is_empty() {
            object.insert("required".into(), json!(required));
        }
        Value::Object(object)
    }

    /// Whether `expr` may evaluate to `undefined`, which leaves the property
    /// holding it out of the message.
    fn may_be_undefined(&mut self, file: &Path, expr: &Expr, scope: &Scope, depth: usize) -> bool {
        if depth > MAX_INFERENCE_DEPTH {
            return false;
        }
        match expr {
            Expr::Paren(p) => self.may_be_undefined(file, &p.expr, scope, depth),
            Expr::Await(a) => self.may_be_undefined(file, &a.arg, scope, depth),
            Expr::TsSatisfies(s) => self.may_be_undefined(file, &s.expr, scope, depth),
            Expr::TsAs(a) => self.schema_ctx().admits_undefined(file, &a.type_ann, 0),
            Expr::Ident(i) if i.sym == "undefined" => true,
            Expr::Ident(i) => match scope.locals.get(i.sym.as_ref()) {
                Some(Some(Binding::Type(ty))) => self.schema_ctx().admits_undefined(file, ty, 0),
                Some(Some(Binding::Init(init))) => {
                    self.may_be_undefined(file, init, scope, depth + 1)
                }
                _ => false,
            },
            Expr::Cond(c) => {
                self.may_be_undefined(file, &c.cons, scope, depth + 1)
                    || self.may_be_undefined(file, &c.alt, scope, depth + 1)
            }
            Expr::OptChain(_) => true,
            Expr::Member(m) => match (&*m.obj, &m.prop) {
                (Expr::This(_), MemberProp::Ident(name)) => {
                    match scope.fields.get(name.sym.as_ref()) {
                        Some(Some((decl_file, ty))) => {
                            self.schema_ctx().admits_undefined(decl_file, ty, 0)
                        }
                        _ => false,
                    }
                }
                (obj, MemberProp::Ident(name)) => {
                    let object = self.expr_schema(file, obj, scope, depth + 1, false);
                    self.components.flatten(&object).is_some_and(|o| {
                        o.get("properties")
                            .is_some_and(|p| p.get(name.sym.as_ref()).is_some())
                            && !o
                                .get("required")
                                .and_then(Value::as_array)
                                .is_some_and(|r| r.iter().any(|k| k == name.sym.as_ref()))
                    })
                }
                _ => false,
            },
            _ => false,
        }
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
fn object_pattern(project: &Project, file: &Path, expr: &Expr, depth: usize) -> Option<String> {
    const MAX_DEPTH: usize = 16;
    if depth > MAX_DEPTH {
        return None;
    }
    let o = match crate::constval::unwrap(expr) {
        Expr::Object(o) => o,
        Expr::Ident(i) => {
            let (decl_file, c) = project.resolve_const(file, &i.sym)?;
            return object_pattern(project, &decl_file, c.init(), depth + 1);
        }
        _ => return None,
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
        let value = match object_pattern(project, file, &kv.value, depth + 1) {
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

/// Whether `expr` reads any of `locals`, which shadow the module's names.
fn reads_any(expr: &Expr, locals: &HashMap<String, Option<Binding>>) -> bool {
    struct Reads<'l>(&'l HashMap<String, Option<Binding>>, bool);
    impl Visit for Reads<'_> {
        fn visit_ident(&mut self, i: &swc_ecma_ast::Ident) {
            self.1 |= self.0.contains_key(i.sym.as_ref());
        }
    }
    let mut reads = Reads(locals, false);
    expr.visit_with(&mut reads);
    reads.1
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

/// The type of a binding declared `ty`, `T | undefined` when declared with
/// `?`: it lowers to the same schema, and admits `undefined`.
fn declared_type(ty: &TsType, optional: bool) -> TsType {
    use swc_ecma_ast::{TsKeywordType, TsKeywordTypeKind, TsUnionOrIntersectionType, TsUnionType};
    if !optional {
        return ty.clone();
    }
    TsType::TsUnionOrIntersectionType(TsUnionOrIntersectionType::TsUnionType(TsUnionType {
        span: ty.span(),
        types: vec![
            Box::new(ty.clone()),
            Box::new(TsType::TsKeywordType(TsKeywordType {
                span: ty.span(),
                kind: TsKeywordTypeKind::TsUndefinedKeyword,
            })),
        ],
    }))
}

/// A declared type, with the file it is written in.
type Declared = Option<(PathBuf, TsType)>;

/// The class and the classes it extends, the class first, with their files.
fn class_chain(project: &Project, file: &Path, class: &Class) -> Vec<(PathBuf, Class)> {
    let mut chain = vec![(file.to_path_buf(), class.clone())];
    let mut walked = std::collections::HashSet::new();
    walked.insert((file.to_path_buf(), class.span.lo));
    while let Some((file, class)) = chain.last() {
        let Some((base_file, base)) = class
            .super_class
            .as_deref()
            .and_then(crate::constval::dotted_name)
            .and_then(|n| project_class(project, file, &n))
        else {
            break;
        };
        if !walked.insert((base_file.clone(), base.span.lo)) {
            break;
        }
        chain.push((base_file, (*base).clone()));
    }
    chain
}

/// The declared return type of each method of a class, inherited ones
/// included. A method whose callable signatures return different types has
/// none: which one a call picks is not known.
fn class_methods(chain: &[(PathBuf, Class)]) -> HashMap<String, Declared> {
    let mut methods: HashMap<String, Declared> = HashMap::new();
    for (file, class) in chain {
        let mut own: HashMap<String, Vec<&Function>> = HashMap::new();
        for member in &class.body {
            if let ClassMember::Method(m) = member {
                own.entry(prop_name_text(&m.key))
                    .or_default()
                    .push(&m.function);
            }
        }
        for (name, signatures) in own {
            methods.entry(name).or_insert_with(|| {
                let signatures = signatures
                    .iter()
                    .map(|f| (f.body.is_some(), f.return_type.as_deref()));
                callable_return(signatures).map(|t| (file.clone(), t.clone()))
            });
        }
    }
    methods
}

/// The return type every callable signature of an overloaded function
/// declares, given each signature's (has a body, return type). With
/// overloads, the implementation is not callable.
pub(crate) fn callable_return<'t>(
    signatures: impl Iterator<Item = (bool, Option<&'t swc_ecma_ast::TsTypeAnn>)>,
) -> Option<&'t TsType> {
    use swc_common::EqIgnoreSpan;
    let signatures: Vec<_> = signatures.collect();
    let overloaded = signatures.iter().any(|(body, _)| !body);
    let mut callable = signatures
        .iter()
        .filter(|(body, _)| !overloaded || !body)
        .map(|(_, ty)| ty.map(|t| &*t.type_ann));
    let first = callable.next()??;
    callable
        .all(|t| t.is_some_and(|t| t.eq_ignore_span(first)))
        .then_some(first)
}

/// The declared type of each field of a class, inherited ones and
/// constructor parameter properties included; `None` for a field declared
/// without one.
fn class_fields(chain: &[(PathBuf, Class)]) -> HashMap<String, Declared> {
    let mut fields = HashMap::new();
    for (file, class) in chain {
        let declared = |t: Option<&swc_ecma_ast::TsTypeAnn>, optional: bool| {
            t.map(|t| (file.clone(), declared_type(&t.type_ann, optional)))
        };
        for member in &class.body {
            match member {
                ClassMember::ClassProp(p) => {
                    fields
                        .entry(prop_name_text(&p.key))
                        .or_insert_with(|| declared(p.type_ann.as_deref(), p.is_optional));
                }
                ClassMember::Constructor(c) => {
                    for p in &c.params {
                        if let ParamOrTsParamProp::TsParamProp(p) = p
                            && let TsParamPropParam::Ident(i) = &p.param
                        {
                            fields
                                .entry(i.id.sym.to_string())
                                .or_insert_with(|| declared(i.type_ann.as_deref(), i.id.optional));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    fields
}

struct Scope<'s> {
    /// `None` for a name bound more than once in the member.
    locals: &'s HashMap<String, Option<Binding>>,
    fields: &'s HashMap<String, Declared>,
    /// The declared return type of each method of the class.
    methods: &'s HashMap<String, Declared>,
}

#[derive(Clone)]
enum Binding {
    Type(TsType),
    Init(Expr),
}

/// The calls in a member, and the names it binds — parameters and
/// variables, of nested functions too — with what they are declared with.
/// Scopes are not told apart: a name bound twice is ambiguous.
#[derive(Default)]
struct MemberScan {
    calls: Vec<CallExpr>,
    bindings: HashMap<String, Option<Binding>>,
}

impl MemberScan {
    fn declarator(&mut self, d: &VarDeclarator, constant: bool) {
        if let Pat::Ident(i) = &d.name {
            let binding = match (&i.type_ann, &d.init) {
                (Some(t), _) => Some(Binding::Type((*t.type_ann).clone())),
                (None, Some(init)) if constant => Some(Binding::Init((**init).clone())),
                _ => None,
            };
            self.bind(i.id.sym.to_string(), binding);
        }
    }

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
                    .map(|t| Binding::Type(declared_type(&t.type_ann, i.id.optional))),
            ),
            Pat::Assign(a) => self.bind_pat(&a.left),
            _ => {}
        }
    }
}

impl Visit for MemberScan {
    fn visit_call_expr(&mut self, c: &CallExpr) {
        self.calls.push(c.clone());
        c.visit_children_with(self);
    }

    fn visit_var_decl(&mut self, v: &swc_ecma_ast::VarDecl) {
        // A `let` or `var` may be reassigned: its initializer isn't its value.
        let constant = v.kind == swc_ecma_ast::VarDeclKind::Const;
        for d in &v.decls {
            self.declarator(d, constant);
        }
        v.visit_children_with(self);
    }

    fn visit_param(&mut self, p: &swc_ecma_ast::Param) {
        self.bind_pat(&p.pat);
        p.visit_children_with(self);
    }

    /// A constructor parameter property is a parameter too.
    fn visit_ts_param_prop(&mut self, p: &swc_ecma_ast::TsParamProp) {
        match &p.param {
            TsParamPropParam::Ident(i) => self.bind_pat(&Pat::Ident(i.clone())),
            TsParamPropParam::Assign(a) => self.bind_pat(&a.left),
        }
        p.visit_children_with(self);
    }

    fn visit_arrow_expr(&mut self, a: &swc_ecma_ast::ArrowExpr) {
        for p in &a.params {
            self.bind_pat(p);
        }
        a.visit_children_with(self);
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
              onArchived(@Payload('id') id: string, @Payload('reason') reason?: string, @Payload('by') by: string = 'staff') {}\n\
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
                        "properties": {
                            "id": { "type": "string" },
                            "reason": { "type": "string" },
                            "by": { "type": "string" }
                        },
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
                this.client.send({ cmd: 'sum' }, [1, 2]);\n\
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
                (
                    r#"{"cmd":"sum"}"#,
                    &json!({ "type": "array", "items": { "type": "number" } })
                ),
            ]
        );
    }

    #[test]
    fn a_payload_key_read_twice_is_required_once() {
        let controller = "@Controller()\n\
            export class Listener {\n\
              @EventPattern('book') on(@Payload('id') a: string, @Payload('id') b: string) {}\n\
            }";
        let (events, _) = extract("twice", &[("a.ts", controller)], AsyncApi::default());
        assert_eq!(events[0].payload["required"], json!(["id"]));
    }

    #[test]
    fn wrappers_are_only_unwrapped_at_the_payload_root() {
        let service = "import { Envelope, BookDto } from './events';\n\
            export class Base { constructor(protected client: ClientProxy) {} }\n\
            export class BooksService extends Base {\n\
              send(envelope: Envelope<BookDto>) {\n\
                this.client.emit('whole', envelope);\n\
                this.client.emit('nested', { nested: envelope });\n\
              }\n\
            }";
        let asyncapi = AsyncApi {
            payload_wrappers: vec!["Envelope".into()],
            ..AsyncApi::default()
        };
        let (events, _) = extract(
            "root-wrappers",
            &[("events.ts", EVENTS), ("books.service.ts", service)],
            asyncapi,
        );
        let payloads: Vec<(&str, &Value)> = events
            .iter()
            .map(|e| (e.channel.as_str(), &e.payload))
            .collect();
        assert_eq!(
            payloads,
            [
                ("whole", &json!({ "$ref": "#/components/schemas/BookDto" })),
                (
                    "nested",
                    &json!({
                        "type": "object",
                        "properties": { "nested": { "$ref": "#/components/schemas/EnvelopeBookDto" } },
                        "required": ["nested"]
                    })
                ),
            ]
        );
    }

    #[test]
    fn every_member_kind_publishes_and_inherited_fields_type_the_receiver() {
        let base = "export class Base { constructor(protected readonly client: ClientProxy) {} }";
        let service = "import { Base } from './base';\n\
            import { BookDto } from './events';\n\
            export class BooksService extends Base {\n\
              #publish = (book: BookDto) => this.client.emit('private_prop', book);\n\
              static { this.client.emit('static_block', 1); }\n\
            }\n\
            export class Seeder {\n\
              constructor(private book: BookDto, private client: ClientProxy) {\n\
                this.client.emit('constructor', book);\n\
              }\n\
            }";
        let (events, _) = extract(
            "members",
            &[
                ("events.ts", EVENTS),
                ("base.ts", base),
                ("books.service.ts", service),
            ],
            AsyncApi::default(),
        );
        let book = json!({ "$ref": "#/components/schemas/BookDto" });
        let ops: Vec<(&str, &str, &Value)> = events
            .iter()
            .map(|e| (e.handler.as_str(), e.channel.as_str(), &e.payload))
            .collect();
        assert_eq!(
            ops,
            [
                ("BooksService.#publish", "private_prop", &book),
                (
                    "BooksService.static",
                    "static_block",
                    &json!({ "type": "number" })
                ),
                ("Seeder.constructor", "constructor", &book),
            ]
        );
    }

    #[test]
    fn literal_payloads_follow_javascript_semantics() {
        let service = "import { BookDto } from './events';\n\
            export function pick(x: string): BookDto;\n\
            export function pick(x: number): number;\n\
            export function pick(x: unknown): unknown { return x; }\n\
            export class BooksService {\n\
              constructor(private client: ClientProxy) {}\n\
              private either: BookDto | { extra: string };\n\
              private many: BookDto[];\n\
              send(book: BookDto) {\n\
                this.client.emit('array', [1, undefined, , 2]);\n\
                this.client.emit('spread', { id: 1, ...book, title: 2, extra: undefined });\n\
                this.client.emit('copy', { ...this.either });\n\
                this.client.emit('copy_array', { ...this.many });\n\
                this.client.emit('copy_undefined', { ...undefined });\n\
                this.client.emit('overloaded', pick(1));\n\
                this.client.emit('method', this.own(1));\n\
              }\n\
              own(x: string): string;\n\
              own(x: number): number;\n\
              own(x: unknown): unknown { return x; }\n\
            }";
        let (events, _) = extract(
            "literals",
            &[("events.ts", EVENTS), ("books.service.ts", service)],
            AsyncApi::default(),
        );
        let payloads: Vec<&Value> = events.iter().map(|e| &e.payload).collect();
        assert_eq!(
            payloads,
            [
                &json!({
                    "type": "array",
                    "items": { "anyOf": [{ "type": "number" }, { "type": "null" }] }
                }),
                &json!({
                    "type": "object",
                    "properties": { "id": { "type": "string" }, "title": { "type": "number" } },
                    "required": ["id", "title"]
                }),
                &json!({ "anyOf": [
                    { "$ref": "#/components/schemas/BookDto" },
                    {
                        "type": "object",
                        "properties": { "extra": { "type": "string" } },
                        "required": ["extra"]
                    }
                ] }),
                &json!({ "x-unresolved": "{ ...this.many }" }),
                &json!({ "type": "object" }),
                &json!({ "x-unresolved": "pick(1)" }),
                &json!({ "x-unresolved": "this.own(1)" }),
            ]
        );
    }

    #[test]
    fn properties_that_may_be_undefined_are_optional() {
        crate::diag::take_warnings();
        let controller = "@Controller()\n\
            export class Listener {\n\
              @EventPattern(PATTERN) on(@Payload('id') id: string, @Payload('note') note: string | undefined) {}\n\
            }\n\
            const PATTERN = { cmd: 'note' };";
        let service = "export class Draft { subtitle?: string; title: string; count?: string; }\n\
            const TOPIC = 'books';\n\
            export class BooksService {\n\
              constructor(private client: ClientProxy, private events: EventsClient, private tag?: string) {}\n\
              private label?: string;\n\
              send(draft: Draft, maybe: string | undefined, note?: string) {\n\
                this.client.emit('optional', { subtitle: draft.subtitle, title: draft.title, maybe, note, label: this.label, tag: this.tag });\n\
                this.client.emit('fallback', { subtitle: 'none', count: 1, ...draft });\n\
                let later = undefined;\n\
                later = draft;\n\
                this.client.emit('reassigned', later);\n\
                this.events.publish('spread', { data: draft, ...{ data: 2 } });\n\
                this.client.send(PATTERN, 1);\n\
              }\n\
              shadowed(TOPIC: string, draft: Draft) {\n\
                this.client.emit(TOPIC, draft);\n\
              }\n\
            }\n\
            const PATTERN = { cmd: 'note' };";
        let asyncapi = AsyncApi {
            publishers: [publisher("publish")]
                .into_iter()
                .chain(AsyncApi::default().publishers)
                .collect(),
            ..AsyncApi::default()
        };
        let (events, _) = extract(
            "optional",
            &[("listener.ts", controller), ("books.service.ts", service)],
            asyncapi,
        );
        let ops: Vec<(&str, &Value)> = events
            .iter()
            .map(|e| (e.channel.as_str(), &e.payload))
            .collect();
        assert_eq!(
            ops,
            [
                (
                    r#"{"cmd":"note"}"#,
                    &json!({
                        "type": "object",
                        "properties": { "id": { "type": "string" }, "note": { "type": "string" } },
                        "required": ["id"]
                    })
                ),
                (
                    "optional",
                    &json!({
                        "type": "object",
                        "properties": {
                            "subtitle": { "type": "string" },
                            "title": { "type": "string" },
                            "maybe": { "type": "string" },
                            "note": { "type": "string" },
                            "label": { "type": "string" },
                            "tag": { "type": "string" }
                        },
                        "required": ["title"]
                    })
                ),
                (
                    "fallback",
                    &json!({
                        "type": "object",
                        "properties": {
                            "subtitle": { "type": "string" },
                            "count": { "anyOf": [{ "type": "number" }, { "type": "string" }] },
                            "title": { "type": "string" }
                        },
                        "required": ["subtitle", "count", "title"]
                    })
                ),
                ("reassigned", &json!({ "x-unresolved": "later" })),
                (
                    "spread",
                    &json!({ "x-unresolved": "{ data: draft, ...{ data: 2 } }" })
                ),
                (r#"{"cmd":"note"}"#, &json!({ "type": "number" })),
            ]
        );
        assert_eq!(
            crate::diag::take_warnings(),
            ["BooksService.shadowed: cannot evaluate the channel of a `emit` call; it is left out"]
        );
    }
}
