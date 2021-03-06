// Copyright 2012-2013 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Error Reporting Code for the inference engine
//!
//! Because of the way inference, and in particular region inference,
//! works, it often happens that errors are not detected until far after
//! the relevant line of code has been type-checked. Therefore, there is
//! an elaborate system to track why a particular constraint in the
//! inference graph arose so that we can explain to the user what gave
//! rise to a particular error.
//!
//! The basis of the system are the "origin" types. An "origin" is the
//! reason that a constraint or inference variable arose. There are
//! different "origin" enums for different kinds of constraints/variables
//! (e.g., `TypeOrigin`, `RegionVariableOrigin`). An origin always has
//! a span, but also more information so that we can generate a meaningful
//! error message.
//!
//! Having a catalogue of all the different reasons an error can arise is
//! also useful for other reasons, like cross-referencing FAQs etc, though
//! we are not really taking advantage of this yet.
//!
//! # Region Inference
//!
//! Region inference is particularly tricky because it always succeeds "in
//! the moment" and simply registers a constraint. Then, at the end, we
//! can compute the full graph and report errors, so we need to be able to
//! store and later report what gave rise to the conflicting constraints.
//!
//! # Subtype Trace
//!
//! Determining whether `T1 <: T2` often involves a number of subtypes and
//! subconstraints along the way. A "TypeTrace" is an extended version
//! of an origin that traces the types and other values that were being
//! compared. It is not necessarily comprehensive (in fact, at the time of
//! this writing it only tracks the root values being compared) but I'd
//! like to extend it to include significant "waypoints". For example, if
//! you are comparing `(T1, T2) <: (T3, T4)`, and the problem is that `T2
//! <: T4` fails, I'd like the trace to include enough information to say
//! "in the 2nd element of the tuple". Similarly, failures when comparing
//! arguments or return types in fn types should be able to cite the
//! specific position, etc.
//!
//! # Reality vs plan
//!
//! Of course, there is still a LOT of code in typeck that has yet to be
//! ported to this system, and which relies on string concatenation at the
//! time of error detection.

use super::InferCtxt;
use super::TypeTrace;
use super::SubregionOrigin;
use super::RegionVariableOrigin;
use super::ValuePairs;
use super::region_inference::RegionResolutionError;
use super::region_inference::ConcreteFailure;
use super::region_inference::SubSupConflict;
use super::region_inference::GenericBoundFailure;
use super::region_inference::GenericKind;
use super::region_inference::ProcessedErrors;
use super::region_inference::ProcessedErrorOrigin;
use super::region_inference::SameRegions;

use hir::map as hir_map;
use hir;

use lint;
use hir::def_id::DefId;
use infer;
use middle::region;
use traits::{ObligationCause, ObligationCauseCode};
use ty::{self, TyCtxt, TypeFoldable};
use ty::{Region, ReFree};
use ty::error::TypeError;

use std::fmt;
use syntax::ast;
use syntax_pos::{Pos, Span};
use errors::DiagnosticBuilder;

impl<'a, 'gcx, 'tcx> TyCtxt<'a, 'gcx, 'tcx> {
    pub fn note_and_explain_region(self,
                                   err: &mut DiagnosticBuilder,
                                   prefix: &str,
                                   region: &'tcx ty::Region,
                                   suffix: &str) {
        fn item_scope_tag(item: &hir::Item) -> &'static str {
            match item.node {
                hir::ItemImpl(..) => "impl",
                hir::ItemStruct(..) => "struct",
                hir::ItemUnion(..) => "union",
                hir::ItemEnum(..) => "enum",
                hir::ItemTrait(..) => "trait",
                hir::ItemFn(..) => "function body",
                _ => "item"
            }
        }

        fn trait_item_scope_tag(item: &hir::TraitItem) -> &'static str {
            match item.node {
                hir::TraitItemKind::Method(..) => "method body",
                hir::TraitItemKind::Const(..) |
                hir::TraitItemKind::Type(..) => "associated item"
            }
        }

        fn impl_item_scope_tag(item: &hir::ImplItem) -> &'static str {
            match item.node {
                hir::ImplItemKind::Method(..) => "method body",
                hir::ImplItemKind::Const(..) |
                hir::ImplItemKind::Type(_) => "associated item"
            }
        }

        fn explain_span<'a, 'gcx, 'tcx>(tcx: TyCtxt<'a, 'gcx, 'tcx>,
                                        heading: &str, span: Span)
                                        -> (String, Option<Span>) {
            let lo = tcx.sess.codemap().lookup_char_pos_adj(span.lo);
            (format!("the {} at {}:{}", heading, lo.line, lo.col.to_usize()),
             Some(span))
        }

        let (description, span) = match *region {
            ty::ReScope(scope) => {
                let new_string;
                let unknown_scope = || {
                    format!("{}unknown scope: {:?}{}.  Please report a bug.",
                            prefix, scope, suffix)
                };
                let span = match scope.span(&self.region_maps, &self.hir) {
                    Some(s) => s,
                    None => {
                        err.note(&unknown_scope());
                        return;
                    }
                };
                let tag = match self.hir.find(scope.node_id(&self.region_maps)) {
                    Some(hir_map::NodeBlock(_)) => "block",
                    Some(hir_map::NodeExpr(expr)) => match expr.node {
                        hir::ExprCall(..) => "call",
                        hir::ExprMethodCall(..) => "method call",
                        hir::ExprMatch(.., hir::MatchSource::IfLetDesugar { .. }) => "if let",
                        hir::ExprMatch(.., hir::MatchSource::WhileLetDesugar) =>  "while let",
                        hir::ExprMatch(.., hir::MatchSource::ForLoopDesugar) =>  "for",
                        hir::ExprMatch(..) => "match",
                        _ => "expression",
                    },
                    Some(hir_map::NodeStmt(_)) => "statement",
                    Some(hir_map::NodeItem(it)) => item_scope_tag(&it),
                    Some(hir_map::NodeTraitItem(it)) => trait_item_scope_tag(&it),
                    Some(hir_map::NodeImplItem(it)) => impl_item_scope_tag(&it),
                    Some(_) | None => {
                        err.span_note(span, &unknown_scope());
                        return;
                    }
                };
                let scope_decorated_tag = match self.region_maps.code_extent_data(scope) {
                    region::CodeExtentData::Misc(_) => tag,
                    region::CodeExtentData::CallSiteScope { .. } => {
                        "scope of call-site for function"
                    }
                    region::CodeExtentData::ParameterScope { .. } => {
                        "scope of function body"
                    }
                    region::CodeExtentData::DestructionScope(_) => {
                        new_string = format!("destruction scope surrounding {}", tag);
                        &new_string[..]
                    }
                    region::CodeExtentData::Remainder(r) => {
                        new_string = format!("block suffix following statement {}",
                                             r.first_statement_index);
                        &new_string[..]
                    }
                };
                explain_span(self, scope_decorated_tag, span)
            }

            ty::ReFree(ref fr) => {
                let prefix = match fr.bound_region {
                    ty::BrAnon(idx) => {
                        format!("the anonymous lifetime #{} defined on", idx + 1)
                    }
                    ty::BrFresh(_) => "an anonymous lifetime defined on".to_owned(),
                    _ => {
                        format!("the lifetime {} as defined on",
                                fr.bound_region)
                    }
                };

                let node = fr.scope.node_id(&self.region_maps);
                let unknown;
                let tag = match self.hir.find(node) {
                    Some(hir_map::NodeBlock(_)) |
                    Some(hir_map::NodeExpr(_)) => "body",
                    Some(hir_map::NodeItem(it)) => item_scope_tag(&it),
                    Some(hir_map::NodeTraitItem(it)) => trait_item_scope_tag(&it),
                    Some(hir_map::NodeImplItem(it)) => impl_item_scope_tag(&it),

                    // this really should not happen, but it does:
                    // FIXME(#27942)
                    Some(_) => {
                        unknown = format!("unexpected node ({}) for scope {:?}.  \
                                           Please report a bug.",
                                          self.hir.node_to_string(node), fr.scope);
                        &unknown
                    }
                    None => {
                        unknown = format!("unknown node for scope {:?}.  \
                                           Please report a bug.", fr.scope);
                        &unknown
                    }
                };
                let (msg, opt_span) = explain_span(self, tag, self.hir.span(node));
                (format!("{} {}", prefix, msg), opt_span)
            }

            ty::ReStatic => ("the static lifetime".to_owned(), None),

            ty::ReEmpty => ("the empty lifetime".to_owned(), None),

            ty::ReEarlyBound(ref data) => (data.name.to_string(), None),

            // FIXME(#13998) ReSkolemized should probably print like
            // ReFree rather than dumping Debug output on the user.
            //
            // We shouldn't really be having unification failures with ReVar
            // and ReLateBound though.
            ty::ReSkolemized(..) |
            ty::ReVar(_) |
            ty::ReLateBound(..) |
            ty::ReErased => {
                (format!("lifetime {:?}", region), None)
            }
        };
        let message = format!("{}{}{}", prefix, description, suffix);
        if let Some(span) = span {
            err.span_note(span, &message);
        } else {
            err.note(&message);
        }
    }
}

impl<'a, 'gcx, 'tcx> InferCtxt<'a, 'gcx, 'tcx> {
    pub fn report_region_errors(&self,
                                errors: &Vec<RegionResolutionError<'tcx>>) {
        debug!("report_region_errors(): {} errors to start", errors.len());

        // try to pre-process the errors, which will group some of them
        // together into a `ProcessedErrors` group:
        let processed_errors = self.process_errors(errors);
        let errors = processed_errors.as_ref().unwrap_or(errors);

        debug!("report_region_errors: {} errors after preprocessing", errors.len());

        for error in errors {
            debug!("report_region_errors: error = {:?}", error);
            match error.clone() {
                ConcreteFailure(origin, sub, sup) => {
                    self.report_concrete_failure(origin, sub, sup).emit();
                }

                GenericBoundFailure(kind, param_ty, sub) => {
                    self.report_generic_bound_failure(kind, param_ty, sub);
                }

                SubSupConflict(var_origin,
                               sub_origin, sub_r,
                               sup_origin, sup_r) => {
                    self.report_sub_sup_conflict(var_origin,
                                                 sub_origin, sub_r,
                                                 sup_origin, sup_r);
                }

                ProcessedErrors(ref origins,
                                ref same_regions) => {
                    if !same_regions.is_empty() {
                        self.report_processed_errors(origins);
                    }
                }
            }
        }
    }

    // This method goes through all the errors and try to group certain types
    // of error together, for the purpose of suggesting explicit lifetime
    // parameters to the user. This is done so that we can have a more
    // complete view of what lifetimes should be the same.
    // If the return value is an empty vector, it means that processing
    // failed (so the return value of this method should not be used).
    //
    // The method also attempts to weed out messages that seem like
    // duplicates that will be unhelpful to the end-user. But
    // obviously it never weeds out ALL errors.
    fn process_errors(&self, errors: &Vec<RegionResolutionError<'tcx>>)
                      -> Option<Vec<RegionResolutionError<'tcx>>> {
        debug!("process_errors()");
        let mut origins = Vec::new();

        // we collect up ConcreteFailures and SubSupConflicts that are
        // relating free-regions bound on the fn-header and group them
        // together into this vector
        let mut same_regions = Vec::new();

        // here we put errors that we will not be able to process nicely
        let mut other_errors = Vec::new();

        // we collect up GenericBoundFailures in here.
        let mut bound_failures = Vec::new();

        for error in errors {
            // Check whether we can process this error into some other
            // form; if not, fall through.
            match *error {
                ConcreteFailure(ref origin, sub, sup) => {
                    debug!("processing ConcreteFailure");
                    if let SubregionOrigin::CompareImplMethodObligation { .. } = *origin {
                        // When comparing an impl method against a
                        // trait method, it is not helpful to suggest
                        // changes to the impl method.  This is
                        // because the impl method signature is being
                        // checked using the trait's environment, so
                        // usually the changes we suggest would
                        // actually have to be applied to the *trait*
                        // method (and it's not clear that the trait
                        // method is even under the user's control).
                    } else if let Some(same_frs) = free_regions_from_same_fn(self.tcx, sub, sup) {
                        origins.push(
                            ProcessedErrorOrigin::ConcreteFailure(
                                origin.clone(),
                                sub,
                                sup));
                        append_to_same_regions(&mut same_regions, &same_frs);
                        continue;
                    }
                }
                SubSupConflict(ref var_origin, ref sub_origin, sub, ref sup_origin, sup) => {
                    debug!("processing SubSupConflict sub: {:?} sup: {:?}", sub, sup);
                    match (sub_origin, sup_origin) {
                        (&SubregionOrigin::CompareImplMethodObligation { .. }, _) => {
                            // As above, when comparing an impl method
                            // against a trait method, it is not helpful
                            // to suggest changes to the impl method.
                        }
                        (_, &SubregionOrigin::CompareImplMethodObligation { .. }) => {
                            // See above.
                        }
                        _ => {
                            if let Some(same_frs) = free_regions_from_same_fn(self.tcx, sub, sup) {
                                origins.push(
                                    ProcessedErrorOrigin::VariableFailure(
                                        var_origin.clone()));
                                append_to_same_regions(&mut same_regions, &same_frs);
                                continue;
                            }
                        }
                    }
                }
                GenericBoundFailure(ref origin, ref kind, region) => {
                    bound_failures.push((origin.clone(), kind.clone(), region));
                    continue;
                }
                ProcessedErrors(..) => {
                    bug!("should not encounter a `ProcessedErrors` yet: {:?}", error)
                }
            }

            // No changes to this error.
            other_errors.push(error.clone());
        }

        // ok, let's pull together the errors, sorted in an order that
        // we think will help user the best
        let mut processed_errors = vec![];

        // first, put the processed errors, if any
        if !same_regions.is_empty() {
            let common_scope_id = same_regions[0].scope_id;
            for sr in &same_regions {
                // Since ProcessedErrors is used to reconstruct the function
                // declaration, we want to make sure that they are, in fact,
                // from the same scope
                if sr.scope_id != common_scope_id {
                    debug!("returning empty result from process_errors because
                            {} != {}", sr.scope_id, common_scope_id);
                    return None;
                }
            }
            assert!(origins.len() > 0);
            let pe = ProcessedErrors(origins, same_regions);
            debug!("errors processed: {:?}", pe);
            processed_errors.push(pe);
        }

        // next, put the other misc errors
        processed_errors.extend(other_errors);

        // finally, put the `T: 'a` errors, but only if there were no
        // other errors. otherwise, these have a very high rate of
        // being unhelpful in practice. This is because they are
        // basically secondary checks that test the state of the
        // region graph after the rest of inference is done, and the
        // other kinds of errors indicate that the region constraint
        // graph is internally inconsistent, so these test results are
        // likely to be meaningless.
        if processed_errors.is_empty() {
            for (origin, kind, region) in bound_failures {
                processed_errors.push(GenericBoundFailure(origin, kind, region));
            }
        }

        // we should always wind up with SOME errors, unless there were no
        // errors to start
        assert!(if errors.len() > 0 {processed_errors.len() > 0} else {true});

        return Some(processed_errors);

        #[derive(Debug)]
        struct FreeRegionsFromSameFn {
            sub_fr: ty::FreeRegion,
            sup_fr: ty::FreeRegion,
            scope_id: ast::NodeId
        }

        impl FreeRegionsFromSameFn {
            fn new(sub_fr: ty::FreeRegion,
                   sup_fr: ty::FreeRegion,
                   scope_id: ast::NodeId)
                   -> FreeRegionsFromSameFn {
                FreeRegionsFromSameFn {
                    sub_fr: sub_fr,
                    sup_fr: sup_fr,
                    scope_id: scope_id
                }
            }
        }

        fn free_regions_from_same_fn<'a, 'gcx, 'tcx>(tcx: TyCtxt<'a, 'gcx, 'tcx>,
                                                     sub: &'tcx Region,
                                                     sup: &'tcx Region)
                                                     -> Option<FreeRegionsFromSameFn> {
            debug!("free_regions_from_same_fn(sub={:?}, sup={:?})", sub, sup);
            let (scope_id, fr1, fr2) = match (sub, sup) {
                (&ReFree(fr1), &ReFree(fr2)) => {
                    if fr1.scope != fr2.scope {
                        return None
                    }
                    assert!(fr1.scope == fr2.scope);
                    (fr1.scope.node_id(&tcx.region_maps), fr1, fr2)
                },
                _ => return None
            };
            let parent = tcx.hir.get_parent(scope_id);
            let parent_node = tcx.hir.find(parent);
            match parent_node {
                Some(node) => match node {
                    hir_map::NodeItem(item) => match item.node {
                        hir::ItemFn(..) => {
                            Some(FreeRegionsFromSameFn::new(fr1, fr2, scope_id))
                        },
                        _ => None
                    },
                    hir_map::NodeImplItem(..) |
                    hir_map::NodeTraitItem(..) => {
                        Some(FreeRegionsFromSameFn::new(fr1, fr2, scope_id))
                    },
                    _ => None
                },
                None => {
                    debug!("no parent node of scope_id {}", scope_id);
                    None
                }
            }
        }

        fn append_to_same_regions(same_regions: &mut Vec<SameRegions>,
                                  same_frs: &FreeRegionsFromSameFn) {
            debug!("append_to_same_regions(same_regions={:?}, same_frs={:?})",
                   same_regions, same_frs);
            let scope_id = same_frs.scope_id;
            let (sub_fr, sup_fr) = (same_frs.sub_fr, same_frs.sup_fr);
            for sr in same_regions.iter_mut() {
                if sr.contains(&sup_fr.bound_region) && scope_id == sr.scope_id {
                    sr.push(sub_fr.bound_region);
                    return
                }
            }
            same_regions.push(SameRegions {
                scope_id: scope_id,
                regions: vec![sub_fr.bound_region, sup_fr.bound_region]
            })
        }
    }

    /// Adds a note if the types come from similarly named crates
    fn check_and_note_conflicting_crates(&self,
                                         err: &mut DiagnosticBuilder,
                                         terr: &TypeError<'tcx>,
                                         sp: Span) {
        let report_path_match = |err: &mut DiagnosticBuilder, did1: DefId, did2: DefId| {
            // Only external crates, if either is from a local
            // module we could have false positives
            if !(did1.is_local() || did2.is_local()) && did1.krate != did2.krate {
                let exp_path = self.tcx.item_path_str(did1);
                let found_path = self.tcx.item_path_str(did2);
                // We compare strings because DefPath can be different
                // for imported and non-imported crates
                if exp_path == found_path {
                    let crate_name = self.tcx.sess.cstore.crate_name(did1.krate);
                    err.span_note(sp, &format!("Perhaps two different versions \
                                                of crate `{}` are being used?",
                                               crate_name));
                }
            }
        };
        match *terr {
            TypeError::Sorts(ref exp_found) => {
                // if they are both "path types", there's a chance of ambiguity
                // due to different versions of the same crate
                match (&exp_found.expected.sty, &exp_found.found.sty) {
                    (&ty::TyAdt(exp_adt, _), &ty::TyAdt(found_adt, _)) => {
                        report_path_match(err, exp_adt.did, found_adt.did);
                    },
                    _ => ()
                }
            },
            TypeError::Traits(ref exp_found) => {
                report_path_match(err, exp_found.expected, exp_found.found);
            },
            _ => () // FIXME(#22750) handle traits and stuff
        }
    }

    fn note_error_origin(&self,
                         err: &mut DiagnosticBuilder<'tcx>,
                         cause: &ObligationCause<'tcx>)
    {
        match cause.code {
            ObligationCauseCode::MatchExpressionArm { arm_span, source } => match source {
                hir::MatchSource::IfLetDesugar {..} => {
                    err.span_note(arm_span, "`if let` arm with an incompatible type");
                }
                _ => {
                    err.span_note(arm_span, "match arm with an incompatible type");
                }
            },
            _ => ()
        }
    }

    pub fn note_type_err(&self,
                         diag: &mut DiagnosticBuilder<'tcx>,
                         cause: &ObligationCause<'tcx>,
                         secondary_span: Option<(Span, String)>,
                         values: Option<ValuePairs<'tcx>>,
                         terr: &TypeError<'tcx>)
    {
        let expected_found = match values {
            None => None,
            Some(values) => match self.values_str(&values) {
                Some((expected, found)) => Some((expected, found)),
                None => {
                    // Derived error. Cancel the emitter.
                    self.tcx.sess.diagnostic().cancel(diag);
                    return
                }
            }
        };

        let span = cause.span;

        if let Some((expected, found)) = expected_found {
            let is_simple_error = if let &TypeError::Sorts(ref values) = terr {
                values.expected.is_primitive() && values.found.is_primitive()
            } else {
                false
            };

            if !is_simple_error {
                if expected == found {
                    if let &TypeError::Sorts(ref values) = terr {
                        diag.note_expected_found_extra(
                            &"type", &expected, &found,
                            &format!(" ({})", values.expected.sort_string(self.tcx)),
                            &format!(" ({})", values.found.sort_string(self.tcx)));
                    } else {
                        diag.note_expected_found(&"type", &expected, &found);
                    }
                } else {
                    diag.note_expected_found(&"type", &expected, &found);
                }
            }
        }

        diag.span_label(span, &terr);
        if let Some((sp, msg)) = secondary_span {
            diag.span_label(sp, &msg);
        }

        self.note_error_origin(diag, &cause);
        self.check_and_note_conflicting_crates(diag, terr, span);
        self.tcx.note_and_explain_type_err(diag, terr, span);
    }

    pub fn report_and_explain_type_error(&self,
                                         trace: TypeTrace<'tcx>,
                                         terr: &TypeError<'tcx>)
                                         -> DiagnosticBuilder<'tcx>
    {
        let span = trace.cause.span;
        let failure_str = trace.cause.as_failure_str();
        let mut diag = match trace.cause.code {
            ObligationCauseCode::IfExpressionWithNoElse => {
                struct_span_err!(self.tcx.sess, span, E0317, "{}", failure_str)
            }
            ObligationCauseCode::MainFunctionType => {
                struct_span_err!(self.tcx.sess, span, E0580, "{}", failure_str)
            }
            _ => {
                struct_span_err!(self.tcx.sess, span, E0308, "{}", failure_str)
            }
        };
        self.note_type_err(&mut diag, &trace.cause, None, Some(trace.values), terr);
        diag
    }

    /// Returns a string of the form "expected `{}`, found `{}`".
    fn values_str(&self, values: &ValuePairs<'tcx>) -> Option<(String, String)> {
        match *values {
            infer::Types(ref exp_found) => self.expected_found_str(exp_found),
            infer::TraitRefs(ref exp_found) => self.expected_found_str(exp_found),
            infer::PolyTraitRefs(ref exp_found) => self.expected_found_str(exp_found),
        }
    }

    fn expected_found_str<T: fmt::Display + TypeFoldable<'tcx>>(
        &self,
        exp_found: &ty::error::ExpectedFound<T>)
        -> Option<(String, String)>
    {
        let exp_found = self.resolve_type_vars_if_possible(exp_found);
        if exp_found.references_error() {
            return None;
        }

        Some((format!("{}", exp_found.expected), format!("{}", exp_found.found)))
    }

    fn report_generic_bound_failure(&self,
                                    origin: SubregionOrigin<'tcx>,
                                    bound_kind: GenericKind<'tcx>,
                                    sub: &'tcx Region)
    {
        // FIXME: it would be better to report the first error message
        // with the span of the parameter itself, rather than the span
        // where the error was detected. But that span is not readily
        // accessible.

        let labeled_user_string = match bound_kind {
            GenericKind::Param(ref p) =>
                format!("the parameter type `{}`", p),
            GenericKind::Projection(ref p) =>
                format!("the associated type `{}`", p),
        };

        if let SubregionOrigin::CompareImplMethodObligation {
            span, item_name, impl_item_def_id, trait_item_def_id, lint_id
        } = origin {
            self.report_extra_impl_obligation(span,
                                              item_name,
                                              impl_item_def_id,
                                              trait_item_def_id,
                                              &format!("`{}: {}`", bound_kind, sub),
                                              lint_id)
                .emit();
            return;
        }

        let mut err = match *sub {
            ty::ReFree(ty::FreeRegion {bound_region: ty::BrNamed(..), ..}) => {
                // Does the required lifetime have a nice name we can print?
                let mut err = struct_span_err!(self.tcx.sess,
                                               origin.span(),
                                               E0309,
                                               "{} may not live long enough",
                                               labeled_user_string);
                err.help(&format!("consider adding an explicit lifetime bound `{}: {}`...",
                         bound_kind,
                         sub));
                err
            }

            ty::ReStatic => {
                // Does the required lifetime have a nice name we can print?
                let mut err = struct_span_err!(self.tcx.sess,
                                               origin.span(),
                                               E0310,
                                               "{} may not live long enough",
                                               labeled_user_string);
                err.help(&format!("consider adding an explicit lifetime \
                                   bound `{}: 'static`...",
                                  bound_kind));
                err
            }

            _ => {
                // If not, be less specific.
                let mut err = struct_span_err!(self.tcx.sess,
                                               origin.span(),
                                               E0311,
                                               "{} may not live long enough",
                                               labeled_user_string);
                err.help(&format!("consider adding an explicit lifetime bound for `{}`",
                                  bound_kind));
                self.tcx.note_and_explain_region(
                    &mut err,
                    &format!("{} must be valid for ", labeled_user_string),
                    sub,
                    "...");
                err
            }
        };

        self.note_region_origin(&mut err, &origin);
        err.emit();
    }

    fn report_concrete_failure(&self,
                               origin: SubregionOrigin<'tcx>,
                               sub: &'tcx Region,
                               sup: &'tcx Region)
                                -> DiagnosticBuilder<'tcx> {
        match origin {
            infer::Subtype(trace) => {
                let terr = TypeError::RegionsDoesNotOutlive(sup, sub);
                self.report_and_explain_type_error(trace, &terr)
            }
            infer::Reborrow(span) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0312,
                    "lifetime of reference outlives \
                     lifetime of borrowed content...");
                self.tcx.note_and_explain_region(&mut err,
                    "...the reference is valid for ",
                    sub,
                    "...");
                self.tcx.note_and_explain_region(&mut err,
                    "...but the borrowed content is only valid for ",
                    sup,
                    "");
                err
            }
            infer::ReborrowUpvar(span, ref upvar_id) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0313,
                    "lifetime of borrowed pointer outlives \
                            lifetime of captured variable `{}`...",
                            self.tcx.local_var_name_str(upvar_id.var_id));
                self.tcx.note_and_explain_region(&mut err,
                    "...the borrowed pointer is valid for ",
                    sub,
                    "...");
                self.tcx.note_and_explain_region(&mut err,
                    &format!("...but `{}` is only valid for ",
                             self.tcx.local_var_name_str(upvar_id.var_id)),
                    sup,
                    "");
                err
            }
            infer::InfStackClosure(span) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0314,
                    "closure outlives stack frame");
                self.tcx.note_and_explain_region(&mut err,
                    "...the closure must be valid for ",
                    sub,
                    "...");
                self.tcx.note_and_explain_region(&mut err,
                    "...but the closure's stack frame is only valid for ",
                    sup,
                    "");
                err
            }
            infer::InvokeClosure(span) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0315,
                    "cannot invoke closure outside of its lifetime");
                self.tcx.note_and_explain_region(&mut err,
                    "the closure is only valid for ",
                    sup,
                    "");
                err
            }
            infer::DerefPointer(span) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0473,
                          "dereference of reference outside its lifetime");
                self.tcx.note_and_explain_region(&mut err,
                    "the reference is only valid for ",
                    sup,
                    "");
                err
            }
            infer::FreeVariable(span, id) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0474,
                          "captured variable `{}` does not outlive the enclosing closure",
                          self.tcx.local_var_name_str(id));
                self.tcx.note_and_explain_region(&mut err,
                    "captured variable is valid for ",
                    sup,
                    "");
                self.tcx.note_and_explain_region(&mut err,
                    "closure is valid for ",
                    sub,
                    "");
                err
            }
            infer::IndexSlice(span) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0475,
                          "index of slice outside its lifetime");
                self.tcx.note_and_explain_region(&mut err,
                    "the slice is only valid for ",
                    sup,
                    "");
                err
            }
            infer::RelateObjectBound(span) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0476,
                          "lifetime of the source pointer does not outlive \
                           lifetime bound of the object type");
                self.tcx.note_and_explain_region(&mut err,
                    "object type is valid for ",
                    sub,
                    "");
                self.tcx.note_and_explain_region(&mut err,
                    "source pointer is only valid for ",
                    sup,
                    "");
                err
            }
            infer::RelateParamBound(span, ty) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0477,
                          "the type `{}` does not fulfill the required lifetime",
                          self.ty_to_string(ty));
                self.tcx.note_and_explain_region(&mut err,
                                        "type must outlive ",
                                        sub,
                                        "");
                err
            }
            infer::RelateRegionParamBound(span) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0478,
                          "lifetime bound not satisfied");
                self.tcx.note_and_explain_region(&mut err,
                    "lifetime parameter instantiated with ",
                    sup,
                    "");
                self.tcx.note_and_explain_region(&mut err,
                    "but lifetime parameter must outlive ",
                    sub,
                    "");
                err
            }
            infer::RelateDefaultParamBound(span, ty) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0479,
                          "the type `{}` (provided as the value of \
                           a type parameter) is not valid at this point",
                          self.ty_to_string(ty));
                self.tcx.note_and_explain_region(&mut err,
                                        "type must outlive ",
                                        sub,
                                        "");
                err
            }
            infer::CallRcvr(span) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0480,
                          "lifetime of method receiver does not outlive \
                           the method call");
                self.tcx.note_and_explain_region(&mut err,
                    "the receiver is only valid for ",
                    sup,
                    "");
                err
            }
            infer::CallArg(span) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0481,
                          "lifetime of function argument does not outlive \
                           the function call");
                self.tcx.note_and_explain_region(&mut err,
                    "the function argument is only valid for ",
                    sup,
                    "");
                err
            }
            infer::CallReturn(span) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0482,
                          "lifetime of return value does not outlive \
                           the function call");
                self.tcx.note_and_explain_region(&mut err,
                    "the return value is only valid for ",
                    sup,
                    "");
                err
            }
            infer::Operand(span) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0483,
                          "lifetime of operand does not outlive \
                           the operation");
                self.tcx.note_and_explain_region(&mut err,
                    "the operand is only valid for ",
                    sup,
                    "");
                err
            }
            infer::AddrOf(span) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0484,
                          "reference is not valid at the time of borrow");
                self.tcx.note_and_explain_region(&mut err,
                    "the borrow is only valid for ",
                    sup,
                    "");
                err
            }
            infer::AutoBorrow(span) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0485,
                          "automatically reference is not valid \
                           at the time of borrow");
                self.tcx.note_and_explain_region(&mut err,
                    "the automatic borrow is only valid for ",
                    sup,
                    "");
                err
            }
            infer::ExprTypeIsNotInScope(t, span) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0486,
                          "type of expression contains references \
                           that are not valid during the expression: `{}`",
                          self.ty_to_string(t));
                self.tcx.note_and_explain_region(&mut err,
                    "type is only valid for ",
                    sup,
                    "");
                err
            }
            infer::SafeDestructor(span) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0487,
                          "unsafe use of destructor: destructor might be called \
                           while references are dead");
                // FIXME (22171): terms "super/subregion" are suboptimal
                self.tcx.note_and_explain_region(&mut err,
                    "superregion: ",
                    sup,
                    "");
                self.tcx.note_and_explain_region(&mut err,
                    "subregion: ",
                    sub,
                    "");
                err
            }
            infer::BindingTypeIsNotValidAtDecl(span) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0488,
                          "lifetime of variable does not enclose its declaration");
                self.tcx.note_and_explain_region(&mut err,
                    "the variable is only valid for ",
                    sup,
                    "");
                err
            }
            infer::ParameterInScope(_, span) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0489,
                          "type/lifetime parameter not in scope here");
                self.tcx.note_and_explain_region(&mut err,
                    "the parameter is only valid for ",
                    sub,
                    "");
                err
            }
            infer::DataBorrowed(ty, span) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0490,
                          "a value of type `{}` is borrowed for too long",
                          self.ty_to_string(ty));
                self.tcx.note_and_explain_region(&mut err, "the type is valid for ", sub, "");
                self.tcx.note_and_explain_region(&mut err, "but the borrow lasts for ", sup, "");
                err
            }
            infer::ReferenceOutlivesReferent(ty, span) => {
                let mut err = struct_span_err!(self.tcx.sess, span, E0491,
                          "in type `{}`, reference has a longer lifetime \
                           than the data it references",
                          self.ty_to_string(ty));
                self.tcx.note_and_explain_region(&mut err,
                    "the pointer is valid for ",
                    sub,
                    "");
                self.tcx.note_and_explain_region(&mut err,
                    "but the referenced data is only valid for ",
                    sup,
                    "");
                err
            }
            infer::CompareImplMethodObligation { span,
                                                 item_name,
                                                 impl_item_def_id,
                                                 trait_item_def_id,
                                                 lint_id } => {
                self.report_extra_impl_obligation(span,
                                                  item_name,
                                                  impl_item_def_id,
                                                  trait_item_def_id,
                                                  &format!("`{}: {}`", sup, sub),
                                                  lint_id)
            }
        }
    }

    fn report_sub_sup_conflict(&self,
                               var_origin: RegionVariableOrigin,
                               sub_origin: SubregionOrigin<'tcx>,
                               sub_region: &'tcx Region,
                               sup_origin: SubregionOrigin<'tcx>,
                               sup_region: &'tcx Region) {
        let mut err = self.report_inference_failure(var_origin);

        self.tcx.note_and_explain_region(&mut err,
            "first, the lifetime cannot outlive ",
            sup_region,
            "...");

        self.note_region_origin(&mut err, &sup_origin);

        self.tcx.note_and_explain_region(&mut err,
            "but, the lifetime must be valid for ",
            sub_region,
            "...");

        self.note_region_origin(&mut err, &sub_origin);
        err.emit();
    }

    fn report_processed_errors(&self,
                               origins: &[ProcessedErrorOrigin<'tcx>]) {
        for origin in origins.iter() {
            let mut err = match *origin {
                ProcessedErrorOrigin::VariableFailure(ref var_origin) =>
                    self.report_inference_failure(var_origin.clone()),
                ProcessedErrorOrigin::ConcreteFailure(ref sr_origin, sub, sup) =>
                    self.report_concrete_failure(sr_origin.clone(), sub, sup),
            };

            err.emit();
        }
    }

    pub fn issue_32330_warnings(&self, span: Span, issue32330s: &[ty::Issue32330]) {
        for issue32330 in issue32330s {
            match *issue32330 {
                ty::Issue32330::WontChange => { }
                ty::Issue32330::WillChange { fn_def_id, region_name } => {
                    self.tcx.sess.add_lint(
                        lint::builtin::HR_LIFETIME_IN_ASSOC_TYPE,
                        ast::CRATE_NODE_ID,
                        span,
                        format!("lifetime parameter `{0}` declared on fn `{1}` \
                                 appears only in the return type, \
                                 but here is required to be higher-ranked, \
                                 which means that `{0}` must appear in both \
                                 argument and return types",
                                region_name,
                                self.tcx.item_path_str(fn_def_id)));
                }
            }
        }
    }
}

impl<'a, 'gcx, 'tcx> InferCtxt<'a, 'gcx, 'tcx> {
    fn report_inference_failure(&self,
                                var_origin: RegionVariableOrigin)
                                -> DiagnosticBuilder<'tcx> {
        let br_string = |br: ty::BoundRegion| {
            let mut s = br.to_string();
            if !s.is_empty() {
                s.push_str(" ");
            }
            s
        };
        let var_description = match var_origin {
            infer::MiscVariable(_) => "".to_string(),
            infer::PatternRegion(_) => " for pattern".to_string(),
            infer::AddrOfRegion(_) => " for borrow expression".to_string(),
            infer::Autoref(_) => " for autoref".to_string(),
            infer::Coercion(_) => " for automatic coercion".to_string(),
            infer::LateBoundRegion(_, br, infer::FnCall) => {
                format!(" for lifetime parameter {}in function call",
                        br_string(br))
            }
            infer::LateBoundRegion(_, br, infer::HigherRankedType) => {
                format!(" for lifetime parameter {}in generic type", br_string(br))
            }
            infer::LateBoundRegion(_, br, infer::AssocTypeProjection(type_name)) => {
                format!(" for lifetime parameter {}in trait containing associated type `{}`",
                        br_string(br), type_name)
            }
            infer::EarlyBoundRegion(_, name) => {
                format!(" for lifetime parameter `{}`",
                        name)
            }
            infer::BoundRegionInCoherence(name) => {
                format!(" for lifetime parameter `{}` in coherence check",
                        name)
            }
            infer::UpvarRegion(ref upvar_id, _) => {
                format!(" for capture of `{}` by closure",
                        self.tcx.local_var_name_str(upvar_id.var_id).to_string())
            }
        };

        struct_span_err!(self.tcx.sess, var_origin.span(), E0495,
                  "cannot infer an appropriate lifetime{} \
                   due to conflicting requirements",
                  var_description)
    }

    fn note_region_origin(&self, err: &mut DiagnosticBuilder, origin: &SubregionOrigin<'tcx>) {
        match *origin {
            infer::Subtype(ref trace) => {
                if let Some((expected, found)) = self.values_str(&trace.values) {
                    // FIXME: do we want a "the" here?
                    err.span_note(
                        trace.cause.span,
                        &format!("...so that {} (expected {}, found {})",
                                 trace.cause.as_requirement_str(), expected, found));
                } else {
                    // FIXME: this really should be handled at some earlier stage. Our
                    // handling of region checking when type errors are present is
                    // *terrible*.

                    err.span_note(
                        trace.cause.span,
                        &format!("...so that {}",
                                 trace.cause.as_requirement_str()));
                }
            }
            infer::Reborrow(span) => {
                err.span_note(
                    span,
                    "...so that reference does not outlive \
                    borrowed content");
            }
            infer::ReborrowUpvar(span, ref upvar_id) => {
                err.span_note(
                    span,
                    &format!(
                        "...so that closure can access `{}`",
                        self.tcx.local_var_name_str(upvar_id.var_id)
                            .to_string()));
            }
            infer::InfStackClosure(span) => {
                err.span_note(
                    span,
                    "...so that closure does not outlive its stack frame");
            }
            infer::InvokeClosure(span) => {
                err.span_note(
                    span,
                    "...so that closure is not invoked outside its lifetime");
            }
            infer::DerefPointer(span) => {
                err.span_note(
                    span,
                    "...so that pointer is not dereferenced \
                    outside its lifetime");
            }
            infer::FreeVariable(span, id) => {
                err.span_note(
                    span,
                    &format!("...so that captured variable `{}` \
                            does not outlive the enclosing closure",
                            self.tcx.local_var_name_str(id)));
            }
            infer::IndexSlice(span) => {
                err.span_note(
                    span,
                    "...so that slice is not indexed outside the lifetime");
            }
            infer::RelateObjectBound(span) => {
                err.span_note(
                    span,
                    "...so that it can be closed over into an object");
            }
            infer::CallRcvr(span) => {
                err.span_note(
                    span,
                    "...so that method receiver is valid for the method call");
            }
            infer::CallArg(span) => {
                err.span_note(
                    span,
                    "...so that argument is valid for the call");
            }
            infer::CallReturn(span) => {
                err.span_note(
                    span,
                    "...so that return value is valid for the call");
            }
            infer::Operand(span) => {
                err.span_note(
                    span,
                    "...so that operand is valid for operation");
            }
            infer::AddrOf(span) => {
                err.span_note(
                    span,
                    "...so that reference is valid \
                     at the time of borrow");
            }
            infer::AutoBorrow(span) => {
                err.span_note(
                    span,
                    "...so that auto-reference is valid \
                     at the time of borrow");
            }
            infer::ExprTypeIsNotInScope(t, span) => {
                err.span_note(
                    span,
                    &format!("...so type `{}` of expression is valid during the \
                             expression",
                            self.ty_to_string(t)));
            }
            infer::BindingTypeIsNotValidAtDecl(span) => {
                err.span_note(
                    span,
                    "...so that variable is valid at time of its declaration");
            }
            infer::ParameterInScope(_, span) => {
                err.span_note(
                    span,
                    "...so that a type/lifetime parameter is in scope here");
            }
            infer::DataBorrowed(ty, span) => {
                err.span_note(
                    span,
                    &format!("...so that the type `{}` is not borrowed for too long",
                             self.ty_to_string(ty)));
            }
            infer::ReferenceOutlivesReferent(ty, span) => {
                err.span_note(
                    span,
                    &format!("...so that the reference type `{}` \
                             does not outlive the data it points at",
                            self.ty_to_string(ty)));
            }
            infer::RelateParamBound(span, t) => {
                err.span_note(
                    span,
                    &format!("...so that the type `{}` \
                             will meet its required lifetime bounds",
                            self.ty_to_string(t)));
            }
            infer::RelateDefaultParamBound(span, t) => {
                err.span_note(
                    span,
                    &format!("...so that type parameter \
                             instantiated with `{}`, \
                             will meet its declared lifetime bounds",
                            self.ty_to_string(t)));
            }
            infer::RelateRegionParamBound(span) => {
                err.span_note(
                    span,
                    "...so that the declared lifetime parameter bounds \
                                are satisfied");
            }
            infer::SafeDestructor(span) => {
                err.span_note(
                    span,
                    "...so that references are valid when the destructor \
                     runs");
            }
            infer::CompareImplMethodObligation { span, .. } => {
                err.span_note(
                    span,
                    "...so that the definition in impl matches the definition from the trait");
            }
        }
    }
}

impl<'tcx> ObligationCause<'tcx> {
    fn as_failure_str(&self) -> &'static str {
        use traits::ObligationCauseCode::*;
        match self.code {
            CompareImplMethodObligation { .. } => "method not compatible with trait",
            MatchExpressionArm { source, .. } => match source {
                hir::MatchSource::IfLetDesugar{..} => "`if let` arms have incompatible types",
                _ => "match arms have incompatible types",
            },
            IfExpression => "if and else have incompatible types",
            IfExpressionWithNoElse => "if may be missing an else clause",
            EquatePredicate => "equality predicate not satisfied",
            MainFunctionType => "main function has wrong type",
            StartFunctionType => "start function has wrong type",
            IntrinsicType => "intrinsic has wrong type",
            MethodReceiver => "mismatched method receiver",
            _ => "mismatched types",
        }
    }

    fn as_requirement_str(&self) -> &'static str {
        use traits::ObligationCauseCode::*;
        match self.code {
            CompareImplMethodObligation { .. } => "method type is compatible with trait",
            ExprAssignable => "expression is assignable",
            MatchExpressionArm { source, .. } => match source {
                hir::MatchSource::IfLetDesugar{..} => "`if let` arms have compatible types",
                _ => "match arms have compatible types",
            },
            IfExpression => "if and else have compatible types",
            IfExpressionWithNoElse => "if missing an else returns ()",
            EquatePredicate => "equality where clause is satisfied",
            MainFunctionType => "`main` function has the correct type",
            StartFunctionType => "`start` function has the correct type",
            IntrinsicType => "intrinsic has the correct type",
            MethodReceiver => "method receiver has the correct type",
            _ => "types are compatible",
        }
    }
}
