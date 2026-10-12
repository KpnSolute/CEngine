//! CEngine expression language, draft v1. See `spec/expression-language.md`.
//!
//! Pure and total: no I/O, no clocks, no recursion beyond the parser depth limit.

mod eval;
mod parse;
mod value;

pub use eval::{eval, eval_with, Env, EvalError, NoResolver, Resolver};
pub use parse::{parse, Expr, Op, ParseError, MAX_DEPTH, MAX_LEN, MAX_NODES};
pub use value::Value;

/// Parse and evaluate with a resolver for `ref` and aggregates.
pub fn run_with(src: &str, env: &Env, r: &dyn Resolver) -> Result<Value, Error> {
    let expr = parse(src).map_err(Error::Parse)?;
    eval_with(&expr, env, r).map_err(Error::Eval)
}

/// Parse and evaluate in one step.
pub fn run(src: &str, env: &Env) -> Result<Value, Error> {
    let expr = parse(src).map_err(Error::Parse)?;
    eval(&expr, env).map_err(Error::Eval)
}

#[derive(Debug, PartialEq)]
pub enum Error {
    Parse(ParseError),
    Eval(EvalError),
}
