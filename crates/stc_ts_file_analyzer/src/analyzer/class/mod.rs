use std::{borrow::Cow, cell::RefCell, mem::take};

use itertools::Itertools;
use rnode::{FoldWith, IntoRNode, NodeId, NodeIdGenerator, VisitWith};
use stc_arc_cow::ArcCow;
use stc_ts_ast_rnode::{
    RAssignPat, RAutoAccessor, RBindingIdent, RClass, RClassDecl, RClassExpr, RClassMember, RClassMethod, RClassProp, RConstructor, RDecl,
    RExpr, RFunction, RIdent, RKey, RMemberExpr, RParam, RParamOrTsParamProp, RPat, RPrivateMethod, RPrivateProp, RPropName, RStaticBlock,
    RStmt, RTsEntityName, RTsFnParam, RTsParamProp, RTsParamPropParam, RTsTypeAliasDecl, RTsTypeAnn, RVarDecl, RVarDeclarator,
};
use stc_ts_env::ModuleConfig;
use stc_ts_errors::{DebugExt, ErrorKind, Errors};
use stc_ts_simple_ast_validations::constructor::ConstructorSuperCallFinder;
use stc_ts_type_ops::generalization::{prevent_generalize, LitGeneralizer};
use stc_ts_types::{
    rprop_name_to_expr, Accessor, Class, ClassDef, ClassMember, ClassMetadata, ClassProperty, ConstructorSignature, FnParam, Id, IdCtx,
    Intersection, Key, KeywordType, Method, OperatorMetadata, QueryExpr, QueryType, QueryTypeMetadata, Ref, TsExpr, Type, Unique,
};
use stc_ts_utils::find_ids_in_pat;
use stc_utils::{cache::Freeze, FxHashSet};
use swc_atoms::js_word;
use swc_common::{iter::IdentifyLast, EqIgnoreSpan, Span, Spanned, SyntaxContext, TypeEq, DUMMY_SP};
use swc_ecma_ast::*;
use swc_ecma_utils::private_ident;

use self::type_param::StaticTypeParamValidator;
use super::{expr::AccessPropertyOpts, pat::PatMode};
use crate::{
    analyzer::{
        assign::AssignOpts,
        expr::TypeOfMode,
        props::ComputedPropMode,
        scope::VarKind,
        util::{is_prop_name_eq, ResultExt, VarVisitor},
        Analyzer, Ctx, ScopeKind,
    },
    ty::TypeExt,
    validator,
    validator::ValidateWith,
    VResult,
};

mod order;
mod type_param;

#[derive(Debug, Default)]
pub(crate) struct ClassState {
    /// Used only while validating constructors.
    ///
    /// `false` means `this` can be used.
    pub need_super_call: RefCell<bool>,
}

impl Analyzer<'_, '_> {
    fn validate_type_of_class_property(
        &mut self,
        span: Span,
        readonly: bool,
        is_static: bool,
        type_ann: &Option<Box<RTsTypeAnn>>,
        value: &Option<Box<RExpr>>,
    ) -> VResult<Option<Type>> {
        let mut ty = try_opt!(type_ann.validate_with(self));
        let mut value_ty = {
            let ctx = Ctx {
                in_static_property_initializer: is_static,
                ..self.ctx
            };
            try_opt!(value.validate_with_args(&mut *self.with_ctx(ctx), (TypeOfMode::RValue, None, ty.as_ref())))
        };

        if !self.config.is_builtin {
            // Disabled because of false positives when the constructor initializes the
            // field.
            #[allow(clippy::overly_complex_bool_expr)]
            if false && self.rule().strict_null_checks {
                if value.is_none() {
                    if let Some(ty) = &ty {
                        if self
                            .assign_with_opts(
                                &mut Default::default(),
                                ty,
                                &Type::Keyword(KeywordType {
                                    span,
                                    kind: TsKeywordTypeKind::TsUndefinedKeyword,
                                    metadata: Default::default(),
                                    tracker: Default::default(),
                                }),
                                AssignOpts {
                                    span,
                                    ..Default::default()
                                },
                            )
                            .is_err()
                        {
                            self.storage.report(ErrorKind::ClassPropNotInitialized { span }.into())
                        }
                    }
                }
            }

            // Report error if type is not found.
            if let Some(ty) = &ty {
                self.normalize(Some(span), Cow::Borrowed(ty), Default::default())
                    .report(&mut self.storage);
            }

            // Report error if type is not found.
            if let Some(ty) = &value_ty {
                self.normalize(Some(span), Cow::Borrowed(ty), Default::default())
                    .report(&mut self.storage);
            }
        }

        if readonly {
            if let Some(ty) = &mut ty {
                prevent_generalize(ty);
            }
            if let Some(ty) = &mut value_ty {
                prevent_generalize(ty);
            }
        }

        Ok(ty.or(value_ty).map(|ty| match ty {
            Type::Symbol(..) if readonly && is_static => Type::Unique(Unique {
                span: ty.span(),
                ty: Box::new(Type::Keyword(KeywordType {
                    span,
                    kind: TsKeywordTypeKind::TsSymbolKeyword,
                    metadata: Default::default(),
                    tracker: Default::default(),
                })),
                metadata: OperatorMetadata { common: ty.metadata() },
                tracker: Default::default(),
            }),
            _ => ty,
        }))
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, p: &RClassProp, object_type: Option<&Type>) -> VResult<ClassProperty> {
        let marks = self.marks();

        if p.is_static {
            if let RPropName::Ident(i) = &p.key {
                if &*i.sym == "prototype" {
                    self.storage
                        .report(ErrorKind::StaticPropertyCannotBeNamedPrototype { span: i.span }.into())
                }
            }
        }

        // Verify key if key is computed
        if let RPropName::Computed(p) = &p.key {
            self.validate_computed_prop_key(p.span, &p.expr).report(&mut self.storage);
        }

        let key = self.validate_key(&rprop_name_to_expr(p.key.clone()), matches!(p.key, RPropName::Computed(..)))?;

        if let Some(value) = &p.value {
            if let Some(object_type) = object_type {
                if let Ok(mut type_ann) = self.access_property(
                    p.span,
                    object_type,
                    &key,
                    TypeOfMode::RValue,
                    IdCtx::Var,
                    AccessPropertyOpts {
                        disallow_creating_indexed_type_from_ty_els: true,
                        ..Default::default()
                    },
                ) {
                    type_ann.freeze();
                    self.apply_type_ann_to_expr(value, &type_ann)?;
                }
            }
        }

        let value = self
            .validate_type_of_class_property(p.span, p.readonly, p.is_static, &p.type_ann, &p.value)?
            .map(Box::new)
            .freezed();

        if p.is_static {
            value.visit_with(&mut StaticTypeParamValidator {
                span: p.span,
                analyzer: self,
            });
        }

        match p.accessibility {
            Some(Accessibility::Private) => {}
            _ => {
                if p.type_ann.is_none() {
                    if let Some(m) = &mut self.mutations {
                        m.for_class_props.entry(p.node_id).or_default().ty = value.clone().map(|ty| ty.generalize_lit());
                    }
                }
            }
        }

        Ok(ClassProperty {
            span: p.span,
            key,
            value,
            is_static: p.is_static,
            accessibility: p.accessibility,
            is_abstract: p.is_abstract,
            is_optional: p.is_optional,
            readonly: p.readonly,
            definite: p.definite,
            accessor: Default::default(),
        })
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, p: &RKey) -> VResult<Key> {
        match p {
            RKey::Private(p) => Ok(Key::Private(p.validate_with(self)?)),
            RKey::Public(key) => Ok(key.validate_with(self)?),
        }
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, p: &RAutoAccessor, object_type: Option<&Type>) -> VResult<ClassProperty> {
        let marks = self.marks();

        let key = p.key.validate_with(self)?;

        if let Some(value) = &p.value {
            if let Some(object_type) = object_type {
                if let Ok(mut type_ann) = self.access_property(
                    p.span,
                    object_type,
                    &key,
                    TypeOfMode::RValue,
                    IdCtx::Var,
                    AccessPropertyOpts {
                        disallow_creating_indexed_type_from_ty_els: true,
                        ..Default::default()
                    },
                ) {
                    type_ann.freeze();
                    self.apply_type_ann_to_expr(value, &type_ann)?;
                }
            }
        }

        let value = self
            .validate_type_of_class_property(p.span, false, p.is_static, &p.type_ann, &p.value)?
            .map(Box::new)
            .freezed();

        if p.is_static {
            value.visit_with(&mut StaticTypeParamValidator {
                span: p.span,
                analyzer: self,
            });
        }

        match p.accessibility {
            Some(Accessibility::Private) => {}
            _ => {
                if p.type_ann.is_none() {
                    if let Some(m) = &mut self.mutations {
                        m.for_class_props.entry(p.node_id).or_default().ty = value.clone().map(|ty| ty.generalize_lit());
                    }
                }
            }
        }

        Ok(ClassProperty {
            span: p.span,
            key,
            value,
            is_static: p.is_static,
            accessibility: p.accessibility,
            is_abstract: false,
            is_optional: false,
            readonly: false,
            definite: false,
            accessor: Default::default(),
        })
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, p: &RPrivateProp) -> VResult<ClassProperty> {
        if let js_word!("constructor") = p.key.id.sym {
            self.storage.report(ErrorKind::ConstructorIsKeyword { span: p.key.id.span }.into());
        }

        let key = Key::Private(p.key.clone().into());

        let value = self
            .validate_type_of_class_property(p.span, p.readonly, p.is_static, &p.type_ann, &p.value)?
            .map(Box::new);

        if !self.ctx.in_declare && self.rule().no_implicit_any {
            if value.is_none() {
                self.storage
                    .report(ErrorKind::ImplicitAny { span: key.span() }.context("private class property"))
            }
        }

        Ok(ClassProperty {
            span: p.span,
            key,
            value,
            is_static: p.is_static,
            accessibility: p.accessibility,
            is_abstract: false,
            is_optional: p.is_optional,
            readonly: p.readonly,
            definite: p.definite,
            accessor: Default::default(),
        })
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, c: &RConstructor, super_class: Option<&Type>) -> VResult<ConstructorSignature> {
        let c_span = c.span();

        if !self.config.is_builtin
            && !self.ctx.ignore_errors
            && self.ctx.in_class_with_super
            && c.body.is_some()
            && !matches!(
                super_class.map(Type::normalize),
                Some(Type::Keyword(KeywordType {
                    kind: TsKeywordTypeKind::TsNullKeyword | TsKeywordTypeKind::TsUndefinedKeyword,
                    ..
                }))
            )
        {
            let mut v = ConstructorSuperCallFinder::default();
            c.visit_with(&mut v);
            if !v.has_valid_super_call {
                self.storage.report(ErrorKind::SuperNotCalled { span: c.span }.into());
            } else {
                debug_assert_eq!(self.scope.kind(), ScopeKind::Class);
                *self.scope.class.need_super_call.borrow_mut() = true;
            }

            for span in v.nested_super_calls {
                self.storage.report(ErrorKind::SuperInNestedFunction { span }.into())
            }
        }

        let ctx = Ctx {
            in_declare: self.ctx.in_declare || c.body.is_none(),
            allow_new_target: true,
            ..self.ctx
        };
        self.with_ctx(ctx)
            .with_child(ScopeKind::Constructor, Default::default(), |child: &mut Analyzer| {
                let RConstructor { params, body, .. } = c;

                {
                    // Validate params
                    // TODO(kdy1): Move this to parser
                    let mut has_optional = false;
                    for p in params.iter() {
                        if has_optional {
                            if let RParamOrTsParamProp::Param(RParam { pat, .. }) = p {
                                match pat {
                                    RPat::Ident(RBindingIdent {
                                        id: RIdent { optional: true, .. },
                                        ..
                                    })
                                    | RPat::Rest(..) => {}
                                    _ => {
                                        child.storage.report(ErrorKind::TS1016 { span: p.span() }.into());
                                    }
                                }
                            }
                        }

                        if let RParamOrTsParamProp::Param(RParam {
                            pat:
                                RPat::Ident(RBindingIdent {
                                    id: RIdent { optional, .. },
                                    ..
                                }),
                            ..
                        }) = *p
                        {
                            if optional {
                                has_optional = true;
                            }
                        }
                    }
                }

                let mut ps = Vec::with_capacity(params.len());
                for param in params.iter() {
                    let mut names = vec![];

                    let mut visitor = VarVisitor { names: &mut names };

                    param.visit_with(&mut visitor);

                    child.scope.declaring.extend(names.clone());

                    let p: FnParam = {
                        let ctx = Ctx {
                            in_constructor_param: true,
                            pat_mode: PatMode::Decl,
                            ..child.ctx
                        };

                        param.validate_with(&mut *child.with_ctx(ctx))?
                    };

                    ps.push(p);

                    child.scope.remove_declaring(names);
                }

                if let Some(body) = &c.body {
                    child.ctx.in_class_member = true;
                    child
                        .visit_stmts_for_return(c.span, false, false, &body.stmts)
                        .report(&mut child.storage);
                }

                Ok(ConstructorSignature {
                    accessibility: c.accessibility,
                    span: c.span,
                    params: ps,
                    ret_ty: None,
                    type_params: None,
                })
            })
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, p: &RTsParamProp) -> VResult<FnParam> {
        if self.ctx.in_declare {
            if let RTsParamPropParam::Assign(..) = p.param {
                self.storage
                    .report(ErrorKind::InitializerDisallowedInAmbientContext { span: p.span }.into())
            }
        }

        match &p.param {
            RTsParamPropParam::Ident(ref i) => {
                let ty: Option<Type> = i.type_ann.validate_with(self).transpose()?.freezed();

                self.declare_var(
                    i.id.span,
                    VarKind::Param,
                    i.id.clone().into(),
                    ty.clone(),
                    None,
                    true,
                    false,
                    false,
                    false,
                )?;

                Ok(FnParam {
                    span: p.span,
                    required: !i.id.optional,
                    pat: RPat::Ident(i.clone()),
                    ty: Box::new(ty.unwrap_or_else(|| Type::any(i.id.span, Default::default()))),
                })
            }
            RTsParamPropParam::Assign(RAssignPat {
                left: box RPat::Ident(ref i),
                right,
                ..
            }) => {
                let ty: Option<Type> = i.type_ann.validate_with(self).transpose()?.freezed();

                let right = right.validate_with_default(self).report(&mut self.storage).freezed();

                if let Some(ty) = &ty {
                    if let Some(right) = right {
                        self.assign_with_opts(
                            &mut Default::default(),
                            ty,
                            &right,
                            AssignOpts {
                                span: right.span(),
                                ..Default::default()
                            },
                        )
                        .report(&mut self.storage);
                    }
                }

                self.declare_var(
                    i.id.span,
                    VarKind::Param,
                    i.id.clone().into(),
                    ty.clone(),
                    None,
                    true,
                    false,
                    false,
                    false,
                )?;

                Ok(FnParam {
                    span: p.span,
                    required: !i.id.optional,
                    pat: RPat::Ident(i.clone()),
                    ty: Box::new(ty.unwrap_or_else(|| Type::any(i.id.span, Default::default()))),
                })
            }
            _ => unreachable!(),
        }
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, p: &RTsFnParam) -> VResult<FnParam> {
        let span = p.span();

        macro_rules! ty {
            ($node_id:expr, $e:expr) => {{
                match self
                    .mutations
                    .as_mut()
                    .map(|m| m.for_pats.get(&$node_id).map(|p| p.ty.clone()))
                    .flatten()
                    .flatten()
                {
                    Some(ty) => Box::new(ty),
                    None => {
                        let e: Option<_> = $e.validate_with(self).transpose()?;
                        Box::new(e.unwrap_or_else(|| {
                            let mut ty = Type::any(span, Default::default());
                            self.mark_as_implicitly_typed(&mut ty);
                            ty
                        }))
                    }
                }
            }};
        }

        Ok(match p {
            RTsFnParam::Ident(i) => FnParam {
                span,
                pat: RPat::Ident(i.clone()),
                required: !i.id.optional,
                ty: ty!(i.node_id, i.type_ann),
            },
            RTsFnParam::Array(p) => FnParam {
                span,
                pat: RPat::Array(p.clone()),
                required: true,
                ty: ty!(p.node_id, p.type_ann),
            },
            RTsFnParam::Rest(p) => FnParam {
                span,
                pat: RPat::Rest(p.clone()),
                required: false,
                ty: ty!(p.node_id, p.type_ann),
            },
            RTsFnParam::Object(p) => FnParam {
                span,
                pat: RPat::Object(p.clone()),
                required: true,
                ty: ty!(p.node_id, p.type_ann),
            },
        })
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, c: &RPrivateMethod) -> VResult<ClassMember> {
        if let js_word!("constructor") = c.key.id.sym {
            self.storage.report(ErrorKind::ConstructorIsKeyword { span: c.key.id.span }.into());
        }

        let key = c.key.validate_with(self).map(Key::Private)?;
        let key_span = key.span();

        let (type_params, params, ret_ty) =
            self.with_child(
                ScopeKind::Method { is_static: c.is_static },
                Default::default(),
                |child: &mut Analyzer| -> VResult<_> {
                    child.ctx.in_static_method = c.is_static;

                    let type_params = try_opt!(c.function.type_params.validate_with(child));
                    if (c.kind == MethodKind::Getter || c.kind == MethodKind::Setter) && type_params.is_some() {
                        child.storage.report(ErrorKind::TS1094 { span: key_span }.into())
                    }

                    let params = c.function.params.validate_with(child)?;

                    let declared_ret_ty = try_opt!(c.function.return_type.validate_with(child));

                    let span = c.function.span;
                    let is_async = c.function.is_async;
                    let is_generator = c.function.is_generator;

                    let inferred_ret_ty =
                        match c.function.body.as_ref().map(|bs| {
                            child.visit_stmts_for_return(span.with_ctxt(SyntaxContext::empty()), is_async, is_generator, &bs.stmts)
                        }) {
                            Some(Ok(ty)) => ty,
                            Some(err) => err?,
                            None => None,
                        };

                    Ok((
                        type_params,
                        params,
                        Box::new(
                            declared_ret_ty
                                .or(inferred_ret_ty)
                                .unwrap_or_else(|| Type::any(key_span, Default::default())),
                        ),
                    ))
                },
            )?;

        match c.kind {
            MethodKind::Method => Ok(ClassMember::Method(Method {
                span: c.span,
                key,
                type_params,
                params,
                ret_ty,
                accessibility: None,
                is_static: c.is_static,
                is_abstract: c.is_abstract,
                is_optional: c.is_optional,
            })),
            MethodKind::Getter => Ok(ClassMember::Property(ClassProperty {
                span: c.span,
                key,
                value: Some(ret_ty),
                is_static: c.is_static,
                accessibility: c.accessibility,
                is_abstract: c.is_abstract,
                is_optional: c.is_optional,
                readonly: false,
                definite: false,
                accessor: Accessor {
                    getter: true,
                    setter: false,
                },
            })),
            MethodKind::Setter => Ok(ClassMember::Property(ClassProperty {
                span: c.span,
                key,
                value: Some(ret_ty),
                is_static: c.is_static,
                accessibility: c.accessibility,
                is_abstract: c.is_abstract,
                is_optional: c.is_optional,
                readonly: false,
                definite: false,
                accessor: Accessor {
                    getter: false,
                    setter: true,
                },
            })),
        }
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, c: &RClassMethod, object_type: Option<&Type>) -> VResult<ClassMember> {
        let marks = self.marks();

        let key = c.key.validate_with(self)?;

        if let Some(object_type) = object_type {
            if let Ok(type_ann) = self.access_property(
                c.span,
                object_type,
                &key,
                TypeOfMode::RValue,
                IdCtx::Var,
                AccessPropertyOpts {
                    disallow_creating_indexed_type_from_ty_els: true,
                    ..Default::default()
                },
            ) {
                self.apply_fn_type_ann(
                    c.span,
                    c.function.node_id,
                    c.function.params.iter().map(|v| &v.pat),
                    Some(&type_ann),
                );
            }
        }

        let c_span = c.span();
        let key_span = c.key.span();

        if c.is_override {
            if let Some(super_class) = &self.scope.get_super_class(false) {
                if self
                    .access_property(
                        key_span,
                        super_class,
                        &key,
                        TypeOfMode::RValue,
                        IdCtx::Var,
                        AccessPropertyOpts {
                            allow_access_abstract_method: true,
                            ..Default::default()
                        },
                    )
                    .is_err()
                {
                    return Err(ErrorKind::NotDeclaredInSuperClass { span: key_span }.into());
                }
            }
        }

        let (params, type_params, declared_ret_ty, inferred_ret_ty) =
            self.with_child(
                ScopeKind::Method { is_static: c.is_static },
                Default::default(),
                |child: &mut Analyzer| -> VResult<_> {
                    child.ctx.in_declare |= c.function.body.is_none();
                    child.ctx.in_async = c.function.is_async;
                    child.ctx.in_generator = c.function.is_generator;
                    child.ctx.in_static_method = c.is_static;
                    child.ctx.is_fn_param = true;

                    child.scope.declaring_prop = match &key {
                        Key::Normal { sym, .. } => Some(Id::word(sym.clone())),
                        _ => None,
                    };

                    {
                        // It's error if abstract method has a body

                        if c.is_abstract && c.function.body.is_some() {
                            child.storage.report(ErrorKind::TS1318 { span: key_span }.into());
                        }
                    }

                    {
                        // Validate params
                        // TODO(kdy1): Move this to parser
                        let mut has_optional = false;
                        for p in &c.function.params {
                            if has_optional {
                                match p.pat {
                                    RPat::Ident(RBindingIdent {
                                        id: RIdent { optional: true, .. },
                                        ..
                                    })
                                    | RPat::Rest(..) => {}
                                    _ => {
                                        child.storage.report(ErrorKind::TS1016 { span: p.span() }.into());
                                    }
                                }
                            }

                            if let RPat::Ident(RBindingIdent {
                                id: RIdent { optional, .. },
                                ..
                            }) = p.pat
                            {
                                if optional {
                                    has_optional = true;
                                }
                            }
                        }
                    }

                    let type_params = try_opt!(c.function.type_params.validate_with(child));
                    if (c.kind == MethodKind::Getter || c.kind == MethodKind::Setter) && type_params.is_some() {
                        child.storage.report(ErrorKind::TS1094 { span: key_span }.into())
                    }

                    let params = {
                        let prev_len = child.scope.declaring_parameters.len();
                        let ids: Vec<Id> = find_ids_in_pat(&c.function.params);
                        child.scope.declaring_parameters.extend(ids);

                        let res = c.function.params.validate_with(child);

                        child.scope.declaring_parameters.truncate(prev_len);

                        res?
                    };
                    child.ctx.is_fn_param = false;

                    // c.function.visit_children_with(child);

                    // if child.ctx.in_declare && c.function.body.is_some() {
                    //     child.storage.report(Error::TS1183 { span: key_span })
                    // }

                    if c.kind == MethodKind::Setter && c.function.return_type.is_some() {
                        child.storage.report(ErrorKind::TS1095 { span: key_span }.into())
                    }

                    let declared_ret_ty = try_opt!(c.function.return_type.validate_with(child));
                    let declared_ret_ty = declared_ret_ty.map(|ty| ty.freezed());
                    child.scope.declared_return_type = declared_ret_ty.clone();

                    let span = c.function.span;
                    let is_async = c.function.is_async;
                    let is_generator = c.function.is_generator;

                    child.ctx.in_class_member = true;
                    let inferred_ret_ty =
                        match c.function.body.as_ref().map(|bs| {
                            child.visit_stmts_for_return(span.with_ctxt(SyntaxContext::empty()), is_async, is_generator, &bs.stmts)
                        }) {
                            Some(Ok(ty)) => ty,
                            Some(err) => err?,
                            None => None,
                        };

                    Ok((params, type_params, declared_ret_ty, inferred_ret_ty))
                },
            )?;

        if c.kind == MethodKind::Getter && c.function.body.is_some() {
            // Inferred return type.

            // getter property must have return statements.
            if inferred_ret_ty.is_none() {
                self.storage.report(ErrorKind::TS2378 { span: key_span }.into());
            }
        }

        let ret_ty = Box::new(declared_ret_ty.unwrap_or_else(|| {
            inferred_ret_ty.map(|ty| ty.generalize_lit()).unwrap_or_else(|| {
                Type::Keyword(KeywordType {
                    span: c_span,
                    kind: if c.function.body.is_some() {
                        TsKeywordTypeKind::TsVoidKeyword
                    } else {
                        TsKeywordTypeKind::TsAnyKeyword
                    },
                    metadata: Default::default(),
                    tracker: Default::default(),
                })
            })
        }));

        if c.kind != MethodKind::Setter {
            let node_id = c.function.node_id;

            let ret_ty = if self.may_generalize(&ret_ty) {
                ret_ty.clone().generalize_lit()
            } else {
                *ret_ty.clone()
            };
            if let Some(m) = &mut self.mutations {
                m.for_fns.entry(node_id).or_default().ret_ty = Some(ret_ty);
            }
        }

        match c.kind {
            MethodKind::Method => Ok(ClassMember::Method(Method {
                span: c_span,
                key,
                accessibility: c.accessibility,
                is_static: c.is_static,
                is_abstract: c.is_abstract,
                is_optional: c.is_optional,
                type_params,
                params,
                ret_ty,
            })),
            MethodKind::Getter => Ok(ClassMember::Property(ClassProperty {
                span: c_span,
                key,
                value: Some(ret_ty),
                is_static: c.is_static,
                accessibility: c.accessibility,
                is_abstract: c.is_abstract,
                is_optional: c.is_optional,
                readonly: false,
                definite: false,
                accessor: Accessor {
                    getter: true,
                    setter: false,
                },
            })),
            MethodKind::Setter => Ok(ClassMember::Property(ClassProperty {
                span: c_span,
                key,
                value: if params.len() == 1 {
                    params.get(0).map(|p| p.ty.clone())
                } else {
                    // TODO: Should emit TS1049 error here
                    Some(Box::new(Type::any(key_span, Default::default())))
                },
                is_static: c.is_static,
                accessibility: c.accessibility,
                is_abstract: c.is_abstract,
                is_optional: c.is_optional,
                readonly: false,
                definite: false,
                accessor: Accessor {
                    getter: false,
                    setter: true,
                },
            })),
        }
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, m: &RClassMember, object_type: Option<&Type>) -> VResult<Option<ClassMember>> {
        Ok(match m {
            RClassMember::PrivateMethod(m) => Some(m.validate_with(self).map(From::from)?),
            RClassMember::PrivateProp(m) => Some(m.validate_with(self).map(From::from)?),
            RClassMember::Empty(..) => None,
            RClassMember::StaticBlock(m) => {
                m.validate_with(self)?;
                None
            }

            RClassMember::Constructor(v) => {
                if self.config.is_builtin {
                    Some(v.validate_with_default(self).map(From::from)?)
                } else {
                    unreachable!("constructors should be handled by class handler")
                }
            }
            RClassMember::Method(method) => {
                let v = method.validate_with_args(self, object_type)?;

                Some(v)
            }
            RClassMember::ClassProp(v) => Some(ClassMember::Property(v.validate_with_args(self, object_type)?)),
            RClassMember::TsIndexSignature(v) => Some(ClassMember::IndexSignature(v.validate_with(self)?)),
            RClassMember::AutoAccessor(v) => Some(ClassMember::Property(v.validate_with_args(self, object_type)?)),
        })
    }
}

impl Analyzer<'_, '_> {
    fn report_errors_for_duplicate_class_members(&mut self, c: &RClass) -> VResult<()> {
        fn normalize_prop_name(p: &RPropName) -> Cow<RPropName> {
            match p {
                RPropName::Num(v) => Cow::Owned(RPropName::Ident(RIdent::new(
                    v.value.to_string().into(),
                    v.span.with_ctxt(SyntaxContext::empty()),
                ))),
                _ => Cow::Borrowed(p),
            }
        }

        let mut keys = vec![];
        let mut private_keys = vec![];
        let mut is_props = FxHashSet::default();
        let mut is_private_props = FxHashSet::default();

        for member in &c.body {
            match member {
                RClassMember::Method(
                    m @ RClassMethod {
                        kind: MethodKind::Method,
                        function: box RFunction { body: Some(..), .. },
                        ..
                    },
                ) => {
                    keys.push((normalize_prop_name(&m.key), m.is_static));
                }
                RClassMember::PrivateMethod(
                    m @ RPrivateMethod {
                        function: box RFunction { body: Some(..), .. },
                        ..
                    },
                ) => {
                    private_keys.push((&m.key, m.is_static));
                }

                RClassMember::ClassProp(RClassProp {
                    key: RPropName::Ident(key),
                    is_static,
                    ..
                }) => {
                    is_props.insert(keys.len());
                    keys.push((Cow::Owned(RPropName::Ident(key.clone())), *is_static));
                }

                RClassMember::ClassProp(m) => {
                    is_props.insert(keys.len());

                    let key = Cow::Borrowed(&m.key);
                    keys.push((key, m.is_static));
                }
                RClassMember::PrivateProp(m) => {
                    is_private_props.insert(private_keys.len());
                    private_keys.push((&m.key, m.is_static));
                }
                _ => {}
            }
        }

        for (i, l) in keys.iter().enumerate() {
            for (j, r) in keys.iter().enumerate() {
                if i == j {
                    continue;
                }

                if is_prop_name_eq(&l.0, &r.0) && l.1 == r.1 {
                    if is_props.contains(&i) && is_props.contains(&j) {
                        continue;
                    }
                    // We use different error for duplicate functions
                    if !is_props.contains(&i) && !is_props.contains(&j) {
                        continue;
                    }

                    self.storage.report(ErrorKind::DuplicateNameWithoutName { span: l.0.span() }.into());
                }
            }
        }

        for (i, l) in private_keys.iter().enumerate() {
            for (j, r) in private_keys.iter().enumerate() {
                if i == j {
                    continue;
                }

                if l.0.eq_ignore_span(r.0) {
                    if l.1 == r.1 {
                        if is_private_props.contains(&i) && is_private_props.contains(&j) {
                            continue;
                        }

                        // We use different error for duplicate functions
                        if !is_private_props.contains(&i) && !is_private_props.contains(&j) {
                            continue;
                        }

                        self.storage.report(ErrorKind::DuplicateNameWithoutName { span: l.0.span() }.into());
                    } else if j < i {
                        self.storage
                            .report(ErrorKind::DuplicatePrivateStaticInstance { span: l.0.span() }.into());
                    }
                }
            }
        }

        Ok(())
    }

    fn report_errors_for_statics_mixed_with_instances(&mut self, c: &RClass) -> VResult<()> {
        if self.ctx.in_declare {
            return Ok(());
        }

        let mut spans = vec![];
        let mut name: Option<&RPropName> = None;

        for (last, (idx, m)) in c.body.iter().enumerate().identify_last() {
            match m {
                RClassMember::Method(m) => {
                    let span = m.key.span();

                    if m.function.body.is_some() {
                        spans.clear();
                        name = None;
                        continue;
                    }

                    if name.is_none() {
                        name = Some(&m.key);
                        spans.push((span, m.is_static));
                        continue;
                    }

                    if last {
                        if name.unwrap().eq_ignore_span(&m.key) {
                            spans.push((span, m.is_static));
                        }
                    }

                    if last || !name.unwrap().eq_ignore_span(&m.key) {
                        let spans_for_error = take(&mut spans);

                        let has_static = spans_for_error.iter().any(|(_, v)| *v);
                        let has_instance = spans_for_error.iter().any(|(_, v)| !*v);

                        if has_static && has_instance {
                            let report_error_for_static = !spans_for_error.first().unwrap().1;

                            for (span, is_static) in spans_for_error {
                                if report_error_for_static && is_static {
                                    self.storage.report(ErrorKind::ShouldBeInstanceMethod { span }.into())
                                } else if !report_error_for_static && !is_static {
                                    self.storage.report(ErrorKind::ShouldBeStaticMethod { span }.into())
                                }
                            }
                        }

                        name = None;

                        // Note: This span is for next name.
                        spans.push((span, m.is_static));
                        continue;
                    }

                    // At this point, previous name is identical with current name.

                    spans.push((span, m.is_static));
                }
                _ => {
                    // Other than method.

                    if name.is_none() {
                        continue;
                    }
                }
            }
        }

        Ok(())
    }

    fn report_errors_for_wrong_ambient_methods_of_class(&mut self, c: &RClass, declare: bool) -> VResult<()> {
        if self.ctx.in_declare || self.config.is_builtin {
            return Ok(());
        }

        fn is_key_optional(key: &RPropName) -> bool {
            match key {
                RPropName::Ident(i) => i.optional,
                _ => false,
            }
        }

        fn is_prop_name_eq_include_computed(l: &RPropName, r: &RPropName) -> bool {
            if let RPropName::Computed(l) = l {
                if let RPropName::Computed(r) = r {
                    if l.eq_ignore_span(r) {
                        // TODO(kdy1): Return true only if l and r are both
                        // symbol type
                        return true;
                    }
                }
            }

            is_prop_name_eq(l, r)
        }

        // Report errors for code like
        //
        //      class C {
        //           foo();
        //      }
        if declare {
            return Ok(());
        }

        let mut ignore_not_following_for = vec![];

        {
            // Check for mixed `abstract`.
            //
            // Class members with same name should be all abstract or all concrete.

            let mut spans = vec![];
            let mut name: Option<&RPropName> = None;
            let mut last_was_abstract = false;

            for (last, (idx, m)) in c.body.iter().enumerate().identify_last() {
                match m {
                    RClassMember::Method(m) => {
                        let span = m.key.span();

                        if name.is_none() {
                            name = Some(&m.key);
                            spans.push((span, m.is_abstract));
                            last_was_abstract = m.is_abstract;
                            continue;
                        }

                        if last {
                            if name.unwrap().eq_ignore_span(&m.key) {
                                spans.push((span, m.is_abstract));
                                last_was_abstract = m.is_abstract;
                            }
                        }

                        if last || !name.unwrap().eq_ignore_span(&m.key) {
                            let report_error_for_abstract = !last_was_abstract;

                            let spans_for_error = take(&mut spans);

                            {
                                let has_abstract = spans_for_error.iter().any(|(_, v)| *v);
                                let has_concrete = spans_for_error.iter().any(|(_, v)| !*v);

                                if has_abstract && has_concrete {
                                    ignore_not_following_for.push(name.unwrap().clone());

                                    for (span, is_abstract) in spans_for_error {
                                        if (report_error_for_abstract && is_abstract) || (!report_error_for_abstract && !is_abstract) {
                                            self.storage.report(ErrorKind::AbstractAndConcreteIsMixed { span }.into())
                                        }
                                    }
                                }
                            }

                            name = None;

                            // Note: This span is for next name.
                            spans.push((span, m.is_abstract));
                            continue;
                        }

                        // At this point, previous name is identical with current name.

                        last_was_abstract = m.is_abstract;

                        spans.push((span, m.is_abstract));
                    }
                    _ => {
                        // Other than method.

                        if name.is_none() {
                            continue;
                        }

                        let is_not_finished = c.body[idx..].iter().any(|member| match member {
                            RClassMember::Method(m) => m.key.eq_ignore_span(name.unwrap()),
                            _ => false,
                        });

                        if is_not_finished {
                            // In this case, we report `abstract methods must be
                            // sequential`
                            if let Some((span, _)) = spans.last() {
                                self.storage
                                    .report(ErrorKind::AbstractClassMethodShouldBeSequential { span: *span }.into())
                            }
                        }
                    }
                }
            }
        }

        let mut errors = Errors::default();
        // Span of name
        let mut spans = vec![];
        let mut name: Option<&RPropName> = None;

        for (last, (idx, m)) in c.body.iter().enumerate().identify_last() {
            macro_rules! check {
                ($m:expr, $body:expr, $is_constructor:expr) => {{
                    let m = $m;

                    let computed = matches!(m.key, RPropName::Computed(..));

                    if $body.is_none() {
                        if name.is_some() && !is_key_optional(&m.key) && !is_prop_name_eq_include_computed(&name.unwrap(), &m.key) {
                            for (span, is_constructor) in take(&mut spans) {
                                if is_constructor {
                                    errors.push(ErrorKind::ConstructorImplMissingOrNotFollowedByDecl { span }.into());
                                } else {
                                    errors.push(ErrorKind::FnImplMissingOrNotFollowedByDecl { span }.into());
                                }
                            }
                        }

                        // Body of optional method can be omitted
                        if is_key_optional(&m.key) {
                            name = None;
                            spans.clear();
                        } else {
                            if name.is_some() && is_prop_name_eq_include_computed(&name.unwrap(), &m.key) {
                                spans.clear();
                            }
                            spans.push((m.key.span(), $is_constructor));

                            name = Some(&m.key);
                        }
                    } else {
                        if name.is_none() || is_prop_name_eq_include_computed(&name.unwrap(), &m.key) {
                            // TODO(kdy1): Verify parameters

                            if name.is_some() {
                                if let Some(mutations) = &mut self.mutations {
                                    mutations.for_class_members.entry(m.node_id).or_default().remove = true;
                                }
                            }

                            spans.clear();
                            name = None;
                        } else {
                            let constructor_name = RPropName::Ident(RIdent::new(js_word!("constructor"), DUMMY_SP));

                            if is_prop_name_eq_include_computed(&name.unwrap(), &constructor_name) {
                                for (span, is_constructor) in take(&mut spans) {
                                    if is_constructor {
                                        errors.push(ErrorKind::ConstructorImplMissingOrNotFollowedByDecl { span }.into());
                                    } else {
                                        errors.push(ErrorKind::FnImplMissingOrNotFollowedByDecl { span }.into());
                                    }
                                }
                            } else if is_prop_name_eq_include_computed(&m.key, &constructor_name) {
                                for (span, is_constructor) in take(&mut spans) {
                                    if is_constructor {
                                        errors.push(ErrorKind::ConstructorImplMissingOrNotFollowedByDecl { span }.into());
                                    } else {
                                        errors.push(ErrorKind::FnImplMissingOrNotFollowedByDecl { span }.into());
                                    }
                                }
                            } else {
                                spans = vec![];

                                errors.push(ErrorKind::TS2389 { span: m.key.span() }.into());
                            }

                            name = None;
                        }

                        if is_key_optional(&m.key) {
                            name = None;
                            spans.clear();
                        }
                    }
                }};
            }

            match *m {
                RClassMember::Constructor(ref m) => {
                    if !c.is_abstract {
                        check!(m, m.body, true)
                    }
                }
                RClassMember::Method(ref m @ RClassMethod { is_abstract: false, .. }) => {
                    if ignore_not_following_for
                        .iter()
                        .any(|item| is_prop_name_eq_include_computed(item, &m.key))
                    {
                        continue;
                    }

                    check!(m, m.function.body, false)
                }
                _ => {}
            }
        }

        // Class definition ended with `foo();`
        for (span, is_constructor) in std::mem::take(&mut spans) {
            if is_constructor {
                errors.push(ErrorKind::ConstructorImplMissingOrNotFollowedByDecl { span }.into());
            } else {
                errors.push(ErrorKind::FnImplMissingOrNotFollowedByDecl { span }.into());
            }
        }

        self.storage.report_all(errors);

        Ok(())
    }

    pub(super) fn validate_computed_prop_key(&mut self, span: Span, key: &RExpr) -> VResult<()> {
        if self.config.is_builtin {
            // We don't need to validate builtin
            return Ok(());
        }

        let mut errors = Errors::default();
        let is_symbol_access = matches!(
            *key,
            RExpr::Member(RMemberExpr {
                obj: box RExpr::Ident(RIdent {
                    sym: js_word!("Symbol"), ..
                }),
                ..
            })
        );

        let ty = match key.validate_with_default(self).map(|mut ty| {
            ty.respan(span);
            ty
        }) {
            Ok(ty) => ty,
            Err(err) => {
                if let ErrorKind::TS2585 { span } = *err {
                    Err(ErrorKind::TS2585 { span })?
                }

                errors.push(err);

                Type::any(span, Default::default())
            }
        };

        let ty = self.expand_enum_variant(ty)?;

        match *ty.normalize() {
            Type::Lit(..) => {}

            _ if is_symbol_access || ty.is_symbol_like() => {}
            _ => errors.push(ErrorKind::TS1166 { span }.into()),
        }

        if !errors.is_empty() {
            Err(ErrorKind::Errors {
                span,
                errors: errors.into(),
            })?
        }

        Ok(())
    }

    /// TODO(kdy1): Implement this.
    fn report_errors_for_conflicting_interfaces(&mut self, interfaces: &[TsExpr]) {}

    fn report_errors_for_wrong_implementations_of_class(&mut self, name: Option<Span>, class: &ArcCow<ClassDef>) {
        if self.config.is_builtin {
            return;
        }

        if class.is_abstract || self.ctx.in_declare {
            return;
        }

        let class_ty = Type::Class(Class {
            span: class.span,
            def: class.clone(),
            metadata: ClassMetadata {
                common: class.metadata.common,
                ..Default::default()
            },
            tracker: Default::default(),
        })
        .freezed();

        for parent in &*class.implements {
            let res: VResult<_> = try {
                let parent = self
                    .type_of_ts_entity_name(parent.span(), &parent.expr, parent.type_args.as_deref())?
                    .freezed();
                let parent = self.instantiate_class(parent.span(), &parent)?;

                self.assign_with_opts(
                    &mut Default::default(),
                    &parent,
                    &class_ty,
                    AssignOpts {
                        span: parent.span(),
                        allow_unknown_rhs: Some(true),
                        ..Default::default()
                    },
                )
                .context("tried to assign a class to parent interface")
                .convert_err(|err| {
                    let span = err.span();
                    if err.code() == 2322 {
                        ErrorKind::Errors {
                            span,
                            errors: err
                                .into_causes()
                                .into_iter()
                                .map(|err| {
                                    err.convert_all(|err| {
                                        ErrorKind::InvalidImplOfInterface {
                                            span: match &*err {
                                                ErrorKind::AssignFailed { right_ident: Some(s), .. } => *s,
                                                ErrorKind::AssignFailed { left, .. } => left.span(),
                                                _ => err.span(),
                                            },
                                            cause: Box::new(err),
                                        }
                                        .into()
                                    })
                                })
                                .collect(),
                        }
                    } else {
                        match err {
                            ErrorKind::Errors { errors, span } => {
                                for e in &errors {
                                    if let ErrorKind::MissingFields { .. } = **e {
                                        return ErrorKind::ClassIncorrectlyImplementsInterface { span: parent.span() };
                                    }
                                }

                                ErrorKind::Errors { errors, span }
                            }
                            ErrorKind::MissingFields { .. } => ErrorKind::ClassIncorrectlyImplementsInterface { span: parent.span() },
                            _ => err,
                        }
                    }
                })?;
            };

            res.report(&mut self.storage);
        }
    }

    /// Should be called only from `Validate<Class>`.
    fn validate_inherited_members_from_super_class(&mut self, name: Option<Span>, class: &ClassDef) {
        if class.is_abstract || self.ctx.in_declare {
            return;
        }

        let span = name.unwrap_or({
            // TODD: c.span().lo() + BytePos(5) (aka class token)
            class.span
        });

        if let Some(super_ty) = &class.super_class {
            self.validate_super_class(span, super_ty);

            self.report_error_for_wrong_super_class_inheritance(span, &class.body, super_ty)
        }
    }

    fn report_error_for_wrong_super_class_inheritance(&mut self, span: Span, members: &[ClassMember], super_ty: &Type) {
        let super_ty = self.normalize(Some(span), Cow::Borrowed(super_ty), Default::default());
        let super_ty = match super_ty {
            Ok(v) => v,
            Err(err) => {
                self.storage.report(err);
                return;
            }
        };

        let mut errors = Errors::default();
        let mut new_members = vec![];

        let res: VResult<()> = try {
            if let Type::ClassDef(sc) = super_ty.normalize() {
                'outer: for sm in &sc.body {
                    match sm {
                        ClassMember::Property(super_property) => {
                            for m in members {
                                if let ClassMember::Property(ref p) = m {
                                    if !&p.key.type_eq(&super_property.key) {
                                        continue;
                                    }

                                    if !p.is_static
                                        && !super_property.is_static
                                        && p.accessor != super_property.accessor
                                        && (super_property.accessor.getter || super_property.accessor.setter)
                                    {
                                        self.storage
                                            .report(ErrorKind::DefinedWithAccessorInSuper { span: p.key.span() }.into())
                                    }

                                    if super_property.accessibility == Some(Accessibility::Private)
                                        && p.accessibility != Some(Accessibility::Private)
                                    {
                                        self.storage
                                            .report(ErrorKind::PrivatePropertyIsDifferent { span: super_ty.span() }.into())
                                    }

                                    continue 'outer;
                                }
                            }

                            continue 'outer;
                        }
                        ClassMember::Method(super_method) => {
                            if !super_method.is_abstract {
                                new_members.push(sm.clone());
                                continue 'outer;
                            }
                            if super_method.is_optional {
                                // TODO(kdy1): Validate parameters

                                // TODO(kdy1): Validate return type
                                continue 'outer;
                            }

                            for m in members {
                                if let ClassMember::Method(ref m) = m {
                                    if !&m.key.type_eq(&super_method.key) {
                                        continue;
                                    }

                                    // TODO(kdy1): Validate parameters

                                    // TODO(kdy1): Validate return type
                                    continue 'outer;
                                }
                            }
                        }
                        _ => {
                            // TODO(kdy1): Verify
                            continue 'outer;
                        }
                    }

                    if let Some(key) = sm.key() {
                        errors.push(
                            ErrorKind::ClassDoesNotImplementMember {
                                span,
                                key: Box::new(key.into_owned()),
                            }
                            .into(),
                        );
                    }
                }

                if sc.is_abstract {
                    // Check super class of super class
                    if let Some(super_ty) = &sc.super_class {
                        new_members.extend(members.to_vec());
                        self.report_error_for_wrong_super_class_inheritance(span, &new_members, super_ty);
                    }
                }
            }
        };

        if let Err(err) = res {
            errors.push(err);
        }

        self.storage.report_all(errors);
    }
}

/// Order:
///
/// 1. static properties
/// 2. static methods, using dependency graph.
/// 3. TsParamProp in constructors.
/// 4. Properties from top to bottom.
/// 5. Others, using dependency graph.
#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, c: &RClass, type_ann: Option<&Type>) -> VResult<ArcCow<ClassDef>> {
        let marks = self.marks();

        self.ctx.computed_prop_mode = ComputedPropMode::Class {
            has_body: !self.ctx.in_declare,
        };

        c.decorators.visit_with(self);
        let name = self.scope.this_class_name.take();
        if let Some(i) = &name {
            match &**i.sym() {
                "any" | "void" | "never" | "string" | "number" | "boolean" | "null" | "undefined" | "symbol" => {
                    self.storage.report(ErrorKind::InvalidClassName { span: c.span }.into());
                }
                "Object" if self.env.target() <= EsVersion::Es5 => match self.env.module() {
                    ModuleConfig::None if self.ctx.in_declare => {}

                    ModuleConfig::None | ModuleConfig::Umd | ModuleConfig::System | ModuleConfig::Amd | ModuleConfig::CommonJs => {
                        self.storage
                            .report(ErrorKind::ClassNameCannotBeObjectWhenTargetingEs5WithModule { span: c.span }.into());
                    }
                    _ => {}
                },
                _ => {}
            }
        }

        let mut types_to_register: Vec<(Id, _)> = vec![];
        let mut additional_members = vec![];

        // Scope is required because of type parameters.
        let c = self.with_child(ScopeKind::Class, Default::default(), |child: &mut Analyzer| -> VResult<_> {
            child.ctx.super_references_super_class = true;
            child.ctx.in_class_with_super = c.super_class.is_some();

            child.scope.declaring_type_params.extend(
                c.type_params
                    .iter()
                    .flat_map(|decl| &decl.params)
                    .map(|param| param.name.clone().into()),
            );

            // Register the class.
            child.scope.this_class_name = name.clone();

            // We handle type parameters first.
            let type_params = try_opt!(c.type_params.validate_with(child)).map(Box::new);
            child.resolve_parent_interfaces(&c.implements, false);

            let mut super_class = {
                // Then, we can expand super class

                let super_type_params = try_opt!(c.super_type_params.validate_with(child));

                match &c.super_class {
                    Some(box expr) => {
                        let need_base_class = !matches!(expr, RExpr::Ident(..));
                        let super_ty = expr
                            .validate_with_args(child, (TypeOfMode::RValue, super_type_params.as_ref(), None))
                            .context("tried to validate super class")
                            .convert_err(|err| match err {
                                ErrorKind::TypeUsedAsVar { span, name } => ErrorKind::CannotExtendTypeOnlyItem { span, name },
                                _ => err,
                            })
                            .report(&mut child.storage)
                            .unwrap_or_else(|| Type::any(expr.span(), Default::default()));
                        child.validate_with(|a| match super_ty.normalize() {
                            Type::Lit(..)
                            | Type::Keyword(KeywordType {
                                kind: TsKeywordTypeKind::TsStringKeyword,
                                ..
                            })
                            | Type::Keyword(KeywordType {
                                kind: TsKeywordTypeKind::TsNumberKeyword,
                                ..
                            })
                            | Type::Keyword(KeywordType {
                                kind: TsKeywordTypeKind::TsBooleanKeyword,
                                ..
                            }) => Err(ErrorKind::InvalidSuperClass { span: super_ty.span() }.into()),
                            _ => Ok(()),
                        });

                        match super_ty.normalize() {
                            // We should handle mixin
                            Type::Intersection(i) if need_base_class => {
                                let mut has_class_in_super = false;
                                let class_name = name.clone().unwrap_or_else(|| Id::word("__class".into()));
                                let new_ty: RIdent =
                                    private_ident!(format!("{}_base", class_name.as_str())).into_rnode(&mut NodeIdGenerator::invalid());

                                // We should add it at same level as class
                                types_to_register.push((new_ty.clone().into(), super_ty.clone()));

                                let super_ty = Box::new(Type::Intersection(Intersection {
                                    types: i
                                        .types
                                        .iter()
                                        .map(|ty| {
                                            if let Type::Class(c) = ty.normalize() {
                                                has_class_in_super = true;
                                                // class A -> typeof A
                                                return c
                                                    .def
                                                    .name
                                                    .as_ref()
                                                    .map(|id| {
                                                        Type::Query(QueryType {
                                                            span: c.span,
                                                            expr: Box::new(QueryExpr::TsEntityName(id.clone().into())),
                                                            metadata: QueryTypeMetadata {
                                                                common: c.metadata.common,
                                                                ..Default::default()
                                                            },
                                                            tracker: Default::default(),
                                                        })
                                                    })
                                                    .expect("Super class should be named");
                                            }

                                            ty.clone()
                                        })
                                        .collect(),
                                    ..i.clone()
                                }));

                                if has_class_in_super {
                                    child.data.prepend_stmts.push(RStmt::Decl(RDecl::Var(Box::new(RVarDecl {
                                        node_id: NodeId::invalid(),
                                        span: DUMMY_SP,
                                        kind: VarDeclKind::Const,
                                        declare: false,
                                        decls: vec![RVarDeclarator {
                                            node_id: NodeId::invalid(),
                                            span: i.span,
                                            name: RPat::Ident(RBindingIdent {
                                                node_id: NodeId::invalid(),
                                                type_ann: Some(Box::new(RTsTypeAnn {
                                                    node_id: NodeId::invalid(),
                                                    span: DUMMY_SP,
                                                    type_ann: Box::new(super_ty.into()),
                                                })),
                                                id: new_ty.clone(),
                                            }),
                                            init: None,
                                            definite: false,
                                        }],
                                    }))));
                                } else {
                                    child
                                        .data
                                        .prepend_stmts
                                        .push(RStmt::Decl(RDecl::TsTypeAlias(Box::new(RTsTypeAliasDecl {
                                            node_id: NodeId::invalid(),
                                            span: DUMMY_SP,
                                            declare: false,
                                            id: new_ty.clone(),
                                            // TODO(kdy1): Handle type parameters
                                            type_params: None,
                                            type_ann: Box::new(super_ty.into()),
                                        }))));
                                }

                                if let Some(m) = &mut child.mutations {
                                    let node_id = c.node_id;
                                    m.for_classes.entry(node_id).or_default().super_class = Some(Box::new(RExpr::Ident(new_ty.clone())));
                                }
                                Some(Box::new(Type::Ref(Ref {
                                    span: DUMMY_SP,
                                    type_name: RTsEntityName::Ident(new_ty),
                                    // TODO(kdy1): Handle type parameters
                                    type_args: None,
                                    metadata: Default::default(),
                                    tracker: Default::default(),
                                })))
                            }
                            Type::ClassDef(cls) => {
                                // check if the constructor of the super class is private.
                                for member in cls.body.iter() {
                                    if let ClassMember::Constructor(cons) = member {
                                        if matches!(cons.accessibility, Some(Accessibility::Private)) {
                                            child
                                                .storage
                                                .report(ErrorKind::InvalidExtendDueToConstructorPrivate { span: cls.span }.into());
                                        };
                                    }
                                }
                                Some(Box::new(super_ty))
                            }
                            _ => Some(Box::new(super_ty)),
                        }
                    }

                    _ => None,
                }
            };
            super_class.freeze();

            let implements = c.implements.validate_with(child).map(Box::new)?;

            // TODO(kdy1): Check for implements
            child
                .report_errors_for_wrong_ambient_methods_of_class(c, false)
                .report(&mut child.storage);
            child.report_errors_for_statics_mixed_with_instances(c).report(&mut child.storage);
            child.report_errors_for_duplicate_class_members(c).report(&mut child.storage);

            child.scope.super_class = super_class.clone();
            {
                // Validate constructors
                let constructors_with_body = c
                    .body
                    .iter()
                    .filter_map(|member| match member {
                        RClassMember::Constructor(c) if c.body.is_some() => Some(c.span),
                        _ => None,
                    })
                    .collect_vec();

                if constructors_with_body.len() >= 2 {
                    for &span in &constructors_with_body {
                        child.storage.report(ErrorKind::DuplicateConstructor { span }.into())
                    }
                }

                for m in c.body.iter() {
                    if let RClassMember::Constructor(ref cons) = *m {
                        //
                        if cons.body.is_none() {
                            for p in &cons.params {
                                if let RParamOrTsParamProp::TsParamProp(..) = *p {
                                    child
                                        .storage
                                        .report(ErrorKind::ParamPropIsNotAllowedInAmbientConstructor { span: p.span() }.into())
                                }
                            }
                        }
                    }
                }
            }

            // Handle nodes in order described above
            let body = {
                let mut declared_static_keys = vec![];
                let mut declared_instance_keys = vec![];

                // Handle static properties
                for (index, node) in c.body.iter().enumerate() {
                    if let RClassMember::TsIndexSignature(..) = node {
                        let m = node.validate_with_args(child, type_ann)?;
                        if let Some(member) = m {
                            child.scope.this_class_members.push((index, member));
                        }
                    }
                }

                // Handle static properties
                for (index, node) in c.body.iter().enumerate() {
                    match node {
                        RClassMember::ClassProp(RClassProp { is_static: true, .. })
                        | RClassMember::PrivateProp(RPrivateProp { is_static: true, .. })
                        | RClassMember::AutoAccessor(RAutoAccessor { is_static: true, .. }) => {
                            let m = node.validate_with_args(child, type_ann)?;
                            if let Some(member) = m {
                                // Check for duplicate property names.
                                if let Some(key) = member.key() {
                                    // TODO(kdy1): Use better logic for testing key equality
                                    if declared_static_keys.iter().any(|prev: &Key| prev.type_eq(&*key)) {
                                        child.storage.report(ErrorKind::DuplicateProperty { span: key.span() }.into())
                                    }
                                    declared_static_keys.push(key.into_owned());
                                }

                                let member = member.fold_with(&mut LitGeneralizer {});
                                child.scope.this_class_members.push((index, member));
                            }
                        }
                        _ => {}
                    }
                }

                // Handle ts parameter properties
                for (index, constructor) in c.body.iter().enumerate().filter_map(|(i, member)| match member {
                    RClassMember::Constructor(c) => Some((i, c)),
                    _ => None,
                }) {
                    for param in &constructor.params {
                        match param {
                            RParamOrTsParamProp::TsParamProp(p) => {
                                if p.accessibility == Some(Accessibility::Private) {
                                    let is_optional = match p.param {
                                        RTsParamPropParam::Ident(_) => false,
                                        RTsParamPropParam::Assign(_) => true,
                                    };
                                    let mut key = match &p.param {
                                        RTsParamPropParam::Assign(RAssignPat {
                                            left: box RPat::Ident(key),
                                            ..
                                        })
                                        | RTsParamPropParam::Ident(key) => key.clone(),
                                        _ => unreachable!("TypeScript parameter property with pattern other than an identifier"),
                                    };
                                    key.type_ann = None;
                                    let key = RPropName::Ident(key.id);
                                    additional_members.push(RClassMember::ClassProp(RClassProp {
                                        node_id: NodeId::invalid(),
                                        span: p.span,
                                        key,
                                        value: None,
                                        is_static: false,
                                        accessibility: Some(Accessibility::Private),
                                        is_abstract: false,
                                        is_optional,
                                        readonly: p.readonly,
                                        definite: false,
                                        type_ann: None,
                                        decorators: Default::default(),
                                        declare: false,
                                        is_override: false,
                                    }));
                                }

                                let (i, ty) = match &p.param {
                                    RTsParamPropParam::Ident(i) => {
                                        let ty = i.type_ann.clone();
                                        let ty = try_opt!(ty.validate_with(child));
                                        (i, ty)
                                    }
                                    RTsParamPropParam::Assign(RAssignPat {
                                        span,
                                        left: box RPat::Ident(i),
                                        type_ann,
                                        right,
                                        ..
                                    }) => {
                                        let ctx = Ctx {
                                            use_properties_of_this_implicitly: true,
                                            ..child.ctx
                                        };
                                        let ty = type_ann.clone().or_else(|| i.type_ann.clone());
                                        let mut ty = try_opt!(ty.validate_with(&mut *child.with_ctx(ctx)));
                                        if ty.is_none() {
                                            ty = Some(right.validate_with_default(&mut *child.with_ctx(ctx))?.generalize_lit());
                                        }
                                        (i, ty)
                                    }
                                    _ => unreachable!(),
                                };

                                if let Some(ty) = &ty {
                                    if i.type_ann.is_none() {
                                        if let Some(m) = &mut child.mutations {
                                            m.for_pats.entry(i.node_id).or_default().ty = Some(ty.clone());
                                        }
                                    }
                                }
                                // Register a class property.

                                child.scope.this_class_members.push((
                                    index,
                                    ClassMember::Property(stc_ts_types::ClassProperty {
                                        span: p.span,
                                        key: Key::Normal {
                                            span: i.id.span,
                                            sym: i.id.sym.clone(),
                                        },
                                        value: ty.map(Box::new),
                                        is_static: false,
                                        accessibility: p.accessibility,
                                        is_abstract: false,
                                        is_optional: false,
                                        readonly: p.readonly,
                                        definite: false,
                                        accessor: Default::default(),
                                    }),
                                ));
                            }
                            RParamOrTsParamProp::Param(..) => {}
                        }
                    }
                }

                // Handle properties
                for (index, member) in c.body.iter().enumerate() {
                    match member {
                        RClassMember::ClassProp(RClassProp { is_static: false, .. })
                        | RClassMember::PrivateProp(RPrivateProp { is_static: false, .. })
                        | RClassMember::AutoAccessor(RAutoAccessor { is_static: false, .. }) => {
                            //
                            let class_member = member.validate_with_args(child, type_ann).report(&mut child.storage).flatten();
                            if let Some(member) = class_member {
                                // Check for duplicate property names.
                                if let Some(key) = member.key() {
                                    // TODO(kdy1): Use better logic for testing key equality
                                    if declared_instance_keys.iter().any(|prev: &Key| prev.type_eq(&*key)) {
                                        child.storage.report(ErrorKind::DuplicateProperty { span: key.span() }.into())
                                    }
                                    declared_instance_keys.push(key.into_owned());
                                }

                                let member = member.fold_with(&mut LitGeneralizer);
                                child.scope.this_class_members.push((index, member));
                            }
                        }
                        _ => {}
                    }
                }

                {
                    let mut ambient_cons: Vec<ConstructorSignature> = vec![];
                    let mut cons_with_body = None;
                    for (index, constructor) in c.body.iter().enumerate().filter_map(|(i, member)| match member {
                        RClassMember::Constructor(c) => Some((i, c)),
                        _ => None,
                    }) {
                        let member = constructor.validate_with_args(child, super_class.as_deref())?;
                        if constructor.body.is_some() {
                            ambient_cons.push(member.clone());
                        } else {
                            cons_with_body = Some(member.clone());
                        }
                        child.scope.this_class_members.push((index, member.into()));
                    }
                    child
                        .report_errors_for_wrong_constructor_overloads(&ambient_cons, cons_with_body.as_ref())
                        .report(&mut child.storage);
                }

                // Handle user-declared method signatures.
                for member in &c.body {}

                // Handle body of methods and a constructor
                // (in try mode - we ignores error if it's not related to the type of return
                // value)
                //
                // This is to infer return types of methods
                for member in &c.body {}

                // Actually check types of method / constructors.

                let remaining = c
                    .body
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| child.scope.this_class_members.iter().all(|(idx, _)| *idx != *index))
                    .map(|v| v.0)
                    .collect::<Vec<_>>();

                let order = child.calc_eval_order_of_class_methods(remaining, &c.body);

                for index in order {
                    let ty = c.body[index].validate_with_args(child, type_ann)?;
                    if let Some(ty) = ty {
                        child.scope.this_class_members.push((index, ty));
                    }
                }

                take(&mut child.scope.this_class_members)
            };

            let body = child.combine_class_properties(body);

            if !additional_members.is_empty() {
                // Add private parameter properties to .d.ts file

                if let Some(m) = &mut child.mutations {
                    m.for_classes
                        .entry(c.node_id)
                        .or_default()
                        .additional_members
                        .extend(additional_members);
                }
            }

            let body = body.into_iter().map(|v| v.1).collect_vec();

            let class = ArcCow::new_freezed(ClassDef {
                span: c.span,
                name,
                is_abstract: c.is_abstract,
                super_class,
                type_params,
                body,
                implements,
                metadata: Default::default(),
                tracker: Default::default(),
            });

            child
                .report_errors_for_class_member_incompatible_with_index_signature(&class)
                .report(&mut child.storage);

            child.validate_inherited_members_from_super_class(None, &class);
            child.report_errors_for_wrong_implementations_of_class(None, &class);
            child.report_errors_for_conflicting_interfaces(&class.implements);

            Ok(class)
        })?;

        for (i, ty) in types_to_register {
            self.register_type(i, ty);
        }

        Ok(c)
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, c: &RClassExpr, type_ann: Option<&Type>) -> VResult<()> {
        self.scope.this_class_name = c.ident.as_ref().map(|v| v.into());
        let ty = match c.class.validate_with_args(self, type_ann) {
            Ok(ty) => ty.into(),
            Err(err) => {
                self.storage.report(err);
                Type::any(c.span(), Default::default())
            }
        };

        let old_this = self.scope.this.take();
        // self.scope.this = Some(ty.clone());

        let c = self
            .with_child(ScopeKind::Block, Default::default(), |analyzer| {
                if let Some(ref i) = c.ident {
                    let ty = analyzer.register_type(i.into(), ty);

                    match analyzer.declare_var(
                        ty.span(),
                        VarKind::Class,
                        i.into(),
                        Some(ty),
                        None,
                        // initialized = true
                        true,
                        // declare Class does not allow multiple declarations.
                        false,
                        false,
                        false,
                    ) {
                        Ok(..) => {}
                        Err(err) => {
                            analyzer.storage.report(err);
                        }
                    }
                }

                c.visit_children_with(analyzer);

                Ok(())
            })
            .report(&mut self.storage);

        self.scope.this = old_this;

        Ok(())
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, c: &RClassDecl) -> VResult<()> {
        let ctx = Ctx {
            in_declare: self.ctx.in_declare || c.declare,
            ..self.ctx
        };
        self.with_ctx(ctx).visit_class_decl_inner(c);

        Ok(())
    }
}

impl Analyzer<'_, '_> {
    /// This method combines setters and getters, and merge it just like a
    /// normal property.
    fn combine_class_properties(&mut self, body: Vec<(usize, ClassMember)>) -> Vec<(usize, ClassMember)> {
        let mut getters = vec![];
        let mut setters = vec![];

        for (_, body) in &body {
            if let ClassMember::Property(ClassProperty {
                accessor: Accessor {
                    setter: true,
                    getter: true,
                },
                ..
            }) = body
            {
                unreachable!("At this moment, getters and setters should not be combined")
            }

            if let ClassMember::Property(ClassProperty {
                key,
                accessor: Accessor { setter: true, .. },
                ..
            }) = body
            {
                setters.push(key.clone());
            }

            if let ClassMember::Property(ClassProperty {
                key,
                accessor: Accessor { getter: true, .. },
                ..
            }) = body
            {
                getters.push(key.clone());
            }
        }

        // TODO(kdy1): Optimize if intersection is empty
        if getters.is_empty() && setters.is_empty() {
            return body;
        }

        body.into_iter()
            .filter_map(|(idx, mut member)| {
                // We combine setters into getters.

                match member {
                    ClassMember::Property(ClassProperty {
                        ref key,
                        accessor:
                            Accessor {
                                getter: true,
                                ref mut setter,
                            },
                        ..
                    }) => {
                        if setters.iter().any(|setter_key| setter_key.type_eq(key)) {
                            *setter = true;
                        }

                        Some((idx, member))
                    }
                    ClassMember::Property(ClassProperty {
                        ref key,
                        accessor: Accessor { setter: true, .. },
                        ..
                    }) => {
                        if getters.iter().any(|getter_key| getter_key.type_eq(key)) {
                            return None;
                        }

                        Some((idx, member))
                    }
                    _ => Some((idx, member)),
                }
            })
            .collect()
    }

    /// If a class have an index signature, properties should be compatible with
    /// it.
    fn report_errors_for_class_member_incompatible_with_index_signature(&mut self, class: &ClassDef) -> VResult<()> {
        let index = match self
            .get_index_signature_from_class(class.span, class)
            .context("tried to get index signature from a class")?
        {
            Some(v) => v,
            None => return Ok(()),
        };
        let index_ret_ty = match &index.type_ann {
            Some(v) => v,
            // It's `any`, so we don't have to verify.
            None => return Ok(()),
        };

        for member in &class.body {
            if let ClassMember::Property(ClassProperty {
                key, value: Some(value), ..
            }) = member
            {
                let span = key.span();

                if !index.params[0].ty.is_kwd(TsKeywordTypeKind::TsStringKeyword)
                    && self.assign(span, &mut Default::default(), &index.params[0].ty, &key.ty()).is_err()
                {
                    continue;
                }

                self.assign_with_opts(
                    &mut Default::default(),
                    index_ret_ty,
                    value,
                    AssignOpts {
                        span,
                        ..Default::default()
                    },
                )
                .convert_err(|_err| {
                    if index.params[0].ty.is_kwd(TsKeywordTypeKind::TsNumberKeyword) {
                        ErrorKind::ClassMemberNotCompatibleWithNumericIndexSignature { span }
                    } else {
                        ErrorKind::ClassMemberNotCompatibleWithStringIndexSignature { span }
                    }
                })?;
            }
        }

        Ok(())
    }

    fn report_errors_for_wrong_constructor_overloads(
        &mut self,
        ambient: &[ConstructorSignature],
        cons_with_body: Option<&ConstructorSignature>,
    ) -> VResult<()> {
        if let Some(i) = cons_with_body {
            for ambient in ambient {
                self.assign_to_fn_like(
                    &mut Default::default(),
                    false,
                    ambient.type_params.as_ref(),
                    &ambient.params,
                    ambient.ret_ty.as_deref(),
                    i.type_params.as_ref(),
                    &i.params,
                    i.ret_ty.as_deref(),
                    AssignOpts {
                        span: i.span,
                        for_overload: true,
                        ..Default::default()
                    },
                )
                .convert_err(|err| ErrorKind::WrongOverloadSignature { span: err.span() })?;
            }
        }

        Ok(())
    }

    fn validate_super_class(&mut self, span: Span, ty: &Type) {
        if self.config.is_builtin {
            return;
        }

        let res: VResult<_> = try {
            if let Type::Ref(Ref {
                type_name: RTsEntityName::Ident(i),
                ..
            }) = ty.normalize()
            {
                if let Some(name) = &self.scope.this_class_name {
                    if *name == i {
                        Err(ErrorKind::SelfReferentialSuperClass { span: i.span })?
                    }
                }
            }

            let ty = self.normalize(Some(span), Cow::Borrowed(ty), Default::default())?;

            if let Type::Function(..) = ty.normalize() {
                Err(ErrorKind::NotConstructorType { span: ty.span() })?
            }
        };

        res.report(&mut self.storage);
    }

    /// TODO(kdy1): Instantiate fully
    pub(crate) fn instantiate_class(&mut self, span: Span, ty: &Type) -> VResult<Type> {
        let span = span.with_ctxt(SyntaxContext::empty());

        Ok(match ty.normalize() {
            Type::ClassDef(def) => Type::Class(Class {
                span,
                def: def.clone(),
                metadata: Default::default(),
                tracker: Default::default(),
            }),
            _ => ty.clone(),
        })
    }

    fn visit_class_decl_inner(&mut self, c: &RClassDecl) {
        c.ident.visit_with(self);

        self.scope.this_class_name = Some(c.ident.clone().into());
        let ty = match c.class.validate_with_args(self, None) {
            Ok(ty) => ty.into(),
            Err(err) => {
                self.storage.report(err);
                Type::any(c.span(), Default::default())
            }
        };
        let ty = ty.freezed();

        let old_this = self.scope.this.take();
        // self.scope.this = Some(ty.clone());

        let ty = self.register_type(c.ident.clone().into(), ty);

        match self.declare_var(
            ty.span(),
            VarKind::Class,
            c.ident.clone().into(),
            Some(ty),
            None,
            // initialized = true
            true,
            // declare Class does not allow multiple declarations.
            false,
            false,
            false,
        ) {
            Ok(..) => {}
            Err(err) => {
                self.storage.report(err);
            }
        }

        self.scope.this = old_this;
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, b: &RStaticBlock) {
        self.with_child(ScopeKind::ClassStaticBlock, Default::default(), |analyzer| {
            analyzer.ctx.in_static_block = true;

            b.body.stmts.visit_with(analyzer);
            Ok(())
        })?;

        Ok(())
    }
}
