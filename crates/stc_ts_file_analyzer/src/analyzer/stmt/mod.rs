use std::time::Instant;

use rnode::VisitWith;
use stc_ts_ast_rnode::{RBlockStmt, RBool, RDecl, RExpr, RExprStmt, RForStmt, RModuleItem, RStmt, RTsExprWithTypeArgs, RTsLit, RWithStmt};
use stc_ts_errors::{DebugExt, ErrorKind};
use stc_ts_types::{LitType, Type};
use stc_utils::{dev_span, stack};
use swc_common::{Spanned, DUMMY_SP};
use swc_ecma_utils::Value::Known;
use tracing::{trace, warn};

use self::return_type::LoopBreakerFinder;
use crate::{
    analyzer::{scope::ScopeKind, util::ResultExt, Analyzer},
    validator,
    validator::ValidateWith,
};

mod ambient_decl;
mod loops;
pub(crate) mod return_type;
mod try_catch;
mod var_decl;

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, i: &RModuleItem) {
        let _stack = stack::start(100);

        i.visit_children_with(self);

        Ok(())
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, s: &RStmt) {
        let span = s.span();
        let line_col = self.line_col(span);

        let _tracing = dev_span!("Stmt", line_col = &*line_col);

        warn!("Statement start");
        let start = Instant::now();

        if self.rule().always_strict && !self.rule().allow_unreachable_code && self.ctx.in_unreachable {
            if !matches!(s, RStmt::Decl(RDecl::TsInterface(..) | RDecl::TsTypeAlias(..))) {
                self.storage.report(ErrorKind::UnreachableCode { span: s.span() }.into());
            }
        }

        let old_in_conditional = self.scope.return_values.in_conditional;
        self.scope.return_values.in_conditional |= matches!(s, RStmt::If(_) | RStmt::Switch(_));

        s.visit_children_with(self);

        self.scope.return_values.in_conditional = old_in_conditional;

        let end = Instant::now();

        warn!(
            kind = "perf",
            op = "validate (Stmt)",
            "({}): Statement validation done. (time = {:?}",
            line_col,
            end - start
        );

        Ok(())
    }
}

impl Analyzer<'_, '_> {
    fn check_for_infinite_loop(&mut self, test: &Type, body: &RStmt) {
        trace!("Checking for infinite loop");

        // Of `s` is always executed and we enter infinite loop, return type should be
        // never
        if !self.scope.return_values.in_conditional {
            let mut v = LoopBreakerFinder { found: false };
            body.visit_with(&mut v);
            let has_break = v.found;
            if !has_break {
                if let Known(v) = test.as_bool() {
                    self.ctx.in_unreachable = true;
                }
            }
        }
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, node: &RForStmt) {
        node.init.visit_with(self);

        let test = try_opt!(node.test.validate_with_default(self));
        let always_true = Type::Lit(LitType {
            span: node.span,
            lit: RTsLit::Bool(RBool {
                span: DUMMY_SP,
                value: true,
            }),
            metadata: Default::default(),
            tracker: Default::default(),
        });

        node.update.visit_with(self);
        node.body.validate_with(self)?;

        self.check_for_infinite_loop(test.as_ref().unwrap_or(&always_true), &node.body);

        Ok(())
    }
}

/// NOTE: We does **not** dig into with statements.
#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, s: &RWithStmt) {
        self.storage.report(ErrorKind::WithStmtNotSupported { span: s.span }.into());

        s.obj.visit_with(self);

        Ok(())
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, s: &RBlockStmt) {
        self.with_child(ScopeKind::Block, Default::default(), |analyzer| {
            s.stmts.visit_with(analyzer);
            Ok(())
        })?;

        Ok(())
    }
}

impl Analyzer<'_, '_> {
    /// Validate that parent interfaces are all resolved.
    pub(super) fn resolve_parent_interfaces(&mut self, parents: &[RTsExprWithTypeArgs], is_for_interface: bool) {
        let _tracing = dev_span!("resolve_parent_interfaces");

        if self.config.is_builtin {
            return;
        }

        for parent in parents {
            // Verify parent interface
            let res: Result<_, _> = try {
                let type_args = try_opt!(parent.type_args.validate_with(self));
                let span = parent.span;

                self.report_error_for_unresolved_type(span, &parent.expr, type_args.as_ref())
                    .convert_err(|err| match err {
                        ErrorKind::TypeNotFound {
                            name,
                            ctxt,
                            type_args,
                            span,
                        } if is_for_interface => ErrorKind::NotExtendableType { span },
                        _ => err,
                    })?;
            };

            res.report(&mut self.storage);
        }
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, node: &RExprStmt) {
        let preserve_cond_facts = !matches!(&*node.expr, RExpr::Call(..));

        let prev_cond_facts = self.cur_facts.clone();

        node.expr.visit_with(self);

        if preserve_cond_facts {
            self.cur_facts = prev_cond_facts;
        }

        Ok(())
    }
}
