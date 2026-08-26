//! Constraint types for version matching

mod bound;
#[allow(clippy::module_inception)]
pub mod constraint;
mod constraint_interface;
mod match_all;
mod match_none;
mod multi_constraint;
mod operator;

pub use bound::Bound;
pub use constraint::{php_version_compare, Constraint, ConstraintError, NormalizedVersion};
pub use constraint_interface::ConstraintInterface;
pub use match_all::MatchAllConstraint;
pub use match_none::MatchNoneConstraint;
pub use multi_constraint::{MultiConstraint, MultiConstraintError};
pub use operator::Operator;
