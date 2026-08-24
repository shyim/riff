//! SAT-based dependency resolver for Composer packages.
//!
//! This module implements a CDCL (Conflict-Driven Clause Learning) SAT solver
//! specifically designed for package dependency resolution. The implementation
//! follows Composer's solver design.
//!
//! # Architecture
//!
//! The solver consists of several key components:
//!
//! - [`Pool`]: Registry of all available packages with lookup by name/constraint
//! - [`PoolOptimizer`]: Reduces pool size before solving for better performance
//! - [`Request`]: Specification of what needs to be resolved
//! - [`RuleSet`]: Collection of SAT clauses representing dependencies
//! - [`Solver`]: The main CDCL algorithm implementation
//!
//! # Algorithm Overview
//!
//! 1. **Pool Optimization** (optional, enabled by default): Reduce pool size by removing
//!    packages with identical dependencies and filtering impossible versions
//! 2. **Rule Generation**: Convert dependency graph to SAT clauses
//! 3. **Unit Propagation**: Force decisions from unit clauses
//! 4. **Decision Making**: Choose package versions using policy
//! 5. **Conflict Analysis**: Learn from conflicts to avoid repeating mistakes
//! 6. **Backtracking**: Revert to appropriate level on conflict
//!
//! # Example
//!
//! ```ignore
//! use composer_rs_core::solver::{Pool, Request, Solver, Policy};
//!
//! let pool = Pool::new();
//! // ... add packages to pool
//!
//! let request = Request::new();
//! // ... add requirements to request
//!
//! let policy = Policy::default();
//! let solver = Solver::new(&pool, &policy);
//!
//! match solver.solve(&request) {
//!     Ok(transaction) => println!("Solution found!"),
//!     Err(problems) => println!("No solution: {:?}", problems),
//! }
//!
//! // To disable pool optimization:
//! let solver = Solver::new(&pool, &policy).with_optimization(false);
//! ```

mod decisions;
mod policy;
mod pool;
mod pool_builder;
mod pool_optimizer;
mod problem;
mod request;
mod rule;
mod rule_generator;
mod rule_set;
mod solver;
mod transaction;
mod watch_graph;

#[cfg(test)]
mod tests;

pub use decisions::Decisions;
pub use policy::Policy;
pub use pool::{PackageId, Pool, PoolBuilder, PoolEntry};
pub use pool_builder::PoolBuilder as LazyPoolBuilder;
pub use pool_optimizer::PoolOptimizer;
pub use problem::Problem;
pub use request::Request;
pub use rule::{Literal, Rule, RuleType};
pub use rule_set::RuleSet;
pub use solver::{Solver, SolverResult};
pub use transaction::{Operation, Transaction};
