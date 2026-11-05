//! The vectorized execution engine: typed vectors, factorized chunks, compiled expressions,
//! a physical plan lowered from the logical plan, and the operators that run it.

pub mod boundary;
pub mod chunk;
pub mod ctx;
pub mod exec;
pub mod expr;
pub mod layout;
pub mod ops;
pub mod parallel;
pub mod plan;
pub mod values;
pub mod vector;
