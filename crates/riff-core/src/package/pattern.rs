/// Match a Composer package name against a case-insensitive `*` glob.
pub fn package_name_matches(pattern: &str, package: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let package = package.to_ascii_lowercase();
    let (mut pattern_index, mut package_index) = (0usize, 0usize);
    let (mut wildcard, mut retry) = (None, 0usize);
    let pattern = pattern.as_bytes();
    let package = package.as_bytes();
    while package_index < package.len() {
        if pattern.get(pattern_index) == package.get(package_index) {
            pattern_index += 1;
            package_index += 1;
        } else if pattern.get(pattern_index) == Some(&b'*') {
            wildcard = Some(pattern_index);
            pattern_index += 1;
            retry = package_index;
        } else if let Some(index) = wildcard {
            pattern_index = index + 1;
            retry += 1;
            package_index = retry;
        } else {
            return false;
        }
    }
    pattern[pattern_index..].iter().all(|byte| *byte == b'*')
}

/// Build Composer's regular-expression source for a group of package globs.
pub fn package_names_to_regex(package_names: &[&str], wrap: &str) -> String {
    let patterns = package_names
        .iter()
        .map(|name| regex::escape(name).replace(r"\*", ".*"))
        .collect::<Vec<_>>()
        .join("|");
    wrap.replacen("%s", &patterns, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from Composer\Test\Package\BasePackageTest::testPackageNamesToRegexp.
    #[test]
    fn composer_package_names_expand_to_wrapped_regular_expressions() {
        for (names, wrap, expected) in [
            (
                &["ext-*", "monolog/monolog"][..],
                "{^%s$}i",
                r"{^ext\-.*|monolog/monolog$}i",
            ),
            (&["php"][..], "{^%s$}i", r"{^php$}i"),
            (&["*"][..], "{^%s$}i", r"{^.*$}i"),
            (&["foo", "bar"][..], "§%s§", r"§foo|bar§"),
        ] {
            assert_eq!(package_names_to_regex(names, wrap), expected);
        }

        assert!(package_name_matches("ext-*", "EXT-JSON"));
        assert!(package_name_matches("monolog/monolog", "Monolog/Monolog"));
    }
}
