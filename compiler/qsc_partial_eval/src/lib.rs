// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod evaluation_context;
mod management;

use evaluation_context::{EvaluationContext, Scope};
use management::{QuantumIntrinsicsChecker, ResourceManager};
use miette::Diagnostic;
use qsc_data_structures::functors::FunctorApp;
use qsc_data_structures::span::Span;
use qsc_eval::{
    self, exec_graph_section,
    output::GenericReceiver,
    val::{self, Value, Var},
    State, StepAction, StepResult,
};
use qsc_fir::{
    fir::{
        BinOp, Block, BlockId, CallableDecl, CallableImpl, ExecGraphNode, Expr, ExprId, ExprKind,
        Global, Ident, PackageId, PackageStore, PackageStoreLookup, Pat, PatId, PatKind, Res,
        SpecDecl, SpecImpl, Stmt, StmtId, StmtKind, StoreBlockId, StoreExprId, StoreItemId,
        StorePatId, StoreStmtId,
    },
    ty::{Prim, Ty},
    visit::Visitor,
};
use qsc_rca::{ComputeKind, ComputePropertiesLookup, PackageStoreComputeProperties};
use qsc_rir::rir::{
    self, Callable, CallableId, CallableType, ConditionCode, Instruction, Literal, Operand,
    Program, Variable,
};
use rustc_hash::FxHashMap;
use std::{collections::hash_map::Entry, rc::Rc, result::Result};
use thiserror::Error;

pub fn partially_evaluate(
    package_id: PackageId,
    package_store: &PackageStore,
    compute_properties: &PackageStoreComputeProperties,
) -> Result<Program, Error> {
    let partial_evaluator = PartialEvaluator::new(package_id, package_store, compute_properties);
    partial_evaluator.eval()
}

#[derive(Clone, Debug, Diagnostic, Error)]
pub enum Error {
    #[error("partial evaluation error: {0}")]
    #[diagnostic(code("Qsc.PartialEval.EvaluationFailed"))]
    EvaluationFailed(qsc_eval::Error),
    #[error("failed to evaluate {0}")]
    #[diagnostic(code("Qsc.PartialEval.FailedToEvaluateCallable"))]
    FailedToEvaluateCallable(String, #[label] Span),
    #[error("failed to evaluate callee expression")]
    #[diagnostic(code("Qsc.PartialEval.FailedToEvaluateCalleeExpression"))]
    FailedToEvaluateCalleeExpression(#[label] Span),
    #[error("failed to evaluate tuple element expression")]
    #[diagnostic(code("Qsc.PartialEval.FailedToEvaluateTupleElementExpression"))]
    FailedToEvaluateTupleElementExpression(#[label] Span),
    #[error("failed to evaluate binary expression operand")]
    #[diagnostic(code("Qsc.PartialEval.FailedToEvaluateBinaryExpressionOperand"))]
    FailedToEvaluateBinaryExpressionOperand(#[label] Span),
    #[error("failed to evaluate: {0} not yet implemented")]
    #[diagnostic(code("Qsc.PartialEval.Unimplemented"))]
    Unimplemented(String, #[label] Span),
}

struct PartialEvaluator<'a> {
    package_store: &'a PackageStore,
    compute_properties: &'a PackageStoreComputeProperties,
    resource_manager: ResourceManager,
    backend: QuantumIntrinsicsChecker,
    callables_map: FxHashMap<Rc<str>, CallableId>,
    eval_context: EvaluationContext,
    program: Program,
    errors: Vec<Error>,
}

impl<'a> PartialEvaluator<'a> {
    fn new(
        entry_package_id: PackageId,
        package_store: &'a PackageStore,
        compute_properties: &'a PackageStoreComputeProperties,
    ) -> Self {
        // Create the entry-point callable.
        let mut resource_manager = ResourceManager::default();
        let mut program = Program::new();
        let entry_block_id = resource_manager.next_block();
        let entry_block = rir::Block(Vec::new());
        program.blocks.insert(entry_block_id, entry_block);
        let entry_point_id = resource_manager.next_callable();
        let entry_point = rir::Callable {
            name: "main".into(),
            input_type: Vec::new(),
            output_type: None,
            body: Some(entry_block_id),
            call_type: CallableType::Regular,
        };
        program.callables.insert(entry_point_id, entry_point);
        program.entry = entry_point_id;

        // Initialize the evaluation context and create a new partial evaluator.
        let context = EvaluationContext::new(entry_package_id, entry_block_id);
        Self {
            package_store,
            compute_properties,
            eval_context: context,
            resource_manager,
            backend: QuantumIntrinsicsChecker::default(),
            callables_map: FxHashMap::default(),
            program,
            errors: Vec::new(),
        }
    }

    fn bind_expr_to_pat(&mut self, pat_id: PatId, expr_id: ExprId) {
        let pat = self.get_pat(pat_id);
        match &pat.kind {
            PatKind::Bind(ident) => {
                let expr_value = self
                    .eval_context
                    .get_current_scope_mut()
                    .get_expr_value(expr_id)
                    .clone();
                self.bind_value_to_ident(ident, expr_value);
            }
            PatKind::Tuple(pats) => {
                let expr = self.get_expr(expr_id);
                match &expr.kind {
                    ExprKind::Tuple(exprs) => {
                        assert!(pats.len() == exprs.len());
                        for (pat_id, expr_id) in pats.iter().zip(exprs.iter()) {
                            self.bind_expr_to_pat(*pat_id, *expr_id);
                        }
                    }
                    _ => panic!("expected a tuple expression to bind to a tuple pattern"),
                };
            }
            PatKind::Discard => {
                // Nothing to bind to.
            }
        }
    }

    fn bind_value_to_ident(&mut self, ident: &Ident, value: Value) {
        self.eval_context
            .get_current_scope_mut()
            .insert_local_var_value(ident.id, value);
    }

    fn create_intrinsic_callable(
        &self,
        store_item_id: StoreItemId,
        callable_decl: &CallableDecl,
    ) -> Callable {
        let callable_package = self.package_store.get(store_item_id.package);
        let name = callable_decl.name.name.to_string();
        let input_type: Vec<rir::Ty> = callable_package
            .derive_callable_input_params(callable_decl)
            .iter()
            .map(|input_param| map_fir_type_to_rir_type(&input_param.ty))
            .collect();
        let output_type = if callable_decl.output == Ty::UNIT {
            None
        } else {
            Some(map_fir_type_to_rir_type(&callable_decl.output))
        };
        let body = None;
        let call_type = if name.eq("__quantum__qis__reset__body") {
            CallableType::Reset
        } else {
            CallableType::Regular
        };
        Callable {
            name,
            input_type,
            output_type,
            body,
            call_type,
        }
    }

    fn eval(mut self) -> Result<Program, Error> {
        let current_package = self.get_current_package_id();
        let entry_package = self.package_store.get(current_package);
        let Some(entry_expr_id) = entry_package.entry else {
            panic!("package does not have an entry expression");
        };

        // Visit the entry point expression.
        self.visit_expr(entry_expr_id);

        // Return the first error, if any.
        // We should eventually return all the errors but since that is an interface change, we will do that as its own
        // change.
        if let Some(error) = self.errors.pop() {
            return Err(error);
        }

        // Insert the return expression and return the generated program.
        let current_block = self
            .program
            .blocks
            .get_mut(self.eval_context.current_block)
            .expect("block does not exist");
        current_block.0.push(Instruction::Return);
        Ok(self.program)
    }

    fn eval_classical_expr(&mut self, expr_id: ExprId) -> Result<Value, Error> {
        let current_package_id = self.get_current_package_id();
        let store_expr_id = StoreExprId::from((current_package_id, expr_id));
        let expr = self.package_store.get_expr(store_expr_id);
        let scope_exec_graph = self.get_current_scope_exec_graph().clone();
        let scope = self.eval_context.get_current_scope_mut();
        let exec_graph = exec_graph_section(&scope_exec_graph, expr.exec_graph_range.clone());
        let mut state = State::new(current_package_id, exec_graph, None);
        let eval_result = state.eval(
            self.package_store,
            &mut scope.env,
            &mut self.backend,
            &mut GenericReceiver::new(&mut std::io::sink()),
            &[],
            StepAction::Continue,
        );
        match eval_result {
            Ok(step_result) => {
                let StepResult::Return(value) = step_result else {
                    panic!("evaluating a classical expression should always return a value");
                };
                Ok(value)
            }
            Err((error, _)) => Err(Error::EvaluationFailed(error)),
        }
    }

    fn eval_classical_stmt(&mut self, stmt_id: StmtId) {
        let current_package_id = self.get_current_package_id();
        let store_stmt_id = StoreStmtId::from((current_package_id, stmt_id));
        let stmt = self.package_store.get_stmt(store_stmt_id);
        let scope_exec_graph = self.get_current_scope_exec_graph().clone();
        let scope = self.eval_context.get_current_scope_mut();
        let exec_graph = exec_graph_section(&scope_exec_graph, stmt.exec_graph_range.clone());
        let mut state = State::new(current_package_id, exec_graph, None);
        let eval_result = state.eval(
            self.package_store,
            &mut scope.env,
            &mut self.backend,
            &mut GenericReceiver::new(&mut std::io::sink()),
            &[],
            StepAction::Continue,
        );
        if let Err((eval_error, _)) = eval_result {
            self.errors.push(Error::EvaluationFailed(eval_error));
        }
    }

    fn eval_expr(&mut self, expr_id: ExprId) -> Result<Value, Error> {
        let current_package_id = self.get_current_package_id();
        let store_expr_id = StoreExprId::from((current_package_id, expr_id));
        let expr = self.package_store.get_expr(store_expr_id);
        match &expr.kind {
            ExprKind::Array(_) => Err(Error::Unimplemented("Array Expr".to_string(), expr.span)),
            ExprKind::ArrayLit(_) => Err(Error::Unimplemented("Array Lit".to_string(), expr.span)),
            ExprKind::ArrayRepeat(_, _) => {
                Err(Error::Unimplemented("Array Repeat".to_string(), expr.span))
            }
            ExprKind::Assign(_, _) => Err(Error::Unimplemented(
                "Assignment Expr".to_string(),
                expr.span,
            )),
            ExprKind::AssignField(_, _, _) => Err(Error::Unimplemented(
                "Field Assignment Expr".to_string(),
                expr.span,
            )),
            ExprKind::AssignIndex(_, _, _) => Err(Error::Unimplemented(
                "Assignment Index Expr".to_string(),
                expr.span,
            )),
            ExprKind::AssignOp(_, _, _) => Err(Error::Unimplemented(
                "Assignment Op Expr".to_string(),
                expr.span,
            )),
            ExprKind::BinOp(bin_op, lhs_expr_id, rhs_expr_id) => {
                self.eval_expr_bin_op(expr_id, *bin_op, *lhs_expr_id, *rhs_expr_id)
            }
            ExprKind::Block(_) => Err(Error::Unimplemented("Block Expr".to_string(), expr.span)),
            ExprKind::Call(callee_expr_id, args_expr_id) => {
                self.eval_expr_call(*callee_expr_id, *args_expr_id)
            }
            ExprKind::Closure(_, _) => {
                panic!("instruction generation for closure expressions is unsupported")
            }
            ExprKind::Fail(_) => panic!("instruction generation for fail expression is invalid"),
            ExprKind::Field(_, _) => Err(Error::Unimplemented("Field Expr".to_string(), expr.span)),
            ExprKind::Hole => panic!("instruction generation for hole expressions is invalid"),
            ExprKind::If(_, _, _) => Err(Error::Unimplemented("If Expr".to_string(), expr.span)),
            ExprKind::Index(_, _) => Err(Error::Unimplemented("Index Expr".to_string(), expr.span)),
            ExprKind::Lit(_) => panic!("instruction generation for literal expressions is invalid"),
            ExprKind::Range(_, _, _) => {
                panic!("instruction generation for range expressions is invalid")
            }
            ExprKind::Return(_) => Err(Error::Unimplemented("Return Expr".to_string(), expr.span)),
            ExprKind::String(_) => {
                panic!("instruction generation for string expressions is invalid")
            }
            ExprKind::Tuple(exprs) => self.eval_expr_tuple(exprs),
            ExprKind::UnOp(_, _) => Err(Error::Unimplemented("Unary Expr".to_string(), expr.span)),
            ExprKind::UpdateField(_, _, _) => Err(Error::Unimplemented(
                "Updated Field Expr".to_string(),
                expr.span,
            )),
            ExprKind::UpdateIndex(_, _, _) => Err(Error::Unimplemented(
                "Update Index Expr".to_string(),
                expr.span,
            )),
            ExprKind::Var(res, _) => Ok(self.eval_expr_var(res)),
            ExprKind::While(_, _) => Err(Error::Unimplemented("While Expr".to_string(), expr.span)),
        }
    }

    #[allow(clippy::similar_names)]
    fn eval_expr_bin_op(
        &mut self,
        bin_op_expr_id: ExprId,
        bin_op: BinOp,
        lhs_expr_id: ExprId,
        rhs_expr_id: ExprId,
    ) -> Result<Value, Error> {
        // Visit the both the LHS and RHS expressions to get their value.
        let maybe_lhs_expr_value = self.try_eval_expr(lhs_expr_id);
        let Ok(lhs_expr_value) = maybe_lhs_expr_value else {
            let lhs_expr = self.get_expr(lhs_expr_id);
            let error = Error::FailedToEvaluateBinaryExpressionOperand(lhs_expr.span);
            return Err(error);
        };

        let maybe_rhs_expr_value = self.try_eval_expr(rhs_expr_id);
        let Ok(rhs_expr_value) = maybe_rhs_expr_value else {
            let rhs_expr = self.get_expr(rhs_expr_id);
            let error = Error::FailedToEvaluateBinaryExpressionOperand(rhs_expr.span);
            return Err(error);
        };

        // Get the operands to use when generating the binary operation instruction depending on the type of the
        // expression's value.
        let lhs_operand = if let Value::Result(result) = lhs_expr_value {
            self.eval_result_as_bool_operand(result)
        } else {
            map_eval_value_to_rir_operand(&lhs_expr_value)
        };
        let rhs_operand = if let Value::Result(result) = rhs_expr_value {
            self.eval_result_as_bool_operand(result)
        } else {
            map_eval_value_to_rir_operand(&rhs_expr_value)
        };

        // Create a variable to store the result of the expression.
        let bin_op_expr = self.get_expr(bin_op_expr_id);
        let variable_id = self.resource_manager.next_var();
        let variable_ty = map_fir_type_to_rir_type(&bin_op_expr.ty);
        let variable = Variable {
            variable_id,
            ty: variable_ty,
        };

        // Create the binary operation instruction and add it to the current block.
        let instruction = match bin_op {
            BinOp::Eq => Instruction::Icmp(ConditionCode::Eq, lhs_operand, rhs_operand, variable),
            BinOp::Neq => Instruction::Icmp(ConditionCode::Ne, lhs_operand, rhs_operand, variable),
            _ => {
                return Err(Error::Unimplemented(
                    format!("BinOp Expr ({bin_op:?})"),
                    bin_op_expr.span,
                ))
            }
        };
        let current_block = self.get_current_block_mut();
        current_block.0.push(instruction);

        // Return the variable as a value.
        let value = Value::Var(Var(variable_id.into()));
        Ok(value)
    }

    fn eval_expr_call(
        &mut self,
        callee_expr_id: ExprId,
        args_expr_id: ExprId,
    ) -> Result<Value, Error> {
        // Visit the both the callee and arguments expressions to get their value.
        let maybe_callable_value = self.try_eval_expr(callee_expr_id);
        let Ok(callable_value) = maybe_callable_value else {
            let callee_expr = self.get_expr(callee_expr_id);
            let error = Error::FailedToEvaluateCalleeExpression(callee_expr.span);
            return Err(error);
        };

        let maybe_args_value = self.try_eval_expr(args_expr_id);
        let Ok(args_value) = maybe_args_value else {
            let args_expr = self.get_expr(args_expr_id);
            let error = Error::FailedToEvaluateCalleeExpression(args_expr.span);
            return Err(error);
        };

        // Get the callable.
        let Value::Global(store_item_id, functor_app) = callable_value else {
            panic!("callee expression is expected to be a global");
        };
        let global = self
            .package_store
            .get_global(store_item_id)
            .expect("global not present");
        let Global::Callable(callable_decl) = global else {
            // Instruction generation for UDTs is not supported.
            panic!("global is not a callable");
        };

        // We generate instructions differently depending on whether we are calling an intrinsic or a specialization
        // with an implementation.
        match &callable_decl.implementation {
            CallableImpl::Intrinsic => {
                let value =
                    self.eval_expr_call_to_intrinsic(store_item_id, callable_decl, args_value);
                Ok(value)
            }
            CallableImpl::Spec(spec_impl) => {
                self.eval_expr_call_to_spec(store_item_id, functor_app, spec_impl, args_expr_id)
            }
        }
    }

    fn eval_expr_call_to_intrinsic(
        &mut self,
        store_item_id: StoreItemId,
        callable_decl: &CallableDecl,
        args_value: Value,
    ) -> Value {
        // There are a few special cases regarding intrinsic callables: qubit allocation/release and measurements.
        // Identify them and handle them properly.
        match callable_decl.name.name.as_ref() {
            "__quantum__rt__qubit_allocate" => self.allocate_qubit(),
            "__quantum__rt__qubit_release" => self.release_qubit(&args_value),
            "__quantum__qis__m__body" => self.measure_qubit(mz_callable(), &args_value),
            "__quantum__qis__mresetz__body" => self.measure_qubit(mresetz_callable(), &args_value),
            _ => self.eval_expr_call_to_intrinsic_qis(store_item_id, callable_decl, args_value),
        }
    }

    fn eval_expr_call_to_intrinsic_qis(
        &mut self,
        store_item_id: StoreItemId,
        callable_decl: &CallableDecl,
        args_value: Value,
    ) -> Value {
        // Check if the callable is already in the program, and if not add it.
        let callable = self.create_intrinsic_callable(store_item_id, callable_decl);
        let callable_id = self.get_or_insert_callable(callable);

        // Resove the call arguments, create the call instruction and insert it to the current block.
        let args = resolve_call_arg_operands(args_value);
        // Note that we currently just support calls to unitary operations.
        let instruction = Instruction::Call(callable_id, args, None);
        let current_block = self.get_current_block_mut();
        current_block.0.push(instruction);
        Value::unit()
    }

    fn eval_expr_call_to_spec(
        &mut self,
        global_callable_id: StoreItemId,
        functor_app: FunctorApp,
        spec_impl: &SpecImpl,
        _args_expr_id: ExprId,
    ) -> Result<Value, Error> {
        let spec_decl = get_spec_decl(spec_impl, functor_app);

        // We are currently not setting the argument values in a way that supports arbitrary calls, but we'll add that
        // support later.
        let callable_scope = Scope::new(
            global_callable_id.package,
            Some((global_callable_id.item, functor_app)),
            Vec::new(),
        );
        self.eval_context.push_scope(callable_scope);
        self.visit_block(spec_decl.block);
        let popped_scope = self.eval_context.pop_scope();
        assert!(
            popped_scope.package_id == global_callable_id.package,
            "scope package ID mismatch"
        );
        let (popped_callable_id, popped_functor_app) = popped_scope
            .callable
            .expect("callable in scope is not specified");
        assert!(
            popped_callable_id == global_callable_id.item,
            "scope callable ID mismatch"
        );
        assert!(popped_functor_app == functor_app, "scope functor mismatch");

        // Check whether evaluating the block failed.
        if self.errors.is_empty() {
            // Once we have proper support for evaluating all kinds of callables (not just parameterless unitary
            // callables), we should the variable that stores the callable return value here.
            Ok(Value::unit())
        } else {
            // Evaluating the block failed, generate an error specific to the callable.
            let global_callable = self
                .package_store
                .get_global(global_callable_id)
                .expect("global does not exist");
            let Global::Callable(callable_decl) = global_callable else {
                panic!("global is not a callable");
            };
            Err(Error::FailedToEvaluateCallable(
                callable_decl.name.name.to_string(),
                callable_decl.name.span,
            ))
        }
    }

    fn eval_expr_tuple(&mut self, exprs: &Vec<ExprId>) -> Result<Value, Error> {
        let mut values = Vec::<Value>::new();
        for expr_id in exprs {
            let maybe_value = self.try_eval_expr(*expr_id);
            let Ok(value) = maybe_value else {
                let expr = self.get_expr(*expr_id);
                return Err(Error::FailedToEvaluateTupleElementExpression(expr.span));
            };
            values.push(value);
        }
        Ok(Value::Tuple(values.into()))
    }

    fn eval_expr_var(&mut self, res: &Res) -> Value {
        match res {
            Res::Err => panic!("resolution error"),
            Res::Item(item) => Value::Global(
                StoreItemId {
                    package: item.package.unwrap_or(self.get_current_package_id()),
                    item: item.item,
                },
                FunctorApp::default(),
            ),
            Res::Local(local_var_id) => self
                .eval_context
                .get_current_scope()
                .get_local_var_value(*local_var_id)
                .clone(),
        }
    }

    fn eval_result_as_bool_operand(&mut self, result: val::Result) -> Operand {
        match result {
            val::Result::Id(id) => {
                // If this is a result ID, generate the instruction to read it.
                let result_operand = Operand::Literal(Literal::Result(
                    id.try_into().expect("could not convert result ID to u32"),
                ));
                let read_result_callable_id = self.get_or_insert_callable(read_result_callable());
                let variable_id = self.resource_manager.next_var();
                let variable_ty = rir::Ty::Boolean;
                let variable = Variable {
                    variable_id,
                    ty: variable_ty,
                };
                let instruction = Instruction::Call(
                    read_result_callable_id,
                    vec![result_operand],
                    Some(variable),
                );
                let current_block = self.get_current_block_mut();
                current_block.0.push(instruction);
                Operand::Variable(variable)
            }
            val::Result::Val(bool) => Operand::Literal(Literal::Bool(bool)),
        }
    }

    fn get_current_block_mut(&mut self) -> &mut rir::Block {
        self.program
            .blocks
            .get_mut(self.eval_context.current_block)
            .expect("block does not exist")
    }

    fn get_current_package_id(&self) -> PackageId {
        self.eval_context.get_current_scope().package_id
    }

    fn get_current_scope_exec_graph(&self) -> &Rc<[ExecGraphNode]> {
        if let Some(spec_decl) = self.get_current_scope_spec_decl() {
            &spec_decl.exec_graph
        } else {
            let package_id = self.get_current_package_id();
            let package = self.package_store.get(package_id);
            &package.entry_exec_graph
        }
    }

    fn get_current_scope_spec_decl(&self) -> Option<&SpecDecl> {
        let current_scope = self.eval_context.get_current_scope();
        let (local_item_id, functor_app) = current_scope.callable?;
        let store_item_id = StoreItemId::from((current_scope.package_id, local_item_id));
        let global = self
            .package_store
            .get_global(store_item_id)
            .expect("global does not exist");
        let Global::Callable(callable_decl) = global else {
            panic!("global is not a callable");
        };

        let CallableImpl::Spec(spec_impl) = &callable_decl.implementation else {
            panic!("callable does not implement specializations");
        };

        let spec_decl = get_spec_decl(spec_impl, functor_app);
        Some(spec_decl)
    }

    fn get_or_insert_callable(&mut self, callable: Callable) -> CallableId {
        // Check if the callable is already in the program, and if not add it.
        let callable_name = callable.name.clone();
        if let Entry::Vacant(entry) = self.callables_map.entry(callable_name.clone().into()) {
            let callable_id = self.resource_manager.next_callable();
            entry.insert(callable_id);
            self.program.callables.insert(callable_id, callable);
        }

        *self
            .callables_map
            .get(callable_name.as_str())
            .expect("callable not present")
    }

    fn is_classical_stmt(&self, stmt_id: StmtId) -> bool {
        let current_package_id = self.get_current_package_id();
        let store_stmt_id = StoreStmtId::from((current_package_id, stmt_id));
        let stmt_generator_set = self.compute_properties.get_stmt(store_stmt_id);
        let callable_scope = self.eval_context.get_current_scope();
        let compute_kind = stmt_generator_set
            .generate_application_compute_kind(&callable_scope.args_runtime_properties);
        matches!(compute_kind, ComputeKind::Classical)
    }

    fn allocate_qubit(&mut self) -> Value {
        let qubit = self.resource_manager.allocate_qubit();
        Value::Qubit(qubit)
    }

    fn measure_qubit(&mut self, measure_callable: Callable, args_value: &Value) -> Value {
        // Get the qubit and result IDs to use in the qubit measure instruction.
        let Value::Qubit(qubit) = args_value else {
            panic!("argument to qubit measure is expected to be a qubit");
        };
        let qubit_value = Value::Qubit(*qubit);
        let qubit_operand = map_eval_value_to_rir_operand(&qubit_value);
        let result_value = Value::Result(self.resource_manager.next_result());
        let result_operand = map_eval_value_to_rir_operand(&result_value);

        // Check if the callable has already been added to the program and if not do so now.
        let measure_callable_id = self.get_or_insert_callable(measure_callable);
        let args = vec![qubit_operand, result_operand];
        let instruction = Instruction::Call(measure_callable_id, args, None);
        let current_block = self.get_current_block_mut();
        current_block.0.push(instruction);

        // Return the result value.
        result_value
    }

    fn release_qubit(&mut self, args_value: &Value) -> Value {
        let Value::Qubit(qubit) = args_value else {
            panic!("argument to qubit release is expected to be a qubit");
        };
        self.resource_manager.release_qubit(*qubit);

        // The value of a qubit release is unit.
        Value::unit()
    }

    fn try_eval_expr(&mut self, expr_id: ExprId) -> Result<Value, ()> {
        // Visit the expression, which will either populate the expression entry in the scope's value map or add an
        // error.
        self.visit_expr(expr_id);
        if self.errors.is_empty() {
            let expr_value = self
                .eval_context
                .get_current_scope()
                .get_expr_value(expr_id);
            Ok(expr_value.clone())
        } else {
            Err(())
        }
    }
}

impl<'a> Visitor<'a> for PartialEvaluator<'a> {
    fn get_block(&self, id: BlockId) -> &'a Block {
        let block_id = StoreBlockId::from((self.get_current_package_id(), id));
        self.package_store.get_block(block_id)
    }

    fn get_expr(&self, id: ExprId) -> &'a Expr {
        let expr_id = StoreExprId::from((self.get_current_package_id(), id));
        self.package_store.get_expr(expr_id)
    }

    fn get_pat(&self, id: PatId) -> &'a Pat {
        let pat_id = StorePatId::from((self.get_current_package_id(), id));
        self.package_store.get_pat(pat_id)
    }

    fn get_stmt(&self, id: StmtId) -> &'a Stmt {
        let stmt_id = StoreStmtId::from((self.get_current_package_id(), id));
        self.package_store.get_stmt(stmt_id)
    }

    fn visit_block(&mut self, block: BlockId) {
        let block = self.get_block(block);
        for stmt_id in &block.stmts {
            self.visit_stmt(*stmt_id);
            // Stop processing more statements if an error occurred.
            if !self.errors.is_empty() {
                return;
            }
        }
    }

    fn visit_expr(&mut self, expr_id: ExprId) {
        assert!(
            self.errors.is_empty(),
            "visiting an expression when errors have already happened should never happen"
        );

        // Determine whether the expression is classical since how we evaluate it depends on it.
        let current_package_id = self.get_current_package_id();
        let store_expr_id = StoreExprId::from((current_package_id, expr_id));
        let expr_generator_set = self.compute_properties.get_expr(store_expr_id);
        let callable_scope = self.eval_context.get_current_scope();
        let compute_kind = expr_generator_set
            .generate_application_compute_kind(&callable_scope.args_runtime_properties);
        let expr_result = if matches!(compute_kind, ComputeKind::Classical) {
            self.eval_classical_expr(expr_id)
        } else {
            self.eval_expr(expr_id)
        };

        // If the evaluation was successful, insert its value to the scope's expression map.
        match expr_result {
            Result::Ok(expr_value) => self
                .eval_context
                .get_current_scope_mut()
                .insert_expr_value(expr_id, expr_value),
            Result::Err(error) => self.errors.push(error),
        };
    }

    fn visit_stmt(&mut self, stmt_id: StmtId) {
        // If the statement is classical, we can just evaluate it.
        if self.is_classical_stmt(stmt_id) {
            self.eval_classical_stmt(stmt_id);
            return;
        }

        // If the statement is not classical, we need to generate instructions for it.
        let store_stmt_id = StoreStmtId::from((self.get_current_package_id(), stmt_id));
        let stmt = self.package_store.get_stmt(store_stmt_id);
        match stmt.kind {
            StmtKind::Expr(expr_id) | StmtKind::Semi(expr_id) => {
                self.visit_expr(expr_id);
            }
            StmtKind::Local(_, pat_id, expr_id) => {
                self.visit_expr(expr_id);
                self.bind_expr_to_pat(pat_id, expr_id);
            }
            StmtKind::Item(_) => {
                // Do nothing.
            }
        };
    }
}

fn get_spec_decl(spec_impl: &SpecImpl, functor_app: FunctorApp) -> &SpecDecl {
    if !functor_app.adjoint && functor_app.controlled == 0 {
        &spec_impl.body
    } else if functor_app.adjoint && functor_app.controlled == 0 {
        spec_impl
            .adj
            .as_ref()
            .expect("adjoint specialization does not exist")
    } else if !functor_app.adjoint && functor_app.controlled > 0 {
        spec_impl
            .ctl
            .as_ref()
            .expect("controlled specialization does not exist")
    } else {
        spec_impl
            .ctl_adj
            .as_ref()
            .expect("controlled adjoint specialization does not exits")
    }
}

fn map_eval_value_to_rir_operand(value: &Value) -> Operand {
    match value {
        Value::Bool(b) => Operand::Literal(Literal::Bool(*b)),
        Value::Double(d) => Operand::Literal(Literal::Double(*d)),
        Value::Int(i) => Operand::Literal(Literal::Integer(*i)),
        Value::Qubit(q) => Operand::Literal(Literal::Qubit(
            q.0.try_into().expect("could not convert qubit ID to u32"),
        )),
        Value::Result(r) => match r {
            val::Result::Id(id) => Operand::Literal(Literal::Result(
                (*id)
                    .try_into()
                    .expect("could not convert result ID to u32"),
            )),
            val::Result::Val(bool) => Operand::Literal(Literal::Bool(*bool)),
        },
        _ => panic!("{value} cannot be mapped to a RIR operand"),
    }
}

fn map_fir_type_to_rir_type(ty: &Ty) -> rir::Ty {
    let Ty::Prim(prim) = ty else {
        panic!("only some primitive types are supported");
    };

    match prim {
        Prim::BigInt
        | Prim::Pauli
        | Prim::Range
        | Prim::RangeFrom
        | Prim::RangeFull
        | Prim::RangeTo
        | Prim::String => panic!("{prim:?} is not a supported primitive type"),
        Prim::Bool => rir::Ty::Boolean,
        Prim::Double => rir::Ty::Double,
        Prim::Int => rir::Ty::Integer,
        Prim::Qubit => rir::Ty::Qubit,
        Prim::Result => rir::Ty::Result,
    }
}

fn mresetz_callable() -> Callable {
    Callable {
        name: "__quantum__qis__mresetz__body".to_string(),
        input_type: vec![rir::Ty::Qubit, rir::Ty::Result],
        output_type: None,
        body: None,
        call_type: CallableType::Measurement,
    }
}

fn mz_callable() -> Callable {
    Callable {
        name: "__quantum__qis__mz__body".to_string(),
        input_type: vec![rir::Ty::Qubit, rir::Ty::Result],
        output_type: None,
        body: None,
        call_type: CallableType::Measurement,
    }
}

fn read_result_callable() -> Callable {
    Callable {
        name: "__quantum__rt__read_result__body".to_string(),
        input_type: vec![rir::Ty::Result],
        output_type: Some(rir::Ty::Boolean),
        body: None,
        call_type: CallableType::Readout,
    }
}

fn resolve_call_arg_operands(args_value: Value) -> Vec<rir::Operand> {
    let mut operands = Vec::<rir::Operand>::new();
    if let Value::Tuple(elements) = args_value {
        for value in elements.iter() {
            let operand = map_eval_value_to_rir_operand(value);
            operands.push(operand);
        }
    } else {
        let operand = map_eval_value_to_rir_operand(&args_value);
        operands.push(operand);
    }

    operands
}