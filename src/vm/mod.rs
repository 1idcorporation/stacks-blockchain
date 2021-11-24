// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

extern crate regex;

pub mod diagnostic;
pub mod errors;

#[macro_use]
pub mod costs;

pub mod types;

pub mod contracts;

pub mod ast;
pub mod contexts;
pub mod database;
pub mod representations;

mod callables;
mod functions;
mod variables;

pub mod analysis;
pub mod docs;
pub mod version;

pub mod coverage;

#[cfg(test)]
pub mod tests;

use crate::clarity_vm::database::MemoryBackingStore;
use crate::core::StacksEpochId;
use vm::callables::CallableType;
use vm::contexts::GlobalContext;
pub use vm::contexts::{CallStack, ContractContext, Environment, LocalContext};
use vm::costs::{
    cost_functions, runtime_cost, CostOverflowingMath, CostTracker, LimitedCostTracker,
    MemoryConsumer,
};
use vm::errors::{
    CheckErrors, Error, InterpreterError, InterpreterResult as Result, RuntimeErrorType,
};
use vm::functions::define::DefineResult;
pub use vm::types::Value;
use vm::types::{PrincipalData, QualifiedContractIdentifier, TraitIdentifier, TypeSignature};

pub use vm::representations::{
    ClarityName, ContractName, SymbolicExpression, SymbolicExpressionType,
};

use std::convert::{TryFrom, TryInto};
pub use vm::contexts::MAX_CONTEXT_DEPTH;
use vm::costs::cost_functions::ClarityCostFunction;
pub use vm::functions::stx_transfer_consolidated;
pub use vm::version::ClarityVersion;

const MAX_CALL_STACK_DEPTH: usize = 64;

fn lookup_variable(name: &str, context: &LocalContext, env: &mut Environment) -> Result<Value> {
    if name.starts_with(char::is_numeric) || name.starts_with('\'') {
        Err(InterpreterError::BadSymbolicRepresentation(format!(
            "Unexpected variable name: {}",
            name
        ))
        .into())
    } else {
        if let Some(value) = variables::lookup_reserved_variable(name, context, env)? {
            Ok(value)
        } else {
            runtime_cost(
                ClarityCostFunction::LookupVariableDepth,
                env,
                context.depth(),
            )?;
            if let Some(value) = context
                .lookup_variable(name)
                .or_else(|| env.contract_context.lookup_variable(name))
            {
                runtime_cost(ClarityCostFunction::LookupVariableSize, env, value.size())?;
                Ok(value.clone())
            } else if let Some(value) = context.lookup_callable_contract(name) {
                let contract_identifier = &value.0;
                Ok(Value::Principal(PrincipalData::Contract(
                    contract_identifier.clone(),
                )))
            } else {
                Err(CheckErrors::UndefinedVariable(name.to_string()).into())
            }
        }
    }
}

pub fn lookup_function(name: &str, env: &mut Environment) -> Result<CallableType> {
    runtime_cost(ClarityCostFunction::LookupFunction, env, 0)?;

    if let Some(result) =
        functions::lookup_reserved_functions(name, env.contract_context.get_clarity_version())
    {
        Ok(result)
    } else {
        let user_function = env
            .contract_context
            .lookup_function(name)
            .ok_or(CheckErrors::UndefinedFunction(name.to_string()))?;
        Ok(CallableType::UserFunction(user_function))
    }
}

fn add_stack_trace(result: &mut Result<Value>, env: &Environment) {
    if let Err(Error::Runtime(_, ref mut stack_trace)) = result {
        if stack_trace.is_none() {
            stack_trace.replace(env.call_stack.make_stack_trace());
        }
    }
}

pub fn apply(
    function: &CallableType,
    args: &[SymbolicExpression],
    env: &mut Environment,
    context: &LocalContext,
) -> Result<Value> {
    let identifier = function.get_identifier();
    // Aaron: in non-debug executions, we shouldn't track a full call-stack.
    //        only enough to do recursion detection.

    // do recursion check on user functions.
    let track_recursion = match function {
        CallableType::UserFunction(_) => true,
        _ => false,
    };

    if track_recursion && env.call_stack.contains(&identifier) {
        return Err(CheckErrors::CircularReference(vec![identifier.to_string()]).into());
    }

    if env.call_stack.depth() >= MAX_CALL_STACK_DEPTH {
        return Err(RuntimeErrorType::MaxStackDepthReached.into());
    }

    if let CallableType::SpecialFunction(_, function) = function {
        env.call_stack.insert(&identifier, track_recursion);
        let mut resp = function(args, env, context);
        add_stack_trace(&mut resp, env);
        env.call_stack.remove(&identifier, track_recursion)?;
        resp
    } else {
        let mut used_memory = 0;
        let mut evaluated_args = vec![];
        env.call_stack.incr_apply_depth();
        for arg_x in args.iter() {
            let arg_value = match eval(arg_x, env, context) {
                Ok(x) => x,
                Err(e) => {
                    env.drop_memory(used_memory);
                    env.call_stack.decr_apply_depth();
                    return Err(e);
                }
            };
            let arg_use = arg_value.get_memory_use();
            match env.add_memory(arg_use) {
                Ok(_x) => {}
                Err(e) => {
                    env.drop_memory(used_memory);
                    env.call_stack.decr_apply_depth();
                    return Err(Error::from(e));
                }
            };
            used_memory += arg_value.get_memory_use();
            evaluated_args.push(arg_value);
        }
        env.call_stack.decr_apply_depth();

        env.call_stack.insert(&identifier, track_recursion);
        let mut resp = match function {
            CallableType::NativeFunction(_, function, cost_function) => {
                runtime_cost(*cost_function, env, evaluated_args.len())
                    .map_err(Error::from)
                    .and_then(|_| function.apply(evaluated_args))
            }
            CallableType::NativeFunction205(_, function, cost_function, cost_input_handle) => {
                let cost_input = if env.epoch() >= &StacksEpochId::Epoch2_05 {
                    cost_input_handle(evaluated_args.as_slice())?
                } else {
                    evaluated_args.len() as u64
                };
                runtime_cost(*cost_function, env, cost_input)
                    .map_err(Error::from)
                    .and_then(|_| function.apply(evaluated_args))
            }
            CallableType::UserFunction(function) => function.apply(&evaluated_args, env),
            _ => panic!("Should be unreachable."),
        };
        add_stack_trace(&mut resp, env);
        env.drop_memory(used_memory);
        env.call_stack.remove(&identifier, track_recursion)?;
        resp
    }
}

pub fn eval<'a>(
    exp: &SymbolicExpression,
    env: &'a mut Environment,
    context: &LocalContext,
) -> Result<Value> {
    use vm::representations::SymbolicExpressionType::{
        Atom, AtomValue, Field, List, LiteralValue, TraitReference,
    };

    if let Some(ref mut coverage_tracker) = env.global_context.coverage_reporting {
        coverage_tracker.report_eval(exp, &env.contract_context.contract_identifier);
    }

    match exp.expr {
        AtomValue(ref value) | LiteralValue(ref value) => Ok(value.clone()),
        Atom(ref value) => lookup_variable(&value, context, env),
        List(ref children) => {
            let (function_variable, rest) = children
                .split_first()
                .ok_or(CheckErrors::NonFunctionApplication)?;

            if let Some(ref mut coverage_tracker) = env.global_context.coverage_reporting {
                coverage_tracker.report_eval(
                    &function_variable,
                    &env.contract_context.contract_identifier,
                );
            }

            let function_name = function_variable
                .match_atom()
                .ok_or(CheckErrors::BadFunctionName)?;
            let f = lookup_function(&function_name, env)?;
            apply(&f, &rest, env, context)
        }
        TraitReference(_, _) | Field(_) => unreachable!("can't be evaluated"),
    }
}

pub fn is_reserved(name: &str, version: &ClarityVersion) -> bool {
    if let Some(_result) = functions::lookup_reserved_functions(name, version) {
        true
    } else if variables::is_reserved_name(name, version) {
        true
    } else {
        false
    }
}

/// This function evaluates a list of expressions, sharing a global context.
/// It returns the final evaluated result.
/// Used for the initialization of a new contract.
pub fn eval_all(
    expressions: &[SymbolicExpression],
    contract_context: &mut ContractContext,
    global_context: &mut GlobalContext,
    sponsor: Option<PrincipalData>,
) -> Result<Option<Value>> {
    let mut last_executed = None;
    let context = LocalContext::new();
    let mut total_memory_use = 0;

    let publisher: PrincipalData = contract_context.contract_identifier.issuer.clone().into();

    finally_drop_memory!(global_context, total_memory_use; {
        for exp in expressions {
            let try_define = global_context.execute(|context| {
                let mut call_stack = CallStack::new();
                let mut env = Environment::new(
                    context, contract_context, &mut call_stack, Some(publisher.clone()), Some(publisher.clone()), sponsor.clone());
                functions::define::evaluate_define(exp, &mut env)
            })?;
            match try_define {
                DefineResult::Variable(name, value) => {
                    runtime_cost(ClarityCostFunction::BindName, global_context, 0)?;
                    let value_memory_use = value.get_memory_use();
                    global_context.add_memory(value_memory_use)?;
                    total_memory_use += value_memory_use;

                    contract_context.variables.insert(name, value);
                },
                DefineResult::Function(name, value) => {
                    runtime_cost(ClarityCostFunction::BindName, global_context, 0)?;

                    contract_context.functions.insert(name, value);
                },
                DefineResult::PersistedVariable(name, value_type, value) => {
                    runtime_cost(ClarityCostFunction::CreateVar, global_context, value_type.size())?;
                    contract_context.persisted_names.insert(name.clone());

                    global_context.add_memory(value_type.type_size()
                                              .expect("type size should be realizable") as u64)?;

                    global_context.add_memory(value.size() as u64)?;

                    let data_type = global_context.database.create_variable(&contract_context.contract_identifier, &name, value_type);
                    global_context.database.set_variable(&contract_context.contract_identifier, &name, value, &data_type)?;

                    contract_context.meta_data_var.insert(name, data_type);
                },
                DefineResult::Map(name, key_type, value_type) => {
                    runtime_cost(ClarityCostFunction::CreateMap, global_context,
                                  u64::from(key_type.size()).cost_overflow_add(
                                      u64::from(value_type.size()))?)?;
                    contract_context.persisted_names.insert(name.clone());

                    global_context.add_memory(key_type.type_size()
                                              .expect("type size should be realizable") as u64)?;
                    global_context.add_memory(value_type.type_size()
                                              .expect("type size should be realizable") as u64)?;

                    let data_type = global_context.database.create_map(&contract_context.contract_identifier, &name, key_type, value_type);

                    contract_context.meta_data_map.insert(name, data_type);
                },
                DefineResult::FungibleToken(name, total_supply) => {
                    runtime_cost(ClarityCostFunction::CreateFt, global_context, 0)?;
                    contract_context.persisted_names.insert(name.clone());

                    global_context.add_memory(TypeSignature::UIntType.type_size()
                                              .expect("type size should be realizable") as u64)?;

                    let data_type = global_context.database.create_fungible_token(&contract_context.contract_identifier, &name, &total_supply);

                    contract_context.meta_ft.insert(name, data_type);
                },
                DefineResult::NonFungibleAsset(name, asset_type) => {
                    runtime_cost(ClarityCostFunction::CreateNft, global_context, asset_type.size())?;
                    contract_context.persisted_names.insert(name.clone());

                    global_context.add_memory(asset_type.type_size()
                                              .expect("type size should be realizable") as u64)?;

                    let data_type = global_context.database.create_non_fungible_token(&contract_context.contract_identifier, &name, &asset_type);

                    contract_context.meta_nft.insert(name, data_type);
                },
                DefineResult::Trait(name, trait_type) => {
                    contract_context.defined_traits.insert(name, trait_type);
                },
                DefineResult::UseTrait(_name, _trait_identifier) => {},
                DefineResult::ImplTrait(trait_identifier) => {
                    contract_context.implemented_traits.insert(trait_identifier);
                },
                DefineResult::NoDefine => {
                    // not a define function, evaluate normally.
                    global_context.execute(|global_context| {
                        let mut call_stack = CallStack::new();
                        let mut env = Environment::new(
                            global_context, contract_context, &mut call_stack, Some(publisher.clone()), Some(publisher.clone()), sponsor.clone());

                        let result = eval(exp, &mut env, &context)?;
                        last_executed = Some(result);
                        Ok(())
                    })?;
                }
            }
        }

        contract_context.data_size = total_memory_use;
        Ok(last_executed)
    })
}

/// Run provided program in a brand new environment, with a transient, empty
/// database. Only used for testing
/// This method executes the program in Epoch 2.0 *and* Epoch 2.05 and asserts
/// that the result is the same before returning the result
#[cfg(test)]
pub fn execute_on_network(program: &str, use_mainnet: bool) -> Result<Option<Value>> {
    let epoch_200_result = execute_in_epoch(program, StacksEpochId::Epoch20, use_mainnet);
    let epoch_205_result = execute_in_epoch(program, StacksEpochId::Epoch2_05, use_mainnet);
    assert_eq!(
        epoch_200_result, epoch_205_result,
        "Epoch 2.0 and 2.05 should have same execution result, but did not for program `{}`",
        program
    );
    epoch_205_result
}

/// Execute `program` on the `Testnet`.
#[cfg(test)]
pub fn execute(program: &str) -> Result<Option<Value>> {
    execute_on_network(program, false)
}

#[cfg(test)]
pub fn execute_in_epoch(
    program: &str,
    epoch: StacksEpochId,
    use_mainnet: bool,
) -> Result<Option<Value>> {
    let contract_id = QualifiedContractIdentifier::transient();
    let mut contract_context = ContractContext::new(contract_id.clone());
    let mut marf = MemoryBackingStore::new();
    let conn = marf.as_clarity_db();
    let mut global_context = GlobalContext::new(false, conn, LimitedCostTracker::new_free(), epoch);
    global_context.execute(|g| {
        let parsed = ast::build_ast(&contract_id, program, &mut ())?.expressions;
        eval_all(&parsed, &mut contract_context, g)
    })
}

/* Run provided program in a brand new environment, with a transient, empty
 *  database. Only used by CLI and unit tests.
 */
pub fn execute_against_version(program: &str, version: ClarityVersion) -> Result<Option<Value>> {
    execute_in_epoch(program, version, false)
}

/* Run provided program in a brand new environment, with a transient, empty
 *  database. Only used by CLI and unit tests.
 *
 * This version of the function assumes that the ClarityVersion is Clarity2.
 */
pub fn execute_v2(program: &str) -> Result<Option<Value>> {
    execute_against_version(program, ClarityVersion::Clarity2)
}

#[cfg(test)]
mod test {
    use crate::clarity_vm::database::MemoryBackingStore;
    use crate::core::StacksEpochId;
    use std::collections::HashMap;
    use vm::callables::{DefineType, DefinedFunction};
    use vm::costs::LimitedCostTracker;
    use vm::errors::RuntimeErrorType;
    use vm::eval;
    use vm::execute;
    use vm::types::{QualifiedContractIdentifier, TypeSignature};
    use vm::{
        CallStack, ContractContext, Environment, GlobalContext, LocalContext, SymbolicExpression,
        Value,
    };

    use super::ClarityVersion;

    #[test]
    fn test_simple_user_function() {
        //
        //  test program:
        //  (define (do_work x) (+ 5 x))
        //  (define a 59)
        //  (do_work a)
        //
        let content = [SymbolicExpression::list(Box::new([
            SymbolicExpression::atom("do_work".into()),
            SymbolicExpression::atom("a".into()),
        ]))];

        let func_body = SymbolicExpression::list(Box::new([
            SymbolicExpression::atom("+".into()),
            SymbolicExpression::atom_value(Value::Int(5)),
            SymbolicExpression::atom("x".into()),
        ]));

        let func_args = vec![("x".into(), TypeSignature::IntType)];
        let user_function = DefinedFunction::new(
            func_args,
            func_body,
            DefineType::Private,
            &"do_work".into(),
            &"",
        );

        let context = LocalContext::new();
        let mut contract_context = ContractContext::new(
            QualifiedContractIdentifier::transient(),
            ClarityVersion::Clarity1,
        );

        let mut marf = MemoryBackingStore::new();
        let mut global_context = GlobalContext::new(
            false,
            marf.as_clarity_db(),
            LimitedCostTracker::new_free(),
            StacksEpochId::Epoch2_05,
        );

        contract_context
            .variables
            .insert("a".into(), Value::Int(59));
        contract_context
            .functions
            .insert("do_work".into(), user_function);

        let mut call_stack = CallStack::new();
        let mut env = Environment::new(
            &mut global_context,
            &contract_context,
            &mut call_stack,
            None,
            None,
            None,
        );
        assert_eq!(Ok(Value::Int(64)), eval(&content[0], &mut env, &context));
    }
}
