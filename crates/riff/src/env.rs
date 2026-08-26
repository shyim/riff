use anyhow::{bail, Result};

pub(crate) fn composer_env_bool(name: &str) -> Result<bool> {
    Ok(parse_bool_env(name, std::env::var(name).ok().as_deref(), Some(false))?.unwrap_or(false))
}

fn parse_bool_env(name: &str, value: Option<&str>, default: Option<bool>) -> Result<Option<bool>> {
    let Some(value) = value else {
        return Ok(default);
    };
    match value {
        "1" | "true" | "on" => Ok(Some(true)),
        "0" | "false" | "off" => Ok(Some(false)),
        _ => bail!("Invalid value for {name}: expected 1, 0, true, false, on, or off"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composer_platform_bool_env_returns_default_when_unset() {
        for default in [Some(false), Some(true), None] {
            assert_eq!(
                parse_bool_env("COMPOSER_TEST_BOOL_ENV", None, default).unwrap(),
                default
            );
        }
    }

    #[test]
    fn composer_platform_bool_env_returns_expected_values() {
        for (value, expected) in [
            ("true", true),
            ("false", false),
            ("1", true),
            ("0", false),
            ("on", true),
            ("off", false),
        ] {
            assert_eq!(
                parse_bool_env("COMPOSER_TEST_BOOL_ENV", Some(value), None).unwrap(),
                Some(expected)
            );
        }
    }

    #[test]
    fn composer_platform_bool_env_rejects_invalid_values() {
        for value in ["2", "-1", "abc", " 1 "] {
            let error = parse_bool_env("COMPOSER_TEST_BOOL_ENV", Some(value), None)
                .unwrap_err()
                .to_string();
            assert!(error.contains("Invalid value for COMPOSER_TEST_BOOL_ENV"));
        }
    }
}
