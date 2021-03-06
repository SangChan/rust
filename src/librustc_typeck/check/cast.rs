//! Code for type-checking cast expressions.
//!
//! A cast `e as U` is valid if one of the following holds:
//! * `e` has type `T` and `T` coerces to `U`; *coercion-cast*
//! * `e` has type `*T`, `U` is `*U_0`, and either `U_0: Sized` or
//!    pointer_kind(`T`) = pointer_kind(`U_0`); *ptr-ptr-cast*
//! * `e` has type `*T` and `U` is a numeric type, while `T: Sized`; *ptr-addr-cast*
//! * `e` is an integer and `U` is `*U_0`, while `U_0: Sized`; *addr-ptr-cast*
//! * `e` has type `T` and `T` and `U` are any numeric types; *numeric-cast*
//! * `e` is a C-like enum and `U` is an integer type; *enum-cast*
//! * `e` has type `bool` or `char` and `U` is an integer; *prim-int-cast*
//! * `e` has type `u8` and `U` is `char`; *u8-char-cast*
//! * `e` has type `&[T; n]` and `U` is `*const T`; *array-ptr-cast*
//! * `e` is a function pointer type and `U` has type `*T`,
//!   while `T: Sized`; *fptr-ptr-cast*
//! * `e` is a function pointer type and `U` is an integer; *fptr-addr-cast*
//!
//! where `&.T` and `*T` are references of either mutability,
//! and where pointer_kind(`T`) is the kind of the unsize info
//! in `T` - the vtable for a trait definition (e.g., `fmt::Display` or
//! `Iterator`, not `Iterator<Item=u8>`) or a length (or `()` if `T: Sized`).
//!
//! Note that lengths are not adjusted when casting raw slices -
//! `T: *const [u16] as *const [u8]` creates a slice that only includes
//! half of the original memory.
//!
//! Casting is not transitive, that is, even if `e as U1 as U2` is a valid
//! expression, `e as U2` is not necessarily so (in fact it will only be valid if
//! `U1` coerces to `U2`).

use super::FnCtxt;

use errors::{DiagnosticBuilder,Applicability};
use hir::def_id::DefId;
use lint;
use rustc::hir;
use rustc::session::Session;
use rustc::traits;
use rustc::ty::{self, Ty, TypeFoldable, TypeAndMut};
use rustc::ty::adjustment::AllowTwoPhase;
use rustc::ty::cast::{CastKind, CastTy};
use rustc::ty::subst::Substs;
use rustc::middle::lang_items;
use syntax::ast;
use syntax_pos::Span;
use util::common::ErrorReported;

/// Reifies a cast check to be checked once we have full type information for
/// a function context.
pub struct CastCheck<'tcx> {
    expr: &'tcx hir::Expr,
    expr_ty: Ty<'tcx>,
    cast_ty: Ty<'tcx>,
    cast_span: Span,
    span: Span,
}

/// The kind of pointer and associated metadata (thin, length or vtable) - we
/// only allow casts between fat pointers if their metadata have the same
/// kind.
#[derive(Copy, Clone, PartialEq, Eq)]
enum PointerKind<'tcx> {
    /// No metadata attached, ie pointer to sized type or foreign type
    Thin,
    /// A trait object
    Vtable(DefId),
    /// Slice
    Length,
    /// The unsize info of this projection
    OfProjection(&'tcx ty::ProjectionTy<'tcx>),
    /// The unsize info of this opaque ty
    OfOpaque(DefId, &'tcx Substs<'tcx>),
    /// The unsize info of this parameter
    OfParam(&'tcx ty::ParamTy),
}

impl<'a, 'gcx, 'tcx> FnCtxt<'a, 'gcx, 'tcx> {
    /// Returns the kind of unsize information of t, or None
    /// if t is unknown.
    fn pointer_kind(&self, t: Ty<'tcx>, span: Span) ->
        Result<Option<PointerKind<'tcx>>, ErrorReported>
    {
        debug!("pointer_kind({:?}, {:?})", t, span);

        let t = self.resolve_type_vars_if_possible(&t);

        if t.references_error() {
            return Err(ErrorReported);
        }

        if self.type_is_known_to_be_sized(t, span) {
            return Ok(Some(PointerKind::Thin));
        }

        Ok(match t.sty {
            ty::Slice(_) | ty::Str => Some(PointerKind::Length),
            ty::Dynamic(ref tty, ..) =>
                Some(PointerKind::Vtable(tty.principal().def_id())),
            ty::Adt(def, substs) if def.is_struct() => {
                match def.non_enum_variant().fields.last() {
                    None => Some(PointerKind::Thin),
                    Some(f) => {
                        let field_ty = self.field_ty(span, f, substs);
                        self.pointer_kind(field_ty, span)?
                    }
                }
            }
            ty::Tuple(fields) => match fields.last() {
                None => Some(PointerKind::Thin),
                Some(f) => self.pointer_kind(f, span)?
            },

            // Pointers to foreign types are thin, despite being unsized
            ty::Foreign(..) => Some(PointerKind::Thin),
            // We should really try to normalize here.
            ty::Projection(ref pi) => Some(PointerKind::OfProjection(pi)),
            ty::UnnormalizedProjection(..) => bug!("only used with chalk-engine"),
            ty::Opaque(def_id, substs) => Some(PointerKind::OfOpaque(def_id, substs)),
            ty::Param(ref p) => Some(PointerKind::OfParam(p)),
            // Insufficient type information.
            ty::Placeholder(..) | ty::Bound(..) | ty::Infer(_) => None,

            ty::Bool | ty::Char | ty::Int(..) | ty::Uint(..) |
            ty::Float(_) | ty::Array(..) | ty::GeneratorWitness(..) |
            ty::RawPtr(_) | ty::Ref(..) | ty::FnDef(..) |
            ty::FnPtr(..) | ty::Closure(..) | ty::Generator(..) |
            ty::Adt(..) | ty::Never | ty::Error => {
                self.tcx.sess.delay_span_bug(
                    span, &format!("`{:?}` should be sized but is not?", t));
                return Err(ErrorReported);
            }
        })
    }
}

#[derive(Copy, Clone)]
enum CastError {
    ErrorReported,

    CastToBool,
    CastToChar,
    DifferingKinds,
    /// Cast of thin to fat raw ptr (eg. `*const () as *const [u8]`)
    SizedUnsizedCast,
    IllegalCast,
    NeedDeref,
    NeedViaPtr,
    NeedViaThinPtr,
    NeedViaInt,
    NonScalar,
    UnknownExprPtrKind,
    UnknownCastPtrKind,
}

impl From<ErrorReported> for CastError {
    fn from(ErrorReported: ErrorReported) -> Self {
        CastError::ErrorReported
    }
}

fn make_invalid_casting_error<'a, 'gcx, 'tcx>(sess: &'a Session,
                                              span: Span,
                                              expr_ty: Ty<'tcx>,
                                              cast_ty: Ty<'tcx>,
                                              fcx: &FnCtxt<'a, 'gcx, 'tcx>)
                                              -> DiagnosticBuilder<'a> {
    type_error_struct!(sess, span, expr_ty, E0606,
                       "casting `{}` as `{}` is invalid",
                       fcx.ty_to_string(expr_ty),
                       fcx.ty_to_string(cast_ty))
}

impl<'a, 'gcx, 'tcx> CastCheck<'tcx> {
    pub fn new(fcx: &FnCtxt<'a, 'gcx, 'tcx>,
               expr: &'tcx hir::Expr,
               expr_ty: Ty<'tcx>,
               cast_ty: Ty<'tcx>,
               cast_span: Span,
               span: Span)
               -> Result<CastCheck<'tcx>, ErrorReported> {
        let check = CastCheck {
            expr,
            expr_ty,
            cast_ty,
            cast_span,
            span,
        };

        // For better error messages, check for some obviously unsized
        // cases now. We do a more thorough check at the end, once
        // inference is more completely known.
        match cast_ty.sty {
            ty::Dynamic(..) | ty::Slice(..) => {
                check.report_cast_to_unsized_type(fcx);
                Err(ErrorReported)
            }
            _ => Ok(check),
        }
    }

    fn report_cast_error(&self, fcx: &FnCtxt<'a, 'gcx, 'tcx>, e: CastError) {
        match e {
            CastError::ErrorReported => {
                // an error has already been reported
            }
            CastError::NeedDeref => {
                let error_span = self.span;
                let mut err = make_invalid_casting_error(fcx.tcx.sess, self.span, self.expr_ty,
                                                         self.cast_ty, fcx);
                let cast_ty = fcx.ty_to_string(self.cast_ty);
                err.span_label(error_span,
                               format!("cannot cast `{}` as `{}`",
                                       fcx.ty_to_string(self.expr_ty),
                                       cast_ty));
                if let Ok(snippet) = fcx.sess().source_map().span_to_snippet(self.expr.span) {
                    err.span_help(self.expr.span,
                        &format!("did you mean `*{}`?", snippet));
                }
                err.emit();
            }
            CastError::NeedViaThinPtr |
            CastError::NeedViaPtr => {
                let mut err = make_invalid_casting_error(fcx.tcx.sess, self.span, self.expr_ty,
                                                         self.cast_ty, fcx);
                if self.cast_ty.is_integral() {
                    err.help(&format!("cast through {} first",
                                      match e {
                                          CastError::NeedViaPtr => "a raw pointer",
                                          CastError::NeedViaThinPtr => "a thin pointer",
                                          _ => bug!(),
                                      }));
                }
                err.emit();
            }
            CastError::NeedViaInt => {
                make_invalid_casting_error(fcx.tcx.sess, self.span, self.expr_ty, self.cast_ty, fcx)
                   .help(&format!("cast through {} first",
                                  match e {
                                      CastError::NeedViaInt => "an integer",
                                      _ => bug!(),
                                  }))
                   .emit();
            }
            CastError::IllegalCast => {
                make_invalid_casting_error(fcx.tcx.sess, self.span, self.expr_ty, self.cast_ty, fcx)
                    .emit();
            }
            CastError::DifferingKinds => {
                make_invalid_casting_error(fcx.tcx.sess, self.span, self.expr_ty, self.cast_ty, fcx)
                    .note("vtable kinds may not match")
                    .emit();
            }
            CastError::CastToBool => {
                struct_span_err!(fcx.tcx.sess, self.span, E0054, "cannot cast as `bool`")
                    .span_label(self.span, "unsupported cast")
                    .help("compare with zero instead")
                    .emit();
            }
            CastError::CastToChar => {
                type_error_struct!(fcx.tcx.sess, self.span, self.expr_ty, E0604,
                    "only `u8` can be cast as `char`, not `{}`", self.expr_ty).emit();
            }
            CastError::NonScalar => {
                type_error_struct!(fcx.tcx.sess, self.span, self.expr_ty, E0605,
                                   "non-primitive cast: `{}` as `{}`",
                                   self.expr_ty,
                                   fcx.ty_to_string(self.cast_ty))
                                  .note("an `as` expression can only be used to convert between \
                                         primitive types. Consider using the `From` trait")
                                  .emit();
            }
            CastError::SizedUnsizedCast => {
                use structured_errors::{SizedUnsizedCastError, StructuredDiagnostic};
                SizedUnsizedCastError::new(&fcx.tcx.sess,
                                           self.span,
                                           self.expr_ty,
                                           fcx.ty_to_string(self.cast_ty))
                    .diagnostic().emit();
            }
            CastError::UnknownCastPtrKind |
            CastError::UnknownExprPtrKind => {
                let unknown_cast_to = match e {
                    CastError::UnknownCastPtrKind => true,
                    CastError::UnknownExprPtrKind => false,
                    _ => bug!(),
                };
                let mut err = struct_span_err!(fcx.tcx.sess, self.span, E0641,
                                               "cannot cast {} a pointer of an unknown kind",
                                               if unknown_cast_to { "to" } else { "from" });
                err.note("The type information given here is insufficient to check whether \
                          the pointer cast is valid");
                if unknown_cast_to {
                    err.span_suggestion_short_with_applicability(
                        self.cast_span,
                        "consider giving more type information",
                        String::new(),
                        Applicability::Unspecified,
                    );
                }
                err.emit();
            }
        }
    }

    fn report_cast_to_unsized_type(&self, fcx: &FnCtxt<'a, 'gcx, 'tcx>) {
        if self.cast_ty.references_error() || self.expr_ty.references_error() {
            return;
        }

        let tstr = fcx.ty_to_string(self.cast_ty);
        let mut err = type_error_struct!(fcx.tcx.sess, self.span, self.expr_ty, E0620,
                                         "cast to unsized type: `{}` as `{}`",
                                         fcx.resolve_type_vars_if_possible(&self.expr_ty),
                                         tstr);
        match self.expr_ty.sty {
            ty::Ref(_, _, mt) => {
                let mtstr = match mt {
                    hir::MutMutable => "mut ",
                    hir::MutImmutable => "",
                };
                if self.cast_ty.is_trait() {
                    match fcx.tcx.sess.source_map().span_to_snippet(self.cast_span) {
                        Ok(s) => {
                            err.span_suggestion_with_applicability(
                                self.cast_span,
                                "try casting to a reference instead",
                                format!("&{}{}", mtstr, s),
                                Applicability::MachineApplicable,
                            );
                        }
                        Err(_) => {
                            span_help!(err, self.cast_span, "did you mean `&{}{}`?", mtstr, tstr)
                        }
                    }
                } else {
                    span_help!(err,
                               self.span,
                               "consider using an implicit coercion to `&{}{}` instead",
                               mtstr,
                               tstr);
                }
            }
            ty::Adt(def, ..) if def.is_box() => {
                match fcx.tcx.sess.source_map().span_to_snippet(self.cast_span) {
                    Ok(s) => {
                        err.span_suggestion_with_applicability(
                            self.cast_span,
                            "try casting to a `Box` instead",
                            format!("Box<{}>", s),
                            Applicability::MachineApplicable,
                        );
                    }
                    Err(_) => span_help!(err, self.cast_span, "did you mean `Box<{}>`?", tstr),
                }
            }
            _ => {
                span_help!(err,
                           self.expr.span,
                           "consider using a box or reference as appropriate");
            }
        }
        err.emit();
    }

    fn trivial_cast_lint(&self, fcx: &FnCtxt<'a, 'gcx, 'tcx>) {
        let t_cast = self.cast_ty;
        let t_expr = self.expr_ty;
        let type_asc_or = if fcx.tcx.features().type_ascription {
            "type ascription or "
        } else {
            ""
        };
        let (adjective, lint) = if t_cast.is_numeric() && t_expr.is_numeric() {
            ("numeric ", lint::builtin::TRIVIAL_NUMERIC_CASTS)
        } else {
            ("", lint::builtin::TRIVIAL_CASTS)
        };
        let mut err = fcx.tcx.struct_span_lint_node(
            lint,
            self.expr.id,
            self.span,
            &format!("trivial {}cast: `{}` as `{}`",
                     adjective,
                     fcx.ty_to_string(t_expr),
                     fcx.ty_to_string(t_cast)));
        err.help(&format!("cast can be replaced by coercion; this might \
                           require {}a temporary variable", type_asc_or));
        err.emit();
    }

    pub fn check(mut self, fcx: &FnCtxt<'a, 'gcx, 'tcx>) {
        self.expr_ty = fcx.structurally_resolved_type(self.span, self.expr_ty);
        self.cast_ty = fcx.structurally_resolved_type(self.span, self.cast_ty);

        debug!("check_cast({}, {:?} as {:?})",
               self.expr.id,
               self.expr_ty,
               self.cast_ty);

        if !fcx.type_is_known_to_be_sized(self.cast_ty, self.span) {
            self.report_cast_to_unsized_type(fcx);
        } else if self.expr_ty.references_error() || self.cast_ty.references_error() {
            // No sense in giving duplicate error messages
        } else if self.try_coercion_cast(fcx) {
            self.trivial_cast_lint(fcx);
            debug!(" -> CoercionCast");
            fcx.tables.borrow_mut().cast_kinds_mut().insert(self.expr.hir_id,
                                                            CastKind::CoercionCast);
        } else {
            match self.do_check(fcx) {
                Ok(k) => {
                    debug!(" -> {:?}", k);
                    fcx.tables.borrow_mut().cast_kinds_mut().insert(self.expr.hir_id, k);
                }
                Err(e) => self.report_cast_error(fcx, e),
            };
        }
    }

    /// Check a cast, and report an error if one exists. In some cases, this
    /// can return Ok and create type errors in the fcx rather than returning
    /// directly. coercion-cast is handled in check instead of here.
    fn do_check(&self, fcx: &FnCtxt<'a, 'gcx, 'tcx>) -> Result<CastKind, CastError> {
        use rustc::ty::cast::IntTy::*;
        use rustc::ty::cast::CastTy::*;

        let (t_from, t_cast) = match (CastTy::from_ty(self.expr_ty),
                                      CastTy::from_ty(self.cast_ty)) {
            (Some(t_from), Some(t_cast)) => (t_from, t_cast),
            // Function item types may need to be reified before casts.
            (None, Some(t_cast)) => {
                if let ty::FnDef(..) = self.expr_ty.sty {
                    // Attempt a coercion to a fn pointer type.
                    let f = self.expr_ty.fn_sig(fcx.tcx);
                    let res = fcx.try_coerce(self.expr,
                                             self.expr_ty,
                                             fcx.tcx.mk_fn_ptr(f),
                                             AllowTwoPhase::No);
                    if res.is_err() {
                        return Err(CastError::NonScalar);
                    }
                    (FnPtr, t_cast)
                } else {
                    return Err(CastError::NonScalar);
                }
            }
            _ => return Err(CastError::NonScalar),
        };

        match (t_from, t_cast) {
            // These types have invariants! can't cast into them.
            (_, RPtr(_)) | (_, Int(CEnum)) | (_, FnPtr) => Err(CastError::NonScalar),

            // * -> Bool
            (_, Int(Bool)) => Err(CastError::CastToBool),

            // * -> Char
            (Int(U(ast::UintTy::U8)), Int(Char)) => Ok(CastKind::U8CharCast), // u8-char-cast
            (_, Int(Char)) => Err(CastError::CastToChar),

            // prim -> float,ptr
            (Int(Bool), Float) |
            (Int(CEnum), Float) |
            (Int(Char), Float) => Err(CastError::NeedViaInt),

            (Int(Bool), Ptr(_)) |
            (Int(CEnum), Ptr(_)) |
            (Int(Char), Ptr(_)) |
            (Ptr(_), Float) |
            (FnPtr, Float) |
            (Float, Ptr(_)) => Err(CastError::IllegalCast),

            // ptr -> *
            (Ptr(m_e), Ptr(m_c)) => self.check_ptr_ptr_cast(fcx, m_e, m_c), // ptr-ptr-cast
            (Ptr(m_expr), Int(_)) => self.check_ptr_addr_cast(fcx, m_expr), // ptr-addr-cast
            (FnPtr, Int(_)) => Ok(CastKind::FnPtrAddrCast),
            (RPtr(p), Int(_)) |
            (RPtr(p), Float) => {
                match p.ty.sty {
                    ty::Int(_) |
                    ty::Uint(_) |
                    ty::Float(_) => {
                        Err(CastError::NeedDeref)
                    }
                    ty::Infer(t) => {
                        match t {
                            ty::InferTy::IntVar(_) |
                            ty::InferTy::FloatVar(_) => Err(CastError::NeedDeref),
                            _ => Err(CastError::NeedViaPtr),
                        }
                    }
                    _ => Err(CastError::NeedViaPtr),
                }
            }
            // * -> ptr
            (Int(_), Ptr(mt)) => self.check_addr_ptr_cast(fcx, mt), // addr-ptr-cast
            (FnPtr, Ptr(mt)) => self.check_fptr_ptr_cast(fcx, mt),
            (RPtr(rmt), Ptr(mt)) => self.check_ref_cast(fcx, rmt, mt), // array-ptr-cast

            // prim -> prim
            (Int(CEnum), Int(_)) => Ok(CastKind::EnumCast),
            (Int(Char), Int(_)) |
            (Int(Bool), Int(_)) => Ok(CastKind::PrimIntCast),

            (Int(_), Int(_)) | (Int(_), Float) | (Float, Int(_)) | (Float, Float) => {
                Ok(CastKind::NumericCast)
            }
        }
    }

    fn check_ptr_ptr_cast(&self,
                          fcx: &FnCtxt<'a, 'gcx, 'tcx>,
                          m_expr: ty::TypeAndMut<'tcx>,
                          m_cast: ty::TypeAndMut<'tcx>)
                          -> Result<CastKind, CastError> {
        debug!("check_ptr_ptr_cast m_expr={:?} m_cast={:?}", m_expr, m_cast);
        // ptr-ptr cast. vtables must match.

        let expr_kind = fcx.pointer_kind(m_expr.ty, self.span)?;
        let cast_kind = fcx.pointer_kind(m_cast.ty, self.span)?;

        let cast_kind = match cast_kind {
            // We can't cast if target pointer kind is unknown
            None => return Err(CastError::UnknownCastPtrKind),
            Some(cast_kind) => cast_kind,
        };

        // Cast to thin pointer is OK
        if cast_kind == PointerKind::Thin {
            return Ok(CastKind::PtrPtrCast);
        }

        let expr_kind = match expr_kind {
            // We can't cast to fat pointer if source pointer kind is unknown
            None => return Err(CastError::UnknownExprPtrKind),
            Some(expr_kind) => expr_kind,
        };

        // thin -> fat? report invalid cast (don't complain about vtable kinds)
        if expr_kind == PointerKind::Thin {
            return Err(CastError::SizedUnsizedCast);
        }

        // vtable kinds must match
        if cast_kind == expr_kind {
            Ok(CastKind::PtrPtrCast)
        } else {
            Err(CastError::DifferingKinds)
        }
    }

    fn check_fptr_ptr_cast(&self,
                           fcx: &FnCtxt<'a, 'gcx, 'tcx>,
                           m_cast: ty::TypeAndMut<'tcx>)
                           -> Result<CastKind, CastError> {
        // fptr-ptr cast. must be to thin ptr

        match fcx.pointer_kind(m_cast.ty, self.span)? {
            None => Err(CastError::UnknownCastPtrKind),
            Some(PointerKind::Thin) => Ok(CastKind::FnPtrPtrCast),
            _ => Err(CastError::IllegalCast),
        }
    }

    fn check_ptr_addr_cast(&self,
                           fcx: &FnCtxt<'a, 'gcx, 'tcx>,
                           m_expr: ty::TypeAndMut<'tcx>)
                           -> Result<CastKind, CastError> {
        // ptr-addr cast. must be from thin ptr

        match fcx.pointer_kind(m_expr.ty, self.span)? {
            None => Err(CastError::UnknownExprPtrKind),
            Some(PointerKind::Thin) => Ok(CastKind::PtrAddrCast),
            _ => Err(CastError::NeedViaThinPtr),
        }
    }

    fn check_ref_cast(&self,
                      fcx: &FnCtxt<'a, 'gcx, 'tcx>,
                      m_expr: ty::TypeAndMut<'tcx>,
                      m_cast: ty::TypeAndMut<'tcx>)
                      -> Result<CastKind, CastError> {
        // array-ptr-cast.

        if m_expr.mutbl == hir::MutImmutable && m_cast.mutbl == hir::MutImmutable {
            if let ty::Array(ety, _) = m_expr.ty.sty {
                // Due to the limitations of LLVM global constants,
                // region pointers end up pointing at copies of
                // vector elements instead of the original values.
                // To allow raw pointers to work correctly, we
                // need to special-case obtaining a raw pointer
                // from a region pointer to a vector.

                // this will report a type mismatch if needed
                fcx.demand_eqtype(self.span, ety, m_cast.ty);
                return Ok(CastKind::ArrayPtrCast);
            }
        }

        Err(CastError::IllegalCast)
    }

    fn check_addr_ptr_cast(&self,
                           fcx: &FnCtxt<'a, 'gcx, 'tcx>,
                           m_cast: TypeAndMut<'tcx>)
                           -> Result<CastKind, CastError> {
        // ptr-addr cast. pointer must be thin.
        match fcx.pointer_kind(m_cast.ty, self.span)? {
            None => Err(CastError::UnknownCastPtrKind),
            Some(PointerKind::Thin) => Ok(CastKind::AddrPtrCast),
            _ => Err(CastError::IllegalCast),
        }
    }

    fn try_coercion_cast(&self, fcx: &FnCtxt<'a, 'gcx, 'tcx>) -> bool {
        fcx.try_coerce(self.expr, self.expr_ty, self.cast_ty, AllowTwoPhase::No).is_ok()
    }
}

impl<'a, 'gcx, 'tcx> FnCtxt<'a, 'gcx, 'tcx> {
    fn type_is_known_to_be_sized(&self, ty: Ty<'tcx>, span: Span) -> bool {
        let lang_item = self.tcx.require_lang_item(lang_items::SizedTraitLangItem);
        traits::type_known_to_meet_bound(self, self.param_env, ty, lang_item, span)
    }
}
