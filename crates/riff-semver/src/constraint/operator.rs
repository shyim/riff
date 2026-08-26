//! Operator types for version constraints

use std::{fmt, str::FromStr};
use thiserror::Error;

/// Comparison operators for version constraints
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operator {
    /// Equal (==)
    Equal,
    /// Less than (<)
    LessThan,
    /// Less than or equal (<=)
    LessThanOrEqual,
    /// Greater than (>)
    GreaterThan,
    /// Greater than or equal (>=)
    GreaterThanOrEqual,
    /// Not equal (!=)
    NotEqual,
}

#[derive(Error, Debug)]
#[error("Invalid operator: {0}")]
pub struct InvalidOperatorError(pub String);

impl Operator {
    /// Parse operator from string
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<Self, InvalidOperatorError> {
        s.parse()
    }

    /// Get the string representation of the operator
    pub fn as_str(&self) -> &'static str {
        match self {
            Operator::Equal => "==",
            Operator::LessThan => "<",
            Operator::LessThanOrEqual => "<=",
            Operator::GreaterThan => ">",
            Operator::GreaterThanOrEqual => ">=",
            Operator::NotEqual => "!=",
        }
    }

    /// Get all supported operators
    pub fn supported_operators() -> &'static [&'static str] {
        &["=", "==", "<", "<=", ">", ">=", "!=", "<>"]
    }
}

impl FromStr for Operator {
    type Err = InvalidOperatorError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "=" | "==" => Ok(Operator::Equal),
            "<" => Ok(Operator::LessThan),
            "<=" => Ok(Operator::LessThanOrEqual),
            ">" => Ok(Operator::GreaterThan),
            ">=" => Ok(Operator::GreaterThanOrEqual),
            "!=" | "<>" => Ok(Operator::NotEqual),
            _ => Err(InvalidOperatorError(s.to_string())),
        }
    }
}

impl fmt::Display for Operator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}
