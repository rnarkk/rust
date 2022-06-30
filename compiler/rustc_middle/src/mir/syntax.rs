//! This defines the syntax of MIR, i.e., the set of available MIR operations, and other definitions
//! closely related to MIR semantics.
//! This is in a dedicated file so that changes to this file can be reviewed more carefully.
//! The intention is that this file only contains datatype declarations, no code.

use super::{BasicBlock, Constant, Field, Local, SwitchTargets, UserTypeProjection};

use crate::mir::coverage::{CodeRegion, CoverageKind};
use crate::ty::adjustment::PointerCast;
use crate::ty::subst::SubstsRef;
use crate::ty::{self, List, Ty};
use crate::ty::{Region, UserTypeAnnotationIndex};

use rustc_ast::{InlineAsmOptions, InlineAsmTemplatePiece};
use rustc_hir::def_id::DefId;
use rustc_hir::{self as hir};
use rustc_hir::{self, GeneratorKind};
use rustc_target::abi::VariantIdx;

use rustc_ast::Mutability;
use rustc_span::symbol::Symbol;
use rustc_span::Span;
use rustc_target::asm::InlineAsmRegOrRegClass;

/// The various "big phases" that MIR goes through.
///
/// These phases all describe dialects of MIR. Since all MIR uses the same datastructures, the
/// dialects forbid certain variants or values in certain phases. The sections below summarize the
/// changes, but do not document them thoroughly. The full documentation is found in the appropriate
/// documentation for the thing the change is affecting.
///
/// Warning: ordering of variants is significant.
#[derive(Copy, Clone, TyEncodable, TyDecodable, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[derive(HashStable)]
pub enum MirPhase {
    /// The dialect of MIR used during all phases before `DropsLowered` is the same. This is also
    /// the MIR that analysis such as borrowck uses.
    ///
    /// One important thing to remember about the behavior of this section of MIR is that drop terminators
    /// (including drop and replace) are *conditional*. The elaborate drops pass will then replace each
    /// instance of a drop terminator with a nop, an unconditional drop, or a drop conditioned on a drop
    /// flag. Of course, this means that it is important that the drop elaboration can accurately recognize
    /// when things are initialized and when things are de-initialized. That means any code running on this
    /// version of MIR must be sure to produce output that drop elaboration can reason about. See the
    /// section on the drop terminatorss for more details.
    Built = 0,
    // FIXME(oli-obk): it's unclear whether we still need this phase (and its corresponding query).
    // We used to have this for pre-miri MIR based const eval.
    Const = 1,
    /// This phase checks the MIR for promotable elements and takes them out of the main MIR body
    /// by creating a new MIR body per promoted element. After this phase (and thus the termination
    /// of the `mir_promoted` query), these promoted elements are available in the `promoted_mir`
    /// query.
    ConstsPromoted = 2,
    /// Beginning with this phase, the following variants are disallowed:
    /// * [`TerminatorKind::DropAndReplace`]
    /// * [`TerminatorKind::FalseUnwind`]
    /// * [`TerminatorKind::FalseEdge`]
    /// * [`StatementKind::FakeRead`]
    /// * [`StatementKind::AscribeUserType`]
    /// * [`Rvalue::Ref`] with `BorrowKind::Shallow`
    ///
    /// And the following variant is allowed:
    /// * [`StatementKind::Retag`]
    ///
    /// Furthermore, `Drop` now uses explicit drop flags visible in the MIR and reaching a `Drop`
    /// terminator means that the auto-generated drop glue will be invoked. Also, `Copy` operands
    /// are allowed for non-`Copy` types.
    DropsLowered = 3,
    /// After this projections may only contain deref projections as the first element.
    Derefered = 4,
    /// Beginning with this phase, the following variant is disallowed:
    /// * [`Rvalue::Aggregate`] for any `AggregateKind` except `Array`
    ///
    /// And the following variant is allowed:
    /// * [`StatementKind::SetDiscriminant`]
    Deaggregated = 5,
    /// Before this phase, generators are in the "source code" form, featuring `yield` statements
    /// and such. With this phase change, they are transformed into a proper state machine. Running
    /// optimizations before this change can be potentially dangerous because the source code is to
    /// some extent a "lie." In particular, `yield` terminators effectively make the value of all
    /// locals visible to the caller. This means that dead store elimination before them, or code
    /// motion across them, is not correct in general. This is also exasperated by type checking
    /// having pre-computed a list of the types that it thinks are ok to be live across a yield
    /// point - this is necessary to decide eg whether autotraits are implemented. Introducing new
    /// types across a yield point will lead to ICEs becaues of this.
    ///
    /// Beginning with this phase, the following variants are disallowed:
    /// * [`TerminatorKind::Yield`]
    /// * [`TerminatorKind::GeneratorDrop`]
    /// * [`ProjectionElem::Deref`] of `Box`
    GeneratorsLowered = 6,
    Optimized = 7,
}

///////////////////////////////////////////////////////////////////////////
// Borrow kinds

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, TyEncodable, TyDecodable)]
#[derive(Hash, HashStable)]
pub enum BorrowKind {
    /// Data must be immutable and is aliasable.
    Shared,

    /// The immediately borrowed place must be immutable, but projections from
    /// it don't need to be. For example, a shallow borrow of `a.b` doesn't
    /// conflict with a mutable borrow of `a.b.c`.
    ///
    /// This is used when lowering matches: when matching on a place we want to
    /// ensure that place have the same value from the start of the match until
    /// an arm is selected. This prevents this code from compiling:
    /// ```compile_fail,E0510
    /// let mut x = &Some(0);
    /// match *x {
    ///     None => (),
    ///     Some(_) if { x = &None; false } => (),
    ///     Some(_) => (),
    /// }
    /// ```
    /// This can't be a shared borrow because mutably borrowing (*x as Some).0
    /// should not prevent `if let None = x { ... }`, for example, because the
    /// mutating `(*x as Some).0` can't affect the discriminant of `x`.
    /// We can also report errors with this kind of borrow differently.
    Shallow,

    /// Data must be immutable but not aliasable. This kind of borrow
    /// cannot currently be expressed by the user and is used only in
    /// implicit closure bindings. It is needed when the closure is
    /// borrowing or mutating a mutable referent, e.g.:
    /// ```
    /// let mut z = 3;
    /// let x: &mut isize = &mut z;
    /// let y = || *x += 5;
    /// ```
    /// If we were to try to translate this closure into a more explicit
    /// form, we'd encounter an error with the code as written:
    /// ```compile_fail,E0594
    /// struct Env<'a> { x: &'a &'a mut isize }
    /// let mut z = 3;
    /// let x: &mut isize = &mut z;
    /// let y = (&mut Env { x: &x }, fn_ptr);  // Closure is pair of env and fn
    /// fn fn_ptr(env: &mut Env) { **env.x += 5; }
    /// ```
    /// This is then illegal because you cannot mutate an `&mut` found
    /// in an aliasable location. To solve, you'd have to translate with
    /// an `&mut` borrow:
    /// ```compile_fail,E0596
    /// struct Env<'a> { x: &'a mut &'a mut isize }
    /// let mut z = 3;
    /// let x: &mut isize = &mut z;
    /// let y = (&mut Env { x: &mut x }, fn_ptr); // changed from &x to &mut x
    /// fn fn_ptr(env: &mut Env) { **env.x += 5; }
    /// ```
    /// Now the assignment to `**env.x` is legal, but creating a
    /// mutable pointer to `x` is not because `x` is not mutable. We
    /// could fix this by declaring `x` as `let mut x`. This is ok in
    /// user code, if awkward, but extra weird for closures, since the
    /// borrow is hidden.
    ///
    /// So we introduce a "unique imm" borrow -- the referent is
    /// immutable, but not aliasable. This solves the problem. For
    /// simplicity, we don't give users the way to express this
    /// borrow, it's just used when translating closures.
    Unique,

    /// Data is mutable and not aliasable.
    Mut {
        /// `true` if this borrow arose from method-call auto-ref
        /// (i.e., `adjustment::Adjust::Borrow`).
        allow_two_phase_borrow: bool,
    },
}

///////////////////////////////////////////////////////////////////////////
// Statements

/// The various kinds of statements that can appear in MIR.
///
/// Not all of these are allowed at every [`MirPhase`]. Check the documentation there to see which
/// ones you do not have to worry about. The MIR validator will generally enforce such restrictions,
/// causing an ICE if they are violated.
#[derive(Clone, Debug, PartialEq, TyEncodable, TyDecodable, Hash, HashStable, TypeFoldable)]
pub enum StatementKind<'tcx> {
    /// Assign statements roughly correspond to an assignment in Rust proper (`x = ...`) except
    /// without the possibility of dropping the previous value (that must be done separately, if at
    /// all). The *exact* way this works is undecided. It probably does something like evaluating
    /// the LHS to a place and the RHS to a value, and then storing the value to the place. Various
    /// parts of this may do type specific things that are more complicated than simply copying
    /// bytes.
    ///
    /// **Needs clarification**: The implication of the above idea would be that assignment implies
    /// that the resulting value is initialized. I believe we could commit to this separately from
    /// committing to whatever part of the memory model we would need to decide on to make the above
    /// paragragh precise. Do we want to?
    ///
    /// Assignments in which the types of the place and rvalue differ are not well-formed.
    ///
    /// **Needs clarification**: Do we ever want to worry about non-free (in the body) lifetimes for
    /// the typing requirement in post drop-elaboration MIR? I think probably not - I'm not sure we
    /// could meaningfully require this anyway. How about free lifetimes? Is ignoring this
    /// interesting for optimizations? Do we want to allow such optimizations?
    ///
    /// **Needs clarification**: We currently require that the LHS place not overlap with any place
    /// read as part of computation of the RHS for some rvalues (generally those not producing
    /// primitives). This requirement is under discussion in [#68364]. As a part of this discussion,
    /// it is also unclear in what order the components are evaluated.
    ///
    /// [#68364]: https://github.com/rust-lang/rust/issues/68364
    ///
    /// See [`Rvalue`] documentation for details on each of those.
    Assign(Box<(Place<'tcx>, Rvalue<'tcx>)>),

    /// This represents all the reading that a pattern match may do (e.g., inspecting constants and
    /// discriminant values), and the kind of pattern it comes from. This is in order to adapt
    /// potential error messages to these specific patterns.
    ///
    /// Note that this also is emitted for regular `let` bindings to ensure that locals that are
    /// never accessed still get some sanity checks for, e.g., `let x: ! = ..;`
    ///
    /// When executed at runtime this is a nop.
    ///
    /// Disallowed after drop elaboration.
    FakeRead(Box<(FakeReadCause, Place<'tcx>)>),

    /// Write the discriminant for a variant to the enum Place.
    ///
    /// This is permitted for both generators and ADTs. This does not necessarily write to the
    /// entire place; instead, it writes to the minimum set of bytes as required by the layout for
    /// the type.
    SetDiscriminant { place: Box<Place<'tcx>>, variant_index: VariantIdx },

    /// Deinitializes the place.
    ///
    /// This writes `uninit` bytes to the entire place.
    Deinit(Box<Place<'tcx>>),

    /// `StorageLive` and `StorageDead` statements mark the live range of a local.
    ///
    /// Using a local before a `StorageLive` or after a `StorageDead` is not well-formed. These
    /// statements are not required. If the entire MIR body contains no `StorageLive`/`StorageDead`
    /// statements for a particular local, the local is always considered live.
    ///
    /// More precisely, the MIR validator currently does a `MaybeStorageLiveLocals` analysis to
    /// check validity of each use of a local. I believe this is equivalent to requiring for every
    /// use of a local, there exist at least one path from the root to that use that contains a
    /// `StorageLive` more recently than a `StorageDead`.
    ///
    /// **Needs clarification**: Is it permitted to have two `StorageLive`s without an intervening
    /// `StorageDead`? Two `StorageDead`s without an intervening `StorageLive`? LLVM says poison,
    /// yes. If the answer to any of these is "no," is breaking that rule UB or is it an error to
    /// have a path in the CFG that might do this?
    StorageLive(Local),

    /// See `StorageLive` above.
    StorageDead(Local),

    /// Retag references in the given place, ensuring they got fresh tags.
    ///
    /// This is part of the Stacked Borrows model. These statements are currently only interpreted
    /// by miri and only generated when `-Z mir-emit-retag` is passed. See
    /// <https://internals.rust-lang.org/t/stacked-borrows-an-aliasing-model-for-rust/8153/> for
    /// more details.
    ///
    /// For code that is not specific to stacked borrows, you should consider retags to read
    /// and modify the place in an opaque way.
    Retag(RetagKind, Box<Place<'tcx>>),

    /// Encodes a user's type ascription. These need to be preserved
    /// intact so that NLL can respect them. For example:
    /// ```ignore (illustrative)
    /// let a: T = y;
    /// ```
    /// The effect of this annotation is to relate the type `T_y` of the place `y`
    /// to the user-given type `T`. The effect depends on the specified variance:
    ///
    /// - `Covariant` -- requires that `T_y <: T`
    /// - `Contravariant` -- requires that `T_y :> T`
    /// - `Invariant` -- requires that `T_y == T`
    /// - `Bivariant` -- no effect
    ///
    /// When executed at runtime this is a nop.
    ///
    /// Disallowed after drop elaboration.
    AscribeUserType(Box<(Place<'tcx>, UserTypeProjection)>, ty::Variance),

    /// Marks the start of a "coverage region", injected with '-Cinstrument-coverage'. A
    /// `Coverage` statement carries metadata about the coverage region, used to inject a coverage
    /// map into the binary. If `Coverage::kind` is a `Counter`, the statement also generates
    /// executable code, to increment a counter variable at runtime, each time the code region is
    /// executed.
    Coverage(Box<Coverage>),

    /// Denotes a call to the intrinsic function `copy_nonoverlapping`.
    ///
    /// First, all three operands are evaluated. `src` and `dest` must each be a reference, pointer,
    /// or `Box` pointing to the same type `T`. `count` must evaluate to a `usize`. Then, `src` and
    /// `dest` are dereferenced, and `count * size_of::<T>()` bytes beginning with the first byte of
    /// the `src` place are copied to the continguous range of bytes beginning with the first byte
    /// of `dest`.
    ///
    /// **Needs clarification**: In what order are operands computed and dereferenced? It should
    /// probably match the order for assignment, but that is also undecided.
    ///
    /// **Needs clarification**: Is this typed or not, ie is there a typed load and store involved?
    /// I vaguely remember Ralf saying somewhere that he thought it should not be.
    CopyNonOverlapping(Box<CopyNonOverlapping<'tcx>>),

    /// No-op. Useful for deleting instructions without affecting statement indices.
    Nop,
}

/// Describes what kind of retag is to be performed.
#[derive(Copy, Clone, TyEncodable, TyDecodable, Debug, PartialEq, Eq, Hash, HashStable)]
pub enum RetagKind {
    /// The initial retag when entering a function.
    FnEntry,
    /// Retag preparing for a two-phase borrow.
    TwoPhase,
    /// Retagging raw pointers.
    Raw,
    /// A "normal" retag.
    Default,
}

/// The `FakeReadCause` describes the type of pattern why a FakeRead statement exists.
#[derive(Copy, Clone, TyEncodable, TyDecodable, Debug, Hash, HashStable, PartialEq)]
pub enum FakeReadCause {
    /// Inject a fake read of the borrowed input at the end of each guards
    /// code.
    ///
    /// This should ensure that you cannot change the variant for an enum while
    /// you are in the midst of matching on it.
    ForMatchGuard,

    /// `let x: !; match x {}` doesn't generate any read of x so we need to
    /// generate a read of x to check that it is initialized and safe.
    ///
    /// If a closure pattern matches a Place starting with an Upvar, then we introduce a
    /// FakeRead for that Place outside the closure, in such a case this option would be
    /// Some(closure_def_id).
    /// Otherwise, the value of the optional DefId will be None.
    ForMatchedPlace(Option<DefId>),

    /// A fake read of the RefWithinGuard version of a bind-by-value variable
    /// in a match guard to ensure that its value hasn't change by the time
    /// we create the OutsideGuard version.
    ForGuardBinding,

    /// Officially, the semantics of
    ///
    /// `let pattern = <expr>;`
    ///
    /// is that `<expr>` is evaluated into a temporary and then this temporary is
    /// into the pattern.
    ///
    /// However, if we see the simple pattern `let var = <expr>`, we optimize this to
    /// evaluate `<expr>` directly into the variable `var`. This is mostly unobservable,
    /// but in some cases it can affect the borrow checker, as in #53695.
    /// Therefore, we insert a "fake read" here to ensure that we get
    /// appropriate errors.
    ///
    /// If a closure pattern matches a Place starting with an Upvar, then we introduce a
    /// FakeRead for that Place outside the closure, in such a case this option would be
    /// Some(closure_def_id).
    /// Otherwise, the value of the optional DefId will be None.
    ForLet(Option<DefId>),

    /// If we have an index expression like
    ///
    /// (*x)[1][{ x = y; 4}]
    ///
    /// then the first bounds check is invalidated when we evaluate the second
    /// index expression. Thus we create a fake borrow of `x` across the second
    /// indexer, which will cause a borrow check error.
    ForIndex,
}

#[derive(Clone, Debug, PartialEq, TyEncodable, TyDecodable, Hash, HashStable, TypeFoldable)]
pub struct Coverage {
    pub kind: CoverageKind,
    pub code_region: Option<CodeRegion>,
}

#[derive(Clone, Debug, PartialEq, TyEncodable, TyDecodable, Hash, HashStable, TypeFoldable)]
pub struct CopyNonOverlapping<'tcx> {
    pub src: Operand<'tcx>,
    pub dst: Operand<'tcx>,
    /// Number of elements to copy from src to dest, not bytes.
    pub count: Operand<'tcx>,
}

///////////////////////////////////////////////////////////////////////////
// Terminators

/// A note on unwinding: Panics may occur during the execution of some terminators. Depending on the
/// `-C panic` flag, this may either cause the program to abort or the call stack to unwind. Such
/// terminators have a `cleanup: Option<BasicBlock>` field on them. If stack unwinding occurs, then
/// once the current function is reached, execution continues at the given basic block, if any. If
/// `cleanup` is `None` then no cleanup is performed, and the stack continues unwinding. This is
/// equivalent to the execution of a `Resume` terminator.
///
/// The basic block pointed to by a `cleanup` field must have its `cleanup` flag set. `cleanup`
/// basic blocks have a couple restrictions:
///  1. All `cleanup` fields in them must be `None`.
///  2. `Return` terminators are not allowed in them. `Abort` and `Unwind` terminators are.
///  3. All other basic blocks (in the current body) that are reachable from `cleanup` basic blocks
///     must also be `cleanup`. This is a part of the type system and checked statically, so it is
///     still an error to have such an edge in the CFG even if it's known that it won't be taken at
///     runtime.
#[derive(Clone, TyEncodable, TyDecodable, Hash, HashStable, PartialEq)]
pub enum TerminatorKind<'tcx> {
    /// Block has one successor; we continue execution there.
    Goto { target: BasicBlock },

    /// Switches based on the computed value.
    ///
    /// First, evaluates the `discr` operand. The type of the operand must be a signed or unsigned
    /// integer, char, or bool, and must match the given type. Then, if the list of switch targets
    /// contains the computed value, continues execution at the associated basic block. Otherwise,
    /// continues execution at the "otherwise" basic block.
    ///
    /// Target values may not appear more than once.
    SwitchInt {
        /// The discriminant value being tested.
        discr: Operand<'tcx>,

        /// The type of value being tested.
        /// This is always the same as the type of `discr`.
        /// FIXME: remove this redundant information. Currently, it is relied on by pretty-printing.
        switch_ty: Ty<'tcx>,

        targets: SwitchTargets,
    },

    /// Indicates that the landing pad is finished and that the process should continue unwinding.
    ///
    /// Like a return, this marks the end of this invocation of the function.
    ///
    /// Only permitted in cleanup blocks. `Resume` is not permitted with `-C unwind=abort` after
    /// deaggregation runs.
    Resume,

    /// Indicates that the landing pad is finished and that the process should abort.
    ///
    /// Used to prevent unwinding for foreign items or with `-C unwind=abort`. Only permitted in
    /// cleanup blocks.
    Abort,

    /// Returns from the function.
    ///
    /// Like function calls, the exact semantics of returns in Rust are unclear. Returning very
    /// likely at least assigns the value currently in the return place (`_0`) to the place
    /// specified in the associated `Call` terminator in the calling function, as if assigned via
    /// `dest = move _0`. It might additionally do other things, like have side-effects in the
    /// aliasing model.
    ///
    /// If the body is a generator body, this has slightly different semantics; it instead causes a
    /// `GeneratorState::Returned(_0)` to be created (as if by an `Aggregate` rvalue) and assigned
    /// to the return place.
    Return,

    /// Indicates a terminator that can never be reached.
    ///
    /// Executing this terminator is UB.
    Unreachable,

    /// The behavior of this statement differs significantly before and after drop elaboration.
    /// After drop elaboration, `Drop` executes the drop glue for the specified place, after which
    /// it continues execution/unwinds at the given basic blocks. It is possible that executing drop
    /// glue is special - this would be part of Rust's memory model. (**FIXME**: due we have an
    /// issue tracking if drop glue has any interesting semantics in addition to those of a function
    /// call?)
    ///
    /// `Drop` before drop elaboration is a *conditional* execution of the drop glue. Specifically, the
    /// `Drop` will be executed if...
    ///
    /// **Needs clarification**: End of that sentence. This in effect should document the exact
    /// behavior of drop elaboration. The following sounds vaguely right, but I'm not quite sure:
    ///
    /// > The drop glue is executed if, among all statements executed within this `Body`, an assignment to
    /// > the place or one of its "parents" occurred more recently than a move out of it. This does not
    /// > consider indirect assignments.
    Drop { place: Place<'tcx>, target: BasicBlock, unwind: Option<BasicBlock> },

    /// Drops the place and assigns a new value to it.
    ///
    /// This first performs the exact same operation as the pre drop-elaboration `Drop` terminator;
    /// it then additionally assigns the `value` to the `place` as if by an assignment statement.
    /// This assignment occurs both in the unwind and the regular code paths. The semantics are best
    /// explained by the elaboration:
    ///
    /// ```ignore (MIR)
    /// BB0 {
    ///   DropAndReplace(P <- V, goto BB1, unwind BB2)
    /// }
    /// ```
    ///
    /// becomes
    ///
    /// ```ignore (MIR)
    /// BB0 {
    ///   Drop(P, goto BB1, unwind BB2)
    /// }
    /// BB1 {
    ///   // P is now uninitialized
    ///   P <- V
    /// }
    /// BB2 {
    ///   // P is now uninitialized -- its dtor panicked
    ///   P <- V
    /// }
    /// ```
    ///
    /// Disallowed after drop elaboration.
    DropAndReplace {
        place: Place<'tcx>,
        value: Operand<'tcx>,
        target: BasicBlock,
        unwind: Option<BasicBlock>,
    },

    /// Roughly speaking, evaluates the `func` operand and the arguments, and starts execution of
    /// the referred to function. The operand types must match the argument types of the function.
    /// The return place type must match the return type. The type of the `func` operand must be
    /// callable, meaning either a function pointer, a function type, or a closure type.
    ///
    /// **Needs clarification**: The exact semantics of this. Current backends rely on `move`
    /// operands not aliasing the return place. It is unclear how this is justified in MIR, see
    /// [#71117].
    ///
    /// [#71117]: https://github.com/rust-lang/rust/issues/71117
    Call {
        /// The function that’s being called.
        func: Operand<'tcx>,
        /// Arguments the function is called with.
        /// These are owned by the callee, which is free to modify them.
        /// This allows the memory occupied by "by-value" arguments to be
        /// reused across function calls without duplicating the contents.
        args: Vec<Operand<'tcx>>,
        /// Where the returned value will be written
        destination: Place<'tcx>,
        /// Where to go after this call returns. If none, the call necessarily diverges.
        target: Option<BasicBlock>,
        /// Cleanups to be done if the call unwinds.
        cleanup: Option<BasicBlock>,
        /// `true` if this is from a call in HIR rather than from an overloaded
        /// operator. True for overloaded function call.
        from_hir_call: bool,
        /// This `Span` is the span of the function, without the dot and receiver
        /// (e.g. `foo(a, b)` in `x.foo(a, b)`
        fn_span: Span,
    },

    /// Evaluates the operand, which must have type `bool`. If it is not equal to `expected`,
    /// initiates a panic. Initiating a panic corresponds to a `Call` terminator with some
    /// unspecified constant as the function to call, all the operands stored in the `AssertMessage`
    /// as parameters, and `None` for the destination. Keep in mind that the `cleanup` path is not
    /// necessarily executed even in the case of a panic, for example in `-C panic=abort`. If the
    /// assertion does not fail, execution continues at the specified basic block.
    Assert {
        cond: Operand<'tcx>,
        expected: bool,
        msg: AssertMessage<'tcx>,
        target: BasicBlock,
        cleanup: Option<BasicBlock>,
    },

    /// Marks a suspend point.
    ///
    /// Like `Return` terminators in generator bodies, this computes `value` and then a
    /// `GeneratorState::Yielded(value)` as if by `Aggregate` rvalue. That value is then assigned to
    /// the return place of the function calling this one, and execution continues in the calling
    /// function. When next invoked with the same first argument, execution of this function
    /// continues at the `resume` basic block, with the second argument written to the `resume_arg`
    /// place. If the generator is dropped before then, the `drop` basic block is invoked.
    ///
    /// Not permitted in bodies that are not generator bodies, or after generator lowering.
    ///
    /// **Needs clarification**: What about the evaluation order of the `resume_arg` and `value`?
    Yield {
        /// The value to return.
        value: Operand<'tcx>,
        /// Where to resume to.
        resume: BasicBlock,
        /// The place to store the resume argument in.
        resume_arg: Place<'tcx>,
        /// Cleanup to be done if the generator is dropped at this suspend point.
        drop: Option<BasicBlock>,
    },

    /// Indicates the end of dropping a generator.
    ///
    /// Semantically just a `return` (from the generators drop glue). Only permitted in the same situations
    /// as `yield`.
    ///
    /// **Needs clarification**: Is that even correct? The generator drop code is always confusing
    /// to me, because it's not even really in the current body.
    ///
    /// **Needs clarification**: Are there type system constraints on these terminators? Should
    /// there be a "block type" like `cleanup` blocks for them?
    GeneratorDrop,

    /// A block where control flow only ever takes one real path, but borrowck needs to be more
    /// conservative.
    ///
    /// At runtime this is semantically just a goto.
    ///
    /// Disallowed after drop elaboration.
    FalseEdge {
        /// The target normal control flow will take.
        real_target: BasicBlock,
        /// A block control flow could conceptually jump to, but won't in
        /// practice.
        imaginary_target: BasicBlock,
    },

    /// A terminator for blocks that only take one path in reality, but where we reserve the right
    /// to unwind in borrowck, even if it won't happen in practice. This can arise in infinite loops
    /// with no function calls for example.
    ///
    /// At runtime this is semantically just a goto.
    ///
    /// Disallowed after drop elaboration.
    FalseUnwind {
        /// The target normal control flow will take.
        real_target: BasicBlock,
        /// The imaginary cleanup block link. This particular path will never be taken
        /// in practice, but in order to avoid fragility we want to always
        /// consider it in borrowck. We don't want to accept programs which
        /// pass borrowck only when `panic=abort` or some assertions are disabled
        /// due to release vs. debug mode builds. This needs to be an `Option` because
        /// of the `remove_noop_landing_pads` and `abort_unwinding_calls` passes.
        unwind: Option<BasicBlock>,
    },

    /// Block ends with an inline assembly block. This is a terminator since
    /// inline assembly is allowed to diverge.
    InlineAsm {
        /// The template for the inline assembly, with placeholders.
        template: &'tcx [InlineAsmTemplatePiece],

        /// The operands for the inline assembly, as `Operand`s or `Place`s.
        operands: Vec<InlineAsmOperand<'tcx>>,

        /// Miscellaneous options for the inline assembly.
        options: InlineAsmOptions,

        /// Source spans for each line of the inline assembly code. These are
        /// used to map assembler errors back to the line in the source code.
        line_spans: &'tcx [Span],

        /// Destination block after the inline assembly returns, unless it is
        /// diverging (InlineAsmOptions::NORETURN).
        destination: Option<BasicBlock>,

        /// Cleanup to be done if the inline assembly unwinds. This is present
        /// if and only if InlineAsmOptions::MAY_UNWIND is set.
        cleanup: Option<BasicBlock>,
    },
}

/// Information about an assertion failure.
#[derive(Clone, TyEncodable, TyDecodable, Hash, HashStable, PartialEq, PartialOrd)]
pub enum AssertKind<O> {
    BoundsCheck { len: O, index: O },
    Overflow(BinOp, O, O),
    OverflowNeg(O),
    DivisionByZero(O),
    RemainderByZero(O),
    ResumedAfterReturn(GeneratorKind),
    ResumedAfterPanic(GeneratorKind),
}

#[derive(Clone, Debug, PartialEq, TyEncodable, TyDecodable, Hash, HashStable, TypeFoldable)]
pub enum InlineAsmOperand<'tcx> {
    In {
        reg: InlineAsmRegOrRegClass,
        value: Operand<'tcx>,
    },
    Out {
        reg: InlineAsmRegOrRegClass,
        late: bool,
        place: Option<Place<'tcx>>,
    },
    InOut {
        reg: InlineAsmRegOrRegClass,
        late: bool,
        in_value: Operand<'tcx>,
        out_place: Option<Place<'tcx>>,
    },
    Const {
        value: Box<Constant<'tcx>>,
    },
    SymFn {
        value: Box<Constant<'tcx>>,
    },
    SymStatic {
        def_id: DefId,
    },
}

/// Type for MIR `Assert` terminator error messages.
pub type AssertMessage<'tcx> = AssertKind<Operand<'tcx>>;

///////////////////////////////////////////////////////////////////////////
// Places

/// Places roughly correspond to a "location in memory." Places in MIR are the same mathematical
/// object as places in Rust. This of course means that what exactly they are is undecided and part
/// of the Rust memory model. However, they will likely contain at least the following pieces of
/// information in some form:
///
///  1. The address in memory that the place refers to.
///  2. The provenance with which the place is being accessed.
///  3. The type of the place and an optional variant index. See [`PlaceTy`][super::tcx::PlaceTy].
///  4. Optionally, some metadata. This exists if and only if the type of the place is not `Sized`.
///
/// We'll give a description below of how all pieces of the place except for the provenance are
/// calculated. We cannot give a description of the provenance, because that is part of the
/// undecided aliasing model - we only include it here at all to acknowledge its existence.
///
/// Each local naturally corresponds to the place `Place { local, projection: [] }`. This place has
/// the address of the local's allocation and the type of the local.
///
/// **Needs clarification:** Unsized locals seem to present a bit of an issue. Their allocation
/// can't actually be created on `StorageLive`, because it's unclear how big to make the allocation.
/// Furthermore, MIR produces assignments to unsized locals, although that is not permitted under
/// `#![feature(unsized_locals)]` in Rust. Besides just putting "unsized locals are special and
/// different" in a bunch of places, I (JakobDegen) don't know how to incorporate this behavior into
/// the current MIR semantics in a clean way - possibly this needs some design work first.
///
/// For places that are not locals, ie they have a non-empty list of projections, we define the
/// values as a function of the parent place, that is the place with its last [`ProjectionElem`]
/// stripped. The way this is computed of course depends on the kind of that last projection
/// element:
///
///  - [`Downcast`](ProjectionElem::Downcast): This projection sets the place's variant index to the
///    given one, and makes no other changes. A `Downcast` projection on a place with its variant
///    index already set is not well-formed.
///  - [`Field`](ProjectionElem::Field): `Field` projections take their parent place and create a
///    place referring to one of the fields of the type. The resulting address is the parent
///    address, plus the offset of the field. The type becomes the type of the field. If the parent
///    was unsized and so had metadata associated with it, then the metadata is retained if the
///    field is unsized and thrown out if it is sized.
///
///    These projections are only legal for tuples, ADTs, closures, and generators. If the ADT or
///    generator has more than one variant, the parent place's variant index must be set, indicating
///    which variant is being used. If it has just one variant, the variant index may or may not be
///    included - the single possible variant is inferred if it is not included.
///  - [`ConstantIndex`](ProjectionElem::ConstantIndex): Computes an offset in units of `T` into the
///    place as described in the documentation for the `ProjectionElem`. The resulting address is
///    the parent's address plus that offset, and the type is `T`. This is only legal if the parent
///    place has type `[T;  N]` or `[T]` (*not* `&[T]`). Since such a `T` is always sized, any
///    resulting metadata is thrown out.
///  - [`Subslice`](ProjectionElem::Subslice): This projection calculates an offset and a new
///    address in a similar manner as `ConstantIndex`. It is also only legal on `[T; N]` and `[T]`.
///    However, this yields a `Place` of type `[T]`, and additionally sets the metadata to be the
///    length of the subslice.
///  - [`Index`](ProjectionElem::Index): Like `ConstantIndex`, only legal on `[T; N]` or `[T]`.
///    However, `Index` additionally takes a local from which the value of the index is computed at
///    runtime. Computing the value of the index involves interpreting the `Local` as a
///    `Place { local, projection: [] }`, and then computing its value as if done via
///    [`Operand::Copy`]. The array/slice is then indexed with the resulting value. The local must
///    have type `usize`.
///  - [`Deref`](ProjectionElem::Deref): Derefs are the last type of projection, and the most
///    complicated. They are only legal on parent places that are references, pointers, or `Box`. A
///    `Deref` projection begins by loading a value from the parent place, as if by
///    [`Operand::Copy`]. It then dereferences the resulting pointer, creating a place of the
///    pointee's type. The resulting address is the address that was stored in the pointer. If the
///    pointee type is unsized, the pointer additionally stored the value of the metadata.
///
/// Computing a place may cause UB. One possibility is that the pointer used for a `Deref` may not
/// be suitably aligned. Another possibility is that the place is not in bounds, meaning it does not
/// point to an actual allocation.
///
/// However, if this is actually UB and when the UB kicks in is undecided. This is being discussed
/// in [UCG#319]. The options include that every place must obey those rules, that only some places
/// must obey them, or that places impose no rules of their own.
///
/// [UCG#319]: https://github.com/rust-lang/unsafe-code-guidelines/issues/319
///
/// Rust currently requires that every place obey those two rules. This is checked by MIRI and taken
/// advantage of by codegen (via `gep inbounds`). That is possibly subject to change.
#[derive(Copy, Clone, PartialEq, Eq, Hash, TyEncodable, HashStable)]
pub struct Place<'tcx> {
    pub local: Local,

    /// projection out of a place (access a field, deref a pointer, etc)
    pub projection: &'tcx List<PlaceElem<'tcx>>,
}

#[cfg(all(target_arch = "x86_64", target_pointer_width = "64"))]
static_assert_size!(Place<'_>, 16);

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[derive(TyEncodable, TyDecodable, HashStable)]
pub enum ProjectionElem<V, T> {
    Deref,
    Field(Field, T),
    /// Index into a slice/array.
    ///
    /// Note that this does not also dereference, and so it does not exactly correspond to slice
    /// indexing in Rust. In other words, in the below Rust code:
    ///
    /// ```rust
    /// let x = &[1, 2, 3, 4];
    /// let i = 2;
    /// x[i];
    /// ```
    ///
    /// The `x[i]` is turned into a `Deref` followed by an `Index`, not just an `Index`. The same
    /// thing is true of the `ConstantIndex` and `Subslice` projections below.
    Index(V),

    /// These indices are generated by slice patterns. Easiest to explain
    /// by example:
    ///
    /// ```ignore (illustrative)
    /// [X, _, .._, _, _] => { offset: 0, min_length: 4, from_end: false },
    /// [_, X, .._, _, _] => { offset: 1, min_length: 4, from_end: false },
    /// [_, _, .._, X, _] => { offset: 2, min_length: 4, from_end: true },
    /// [_, _, .._, _, X] => { offset: 1, min_length: 4, from_end: true },
    /// ```
    ConstantIndex {
        /// index or -index (in Python terms), depending on from_end
        offset: u64,
        /// The thing being indexed must be at least this long. For arrays this
        /// is always the exact length.
        min_length: u64,
        /// Counting backwards from end? This is always false when indexing an
        /// array.
        from_end: bool,
    },

    /// These indices are generated by slice patterns.
    ///
    /// If `from_end` is true `slice[from..slice.len() - to]`.
    /// Otherwise `array[from..to]`.
    Subslice {
        from: u64,
        to: u64,
        /// Whether `to` counts from the start or end of the array/slice.
        /// For `PlaceElem`s this is `true` if and only if the base is a slice.
        /// For `ProjectionKind`, this can also be `true` for arrays.
        from_end: bool,
    },

    /// "Downcast" to a variant of an enum or a generator.
    ///
    /// The included Symbol is the name of the variant, used for printing MIR.
    Downcast(Option<Symbol>, VariantIdx),
}

/// Alias for projections as they appear in places, where the base is a place
/// and the index is a local.
pub type PlaceElem<'tcx> = ProjectionElem<Local, Ty<'tcx>>;

// This type is fairly frequently used, so we shouldn't unintentionally increase
// its size.
#[cfg(all(target_arch = "x86_64", target_pointer_width = "64"))]
static_assert_size!(PlaceElem<'_>, 24);

///////////////////////////////////////////////////////////////////////////
// Operands

/// An operand in MIR represents a "value" in Rust, the definition of which is undecided and part of
/// the memory model. One proposal for a definition of values can be found [on UCG][value-def].
///
/// [value-def]: https://github.com/rust-lang/unsafe-code-guidelines/blob/master/wip/value-domain.md
///
/// The most common way to create values is via loading a place. Loading a place is an operation
/// which reads the memory of the place and converts it to a value. This is a fundamentally *typed*
/// operation. The nature of the value produced depends on the type of the conversion. Furthermore,
/// there may be other effects: if the type has a validity constraint loading the place might be UB
/// if the validity constraint is not met.
///
/// **Needs clarification:** Ralf proposes that loading a place not have side-effects.
/// This is what is implemented in miri today. Are these the semantics we want for MIR? Is this
/// something we can even decide without knowing more about Rust's memory model?
///
/// **Needs clarifiation:** Is loading a place that has its variant index set well-formed? Miri
/// currently implements it, but it seems like this may be something to check against in the
/// validator.
#[derive(Clone, PartialEq, TyEncodable, TyDecodable, Hash, HashStable)]
pub enum Operand<'tcx> {
    /// Creates a value by loading the given place.
    ///
    /// Before drop elaboration, the type of the place must be `Copy`. After drop elaboration there
    /// is no such requirement.
    Copy(Place<'tcx>),

    /// Creates a value by performing loading the place, just like the `Copy` operand.
    ///
    /// This *may* additionally overwrite the place with `uninit` bytes, depending on how we decide
    /// in [UCG#188]. You should not emit MIR that may attempt a subsequent second load of this
    /// place without first re-initializing it.
    ///
    /// [UCG#188]: https://github.com/rust-lang/unsafe-code-guidelines/issues/188
    Move(Place<'tcx>),

    /// Constants are already semantically values, and remain unchanged.
    Constant(Box<Constant<'tcx>>),
}

#[cfg(all(target_arch = "x86_64", target_pointer_width = "64"))]
static_assert_size!(Operand<'_>, 24);

///////////////////////////////////////////////////////////////////////////
/// Rvalues

/// The various kinds of rvalues that can appear in MIR.
///
/// Not all of these are allowed at every [`MirPhase`] - when this is the case, it's stated below.
///
/// Computing any rvalue begins by evaluating the places and operands in some order (**Needs
/// clarification**: Which order?). These are then used to produce a "value" - the same kind of
/// value that an [`Operand`] produces.
#[derive(Clone, TyEncodable, TyDecodable, Hash, HashStable, PartialEq)]
pub enum Rvalue<'tcx> {
    /// Yields the operand unchanged
    Use(Operand<'tcx>),

    /// Creates an array where each element is the value of the operand.
    ///
    /// This is the cause of a bug in the case where the repetition count is zero because the value
    /// is not dropped, see [#74836].
    ///
    /// Corresponds to source code like `[x; 32]`.
    ///
    /// [#74836]: https://github.com/rust-lang/rust/issues/74836
    Repeat(Operand<'tcx>, ty::Const<'tcx>),

    /// Creates a reference of the indicated kind to the place.
    ///
    /// There is not much to document here, because besides the obvious parts the semantics of this
    /// are essentially entirely a part of the aliasing model. There are many UCG issues discussing
    /// exactly what the behavior of this operation should be.
    ///
    /// `Shallow` borrows are disallowed after drop lowering.
    Ref(Region<'tcx>, BorrowKind, Place<'tcx>),

    /// Creates a pointer/reference to the given thread local.
    ///
    /// The yielded type is a `*mut T` if the static is mutable, otherwise if the static is extern a
    /// `*const T`, and if neither of those apply a `&T`.
    ///
    /// **Note:** This is a runtime operation that actually executes code and is in this sense more
    /// like a function call. Also, eliminating dead stores of this rvalue causes `fn main() {}` to
    /// SIGILL for some reason that I (JakobDegen) never got a chance to look into.
    ///
    /// **Needs clarification**: Are there weird additional semantics here related to the runtime
    /// nature of this operation?
    ThreadLocalRef(DefId),

    /// Creates a pointer with the indicated mutability to the place.
    ///
    /// This is generated by pointer casts like `&v as *const _` or raw address of expressions like
    /// `&raw v` or `addr_of!(v)`.
    ///
    /// Like with references, the semantics of this operation are heavily dependent on the aliasing
    /// model.
    AddressOf(Mutability, Place<'tcx>),

    /// Yields the length of the place, as a `usize`.
    ///
    /// If the type of the place is an array, this is the array length. For slices (`[T]`, not
    /// `&[T]`) this accesses the place's metadata to determine the length. This rvalue is
    /// ill-formed for places of other types.
    Len(Place<'tcx>),

    /// Performs essentially all of the casts that can be performed via `as`.
    ///
    /// This allows for casts from/to a variety of types.
    ///
    /// **FIXME**: Document exactly which `CastKind`s allow which types of casts. Figure out why
    /// `ArrayToPointer` and `MutToConstPointer` are special.
    Cast(CastKind, Operand<'tcx>, Ty<'tcx>),

    /// * `Offset` has the same semantics as [`offset`](pointer::offset), except that the second
    ///   parameter may be a `usize` as well.
    /// * The comparison operations accept `bool`s, `char`s, signed or unsigned integers, floats,
    ///   raw pointers, or function pointers and return a `bool`. The types of the operands must be
    ///   matching, up to the usual caveat of the lifetimes in function pointers.
    /// * Left and right shift operations accept signed or unsigned integers not necessarily of the
    ///   same type and return a value of the same type as their LHS. Like in Rust, the RHS is
    ///   truncated as needed.
    /// * The `Bit*` operations accept signed integers, unsigned integers, or bools with matching
    ///   types and return a value of that type.
    /// * The remaining operations accept signed integers, unsigned integers, or floats with
    ///   matching types and return a value of that type.
    BinaryOp(BinOp, Box<(Operand<'tcx>, Operand<'tcx>)>),

    /// Same as `BinaryOp`, but yields `(T, bool)` instead of `T`. In addition to performing the
    /// same computation as the matching `BinaryOp`, checks if the infinite precison result would be
    /// unequal to the actual result and sets the `bool` if this is the case.
    ///
    /// This only supports addition, subtraction, multiplication, and shift operations on integers.
    CheckedBinaryOp(BinOp, Box<(Operand<'tcx>, Operand<'tcx>)>),

    /// Computes a value as described by the operation.
    NullaryOp(NullOp, Ty<'tcx>),

    /// Exactly like `BinaryOp`, but less operands.
    ///
    /// Also does two's-complement arithmetic. Negation requires a signed integer or a float;
    /// bitwise not requires a signed integer, unsigned integer, or bool. Both operation kinds
    /// return a value with the same type as their operand.
    UnaryOp(UnOp, Operand<'tcx>),

    /// Computes the discriminant of the place, returning it as an integer of type
    /// [`discriminant_ty`]. Returns zero for types without discriminant.
    ///
    /// The validity requirements for the underlying value are undecided for this rvalue, see
    /// [#91095]. Note too that the value of the discriminant is not the same thing as the
    /// variant index; use [`discriminant_for_variant`] to convert.
    ///
    /// [`discriminant_ty`]: crate::ty::Ty::discriminant_ty
    /// [#91095]: https://github.com/rust-lang/rust/issues/91095
    /// [`discriminant_for_variant`]: crate::ty::Ty::discriminant_for_variant
    Discriminant(Place<'tcx>),

    /// Creates an aggregate value, like a tuple or struct.
    ///
    /// This is needed because dataflow analysis needs to distinguish
    /// `dest = Foo { x: ..., y: ... }` from `dest.x = ...; dest.y = ...;` in the case that `Foo`
    /// has a destructor.
    ///
    /// Disallowed after deaggregation for all aggregate kinds except `Array` and `Generator`. After
    /// generator lowering, `Generator` aggregate kinds are disallowed too.
    Aggregate(Box<AggregateKind<'tcx>>, Vec<Operand<'tcx>>),

    /// Transmutes a `*mut u8` into shallow-initialized `Box<T>`.
    ///
    /// This is different from a normal transmute because dataflow analysis will treat the box as
    /// initialized but its content as uninitialized. Like other pointer casts, this in general
    /// affects alias analysis.
    ShallowInitBox(Operand<'tcx>, Ty<'tcx>),
}

#[cfg(all(target_arch = "x86_64", target_pointer_width = "64"))]
static_assert_size!(Rvalue<'_>, 40);

#[derive(Clone, Copy, Debug, PartialEq, Eq, TyEncodable, TyDecodable, Hash, HashStable)]
pub enum CastKind {
    /// An exposing pointer to address cast. A cast between a pointer and an integer type, or
    /// between a function pointer and an integer type.
    /// See the docs on `expose_addr` for more details.
    PointerExposeAddress,
    /// An address-to-pointer cast that picks up an exposed provenance.
    /// See the docs on `from_exposed_addr` for more details.
    PointerFromExposedAddress,
    /// All sorts of pointer-to-pointer casts. Note that reference-to-raw-ptr casts are
    /// translated into `&raw mut/const *r`, i.e., they are not actually casts.
    Pointer(PointerCast),
    /// Remaining unclassified casts.
    Misc,
}

#[derive(Clone, Debug, PartialEq, Eq, TyEncodable, TyDecodable, Hash, HashStable)]
pub enum AggregateKind<'tcx> {
    /// The type is of the element
    Array(Ty<'tcx>),
    Tuple,

    /// The second field is the variant index. It's equal to 0 for struct
    /// and union expressions. The fourth field is
    /// active field number and is present only for union expressions
    /// -- e.g., for a union expression `SomeUnion { c: .. }`, the
    /// active field index would identity the field `c`
    Adt(DefId, VariantIdx, SubstsRef<'tcx>, Option<UserTypeAnnotationIndex>, Option<usize>),

    Closure(DefId, SubstsRef<'tcx>),
    Generator(DefId, SubstsRef<'tcx>, hir::Movability),
}

#[cfg(all(target_arch = "x86_64", target_pointer_width = "64"))]
static_assert_size!(AggregateKind<'_>, 48);

#[derive(Copy, Clone, Debug, PartialEq, Eq, TyEncodable, TyDecodable, Hash, HashStable)]
pub enum NullOp {
    /// Returns the size of a value of that type
    SizeOf,
    /// Returns the minimum alignment of a type
    AlignOf,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, TyEncodable, TyDecodable, Hash, HashStable)]
pub enum UnOp {
    /// The `!` operator for logical inversion
    Not,
    /// The `-` operator for negation
    Neg,
}

#[derive(Copy, Clone, Debug, PartialEq, PartialOrd, Eq, TyEncodable, TyDecodable, Hash, HashStable)]
pub enum BinOp {
    /// The `+` operator (addition)
    Add,
    /// The `-` operator (subtraction)
    Sub,
    /// The `*` operator (multiplication)
    Mul,
    /// The `/` operator (division)
    ///
    /// Division by zero is UB, because the compiler should have inserted checks
    /// prior to this.
    Div,
    /// The `%` operator (modulus)
    ///
    /// Using zero as the modulus (second operand) is UB, because the compiler
    /// should have inserted checks prior to this.
    Rem,
    /// The `^` operator (bitwise xor)
    BitXor,
    /// The `&` operator (bitwise and)
    BitAnd,
    /// The `|` operator (bitwise or)
    BitOr,
    /// The `<<` operator (shift left)
    ///
    /// The offset is truncated to the size of the first operand before shifting.
    Shl,
    /// The `>>` operator (shift right)
    ///
    /// The offset is truncated to the size of the first operand before shifting.
    Shr,
    /// The `==` operator (equality)
    Eq,
    /// The `<` operator (less than)
    Lt,
    /// The `<=` operator (less than or equal to)
    Le,
    /// The `!=` operator (not equal to)
    Ne,
    /// The `>=` operator (greater than or equal to)
    Ge,
    /// The `>` operator (greater than)
    Gt,
    /// The `ptr.offset` operator
    Offset,
}
