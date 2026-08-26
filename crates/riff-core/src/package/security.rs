use thiserror::Error;

use super::Package;
use crate::is_platform_package;

/// A dependency package contains metadata that is unsafe to pass to downloaders
/// or binary installers.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PackageMetadataError {
    #[error("Invalid package found during dependency resolution: {0}")]
    InvalidName(String),

    #[error("{package} has an invalid {field}")]
    UnsafeArgument { package: String, field: String },

    #[error("{package} has an invalid bin {binary}, it must not contain \"..\" path segments")]
    UnsafeBinary { package: String, binary: String },
}

/// Validate untrusted package metadata before it reaches a process or the
/// filesystem. Platform packages do not carry downloadable metadata and are
/// accepted as-is, matching Composer's dependency package validation boundary.
pub fn validate_package_metadata(package: &Package) -> Result<(), PackageMetadataError> {
    if is_platform_package(&package.name) {
        return Ok(());
    }

    if !is_safe_package_name(&package.name) {
        return Err(PackageMetadataError::InvalidName(package.name.clone()));
    }

    if let Some(source) = &package.source {
        validate_argument(&package.name, "source.url", &source.url)?;
        validate_argument(&package.name, "source.reference", &source.reference)?;
    }
    if let Some(dist) = &package.dist {
        validate_argument(&package.name, "dist.url", &dist.url)?;
        if let Some(reference) = &dist.reference {
            validate_argument(&package.name, "dist.reference", reference)?;
        }
    }
    for binary in &package.bin {
        if binary.split(['/', '\\']).any(|component| component == "..") {
            return Err(PackageMetadataError::UnsafeBinary {
                package: package.name.clone(),
                binary: binary.to_string(),
            });
        }
    }

    Ok(())
}

fn validate_argument(package: &str, field: &str, value: &str) -> Result<(), PackageMetadataError> {
    if value.trim_start().starts_with('-') {
        return Err(PackageMetadataError::UnsafeArgument {
            package: package.to_string(),
            field: field.to_string(),
        });
    }
    Ok(())
}

fn is_safe_package_name(name: &str) -> bool {
    let Some((vendor, package)) = name.split_once('/') else {
        return false;
    };
    !vendor.is_empty()
        && !package.is_empty()
        && !name.starts_with('-')
        && !name.chars().any(char::is_whitespace)
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-/".contains(&byte)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{Dist, Source};

    // Ported from Composer\Test\Package\Loader\ValidatingArrayLoaderTest::
    // testValidatePackageAllowsValidPackages.
    #[test]
    fn composer_validating_array_loader_allows_valid_packages() {
        let mut package = Package::new("vendor/package", "1.0.0.0");
        package.source = Some(Source::git(
            "https://example.org/vendor/package.git",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ));
        package.dist = Some(
            Dist::zip("https://example.org/vendor/package.zip")
                .with_reference("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        );
        package.bin = ["bin/foo", "console", "some.bin"]
            .into_iter()
            .map(Into::into)
            .collect();

        assert_eq!(validate_package_metadata(&package), Ok(()));
        assert_eq!(
            validate_package_metadata(&Package::new("php", "8.2.0.0")),
            Ok(())
        );
    }

    // Ported from Composer\Test\Package\Loader\ValidatingArrayLoaderTest::
    // testValidatePackageRejectsMaliciousMetadata.
    #[test]
    fn composer_validating_array_loader_rejects_malicious_metadata() {
        let mut cases = Vec::new();
        cases.push(Package::new("--evil/pkg", "1.0.0.0"));

        let mut source_url = Package::new("vendor/pkg", "1.0.0.0");
        source_url.source = Some(Source::git("--upload-pack=touch /tmp/pwned", "main"));
        cases.push(source_url);

        let mut source_reference = Package::new("vendor/pkg", "1.0.0.0");
        source_reference.source = Some(Source::git(
            "https://example.org/vendor/pkg.git",
            "--upload-pack=touch /tmp/pwned",
        ));
        cases.push(source_reference);

        let mut dist_url = Package::new("vendor/pkg", "1.0.0.0");
        dist_url.dist = Some(Dist::zip("-oProxyCommand=touch /tmp/pwned"));
        cases.push(dist_url);

        let mut dist_reference = Package::new("vendor/pkg", "1.0.0.0");
        dist_reference.dist =
            Some(Dist::zip("https://example.org/vendor/pkg.zip").with_reference("--evil"));
        cases.push(dist_reference);

        let mut binary = Package::new("vendor/pkg", "1.0.0.0");
        binary.bin = ["bin/ok", "../../../../escape-target.txt"]
            .into_iter()
            .map(Into::into)
            .collect();
        cases.push(binary);

        for package in cases {
            assert!(
                validate_package_metadata(&package).is_err(),
                "expected malicious metadata to fail for {package:?}"
            );
        }
    }
}
