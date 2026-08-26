//! Version parsing and normalization module

use lazy_static::lazy_static;
use regex::Regex;
use thiserror::Error;

use crate::constraint::{
    Constraint, ConstraintInterface, MatchAllConstraint, MultiConstraint, NormalizedVersion,
    Operator,
};

/// Stability levels for versions
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stability {
    Dev,
    Alpha,
    Beta,
    RC,
    Stable,
}

impl Stability {
    pub fn as_str(&self) -> &'static str {
        match self {
            Stability::Dev => "dev",
            Stability::Alpha => "alpha",
            Stability::Beta => "beta",
            Stability::RC => "RC",
            Stability::Stable => "stable",
        }
    }
}

impl std::fmt::Display for Stability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Error type for version parsing
#[derive(Error, Debug, Clone)]
pub enum VersionParserError {
    #[error("Invalid version string \"{0}\"")]
    InvalidVersion(String),
    #[error("Invalid version string \"{version}\"{extra}")]
    InvalidVersionWithContext { version: String, extra: String },
    #[error("Invalid operator \"{0}\"")]
    InvalidOperator(String),
    #[error("Invalid stability \"{0}\"")]
    InvalidStability(String),
    #[error("Could not parse version constraint {constraint}: {reason}")]
    ConstraintParseError { constraint: String, reason: String },
    #[error("{0}")]
    ConstraintError(String),
    #[error("{0}")]
    MultiConstraintError(String),
}

impl From<crate::constraint::ConstraintError> for VersionParserError {
    fn from(err: crate::constraint::ConstraintError) -> Self {
        VersionParserError::ConstraintError(err.to_string())
    }
}

impl From<crate::constraint::MultiConstraintError> for VersionParserError {
    fn from(err: crate::constraint::MultiConstraintError) -> Self {
        VersionParserError::MultiConstraintError(err.to_string())
    }
}

lazy_static! {
    /// Regex to match pre-release data (Note: Rust regex doesn't support possessive quantifiers ++, so we use +)
    static ref MODIFIER_REGEX: &'static str = r"[._-]?(?:(stable|beta|b|RC|alpha|a|patch|pl|p)((?:[.-]?\d+)*)?)?([.-]?dev)?";

    static ref STABILITIES_REGEX: &'static str = r"stable|RC|beta|alpha|dev";

    // Classical versioning regex
    static ref CLASSICAL_VERSION_RE: Regex = Regex::new(&format!(
        r"(?i)^v?(\d{{1,5}})(\.\d+)?(\.\d+)?(\.\d+)?{}$",
        *MODIFIER_REGEX
    )).unwrap();

    // Date-based versioning regex
    static ref DATE_VERSION_RE: Regex = Regex::new(&format!(
        r"(?i)^v?(\d{{4}}(?:[.:-]?\d{{2}}){{1,6}}(?:[.:-]?\d{{1,3}}){{0,2}}){}$",
        *MODIFIER_REGEX
    )).unwrap();

    // Stability parse regex
    static ref STABILITY_PARSE_RE: Regex = Regex::new(&format!(
        r"(?i){}(?:\+.*)?$",
        *MODIFIER_REGEX
    )).unwrap();

    // Constraint regexes
    static ref WILDCARD_RE: Regex = Regex::new(r"(?i)^(v)?[xX*](\.[xX*])*$").unwrap();
    static ref X_RANGE_RE: Regex = Regex::new(r"^v?(\d++)(?:\.(\d++))?(?:\.(\d++))?(?:\.[xX*])++$").unwrap();

    // Numeric alias prefix regex
    static ref NUMERIC_ALIAS_RE: Regex = Regex::new(r"(?i)^(?P<version>(\d++\.)*\d++)(?:\.x)?-dev$").unwrap();

    // Stability flag in constraint
    static ref CONSTRAINT_STABILITY_RE: Regex = Regex::new(&format!(r"(?i)^([^,\s]*?)@({})$", *STABILITIES_REGEX)).unwrap();

    // Reference on dev version
    static ref CONSTRAINT_REF_RE: Regex = Regex::new(r"(?i)^(dev-[^,\s@]+?|[^,\s@]+?\.x-dev)#.+$").unwrap();

    // Version regex for complex patterns (used by tilde, caret, hyphen)
    static ref VERSION_REGEX: String = format!(
        r"v?(\d+)(?:\.(\d+))?(?:\.(\d+))?(?:\.(\d+))?(?:{}|\.([xX*][.-]?dev))(?:\+[^\s]+)?",
        *MODIFIER_REGEX
    );

    // Hyphen Range
    static ref HYPHEN_RE: Regex = Regex::new(&format!(r"(?i)^(?P<from>{}) +- +(?P<to>{})($)$", *VERSION_REGEX, *VERSION_REGEX)).unwrap();

}

fn fast_normalize_simple(version: &str) -> Option<String> {
    let bytes = version.as_bytes();
    if bytes.is_empty() {
        return None;
    }

    let plus_pos = bytes.iter().position(|&b| b == b'+');
    let end = plus_pos.unwrap_or(bytes.len());

    if end == 0 {
        return None;
    }

    for &b in &bytes[..end] {
        if b.is_ascii_whitespace()
            || b == b'#'
            || b == b'@'
            || b == b'/'
            || b == b'*'
            || b == b'x'
            || b == b'X'
        {
            return None;
        }
    }

    if let Some(pos) = plus_pos {
        let meta = &bytes[pos + 1..];
        if meta.is_empty() {
            return None;
        }
        for &b in meta {
            if b.is_ascii_whitespace() || b == b',' || b == b'+' {
                return None;
            }
        }
    }

    let slice = &version[..end];
    let slice_bytes = slice.as_bytes();

    let mut index = 0;
    if slice_bytes[0] == b'v' || slice_bytes[0] == b'V' {
        index = 1;
        if index >= slice_bytes.len() {
            return None;
        }
    }

    let mut parts: Vec<(usize, usize)> = Vec::with_capacity(4);
    let mut part_start = index;
    let mut pos = index;

    while pos < slice_bytes.len() {
        let b = slice_bytes[pos];
        if b == b'.' {
            if part_start == pos {
                return None;
            }
            parts.push((part_start, pos));
            if parts.len() > 4 {
                return None;
            }
            part_start = pos + 1;
            pos += 1;
            continue;
        }
        if !b.is_ascii_digit() {
            break;
        }
        pos += 1;
    }

    if part_start == pos {
        return None;
    }
    parts.push((part_start, pos));
    if parts.len() > 4 {
        return None;
    }
    if let Some((start, end)) = parts.first() {
        if end - start > 5 {
            return None;
        }
    }

    let mut rest = &slice[pos..];
    let mut dev_suffix = false;
    let mut stability = None;
    let mut stability_raw = "";
    let mut stability_digits = "";

    if !rest.is_empty() {
        let mut leading_sep = None;
        if let Some(&first) = rest.as_bytes().first() {
            if first == b'.' || first == b'-' || first == b'_' {
                leading_sep = Some(first);
                rest = &rest[1..];
            }
        }

        if rest.is_empty() {
            return None;
        }

        if ci_starts_with(rest, "dev") {
            if rest.len() != 3 {
                return None;
            }
            if leading_sep == Some(b'_') {
                return None;
            }
            dev_suffix = true;
        } else if let Some((kind, len)) = match_stability(rest) {
            stability = Some(kind);
            stability_raw = &rest[..len];
            rest = &rest[len..];

            let mut digits_rest = rest;
            while digits_rest.starts_with('.') || digits_rest.starts_with('-') {
                digits_rest = &digits_rest[1..];
            }

            let mut i = 0;
            let bytes = digits_rest.as_bytes();
            let mut saw_digit = false;
            while i < bytes.len() {
                let b = bytes[i];
                if b.is_ascii_digit() {
                    saw_digit = true;
                    i += 1;
                    continue;
                }
                if b == b'.' || b == b'-' {
                    if !saw_digit {
                        return None;
                    }
                    if i + 1 >= bytes.len() || !bytes[i + 1].is_ascii_digit() {
                        break;
                    }
                    i += 1;
                    continue;
                }
                break;
            }

            stability_digits = &digits_rest[..i];
            rest = &digits_rest[i..];

            if !rest.is_empty() {
                let mut tail = rest;
                if let Some(&first) = tail.as_bytes().first() {
                    if first == b'.' || first == b'-' {
                        tail = &tail[1..];
                    } else if first == b'_' {
                        return None;
                    }
                }

                if ci_starts_with(tail, "dev") && tail.len() == 3 {
                    dev_suffix = true;
                } else {
                    return None;
                }
            }
        } else {
            return None;
        }
    }

    let mut normalized = build_simple_numeric(slice, &parts);

    if let Some(kind) = stability {
        if kind == StabilityKind::Stable && stability_raw == "stable" {
            return Some(normalized);
        }

        normalized.push('-');
        normalized.push_str(stability_name(kind));
        if !stability_digits.is_empty() {
            normalized.push_str(stability_digits);
        }
    }

    if dev_suffix {
        normalized.push_str("-dev");
    }

    Some(normalized)
}

fn build_simple_numeric(version: &str, parts: &[(usize, usize)]) -> String {
    let mut total_len = 3 + (4 - parts.len());
    for (start, end) in parts {
        total_len += end - start;
    }

    let mut normalized = String::with_capacity(total_len);
    for i in 0..4 {
        if i > 0 {
            normalized.push('.');
        }
        if let Some((start, end)) = parts.get(i) {
            normalized.push_str(&version[*start..*end]);
        } else {
            normalized.push('0');
        }
    }
    normalized
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StabilityKind {
    Stable,
    Alpha,
    Beta,
    RC,
    Patch,
}

fn match_stability(s: &str) -> Option<(StabilityKind, usize)> {
    if ci_starts_with(s, "stable") {
        return Some((StabilityKind::Stable, 6));
    }
    if ci_starts_with(s, "alpha") {
        return Some((StabilityKind::Alpha, 5));
    }
    if ci_starts_with(s, "beta") {
        return Some((StabilityKind::Beta, 4));
    }
    if ci_starts_with(s, "patch") {
        return Some((StabilityKind::Patch, 5));
    }
    if ci_starts_with(s, "rc") {
        return Some((StabilityKind::RC, 2));
    }
    if ci_starts_with(s, "pl") {
        return Some((StabilityKind::Patch, 2));
    }
    if ci_starts_with(s, "a") {
        return Some((StabilityKind::Alpha, 1));
    }
    if ci_starts_with(s, "b") {
        return Some((StabilityKind::Beta, 1));
    }
    if ci_starts_with(s, "p") {
        return Some((StabilityKind::Patch, 1));
    }
    None
}

fn stability_name(kind: StabilityKind) -> &'static str {
    match kind {
        StabilityKind::Stable => "stable",
        StabilityKind::Alpha => "alpha",
        StabilityKind::Beta => "beta",
        StabilityKind::RC => "RC",
        StabilityKind::Patch => "patch",
    }
}

fn ci_starts_with(s: &str, prefix: &str) -> bool {
    let s_bytes = s.as_bytes();
    let p_bytes = prefix.as_bytes();
    if s_bytes.len() < p_bytes.len() {
        return false;
    }
    for i in 0..p_bytes.len() {
        if !s_bytes[i].eq_ignore_ascii_case(&p_bytes[i]) {
            return false;
        }
    }
    true
}

fn strip_stability_flag(version: &str) -> &str {
    let Some(at) = version.rfind('@') else {
        return version;
    };
    let stability = &version[at + 1..];
    if ["stable", "rc", "beta", "alpha", "dev"]
        .iter()
        .any(|candidate| stability.eq_ignore_ascii_case(candidate))
    {
        &version[..at]
    } else {
        version
    }
}

fn strip_alias(version: &str) -> Option<&str> {
    let bytes = version.as_bytes();
    let left_end = bytes.iter().position(|byte| *byte == b' ')?;
    if left_end == 0 {
        return None;
    }

    let mut cursor = left_end;
    while bytes.get(cursor) == Some(&b' ') {
        cursor += 1;
    }
    if bytes.get(cursor..cursor + 2) != Some(b"as") {
        return None;
    }
    cursor += 2;

    let spaces_start = cursor;
    while bytes.get(cursor) == Some(&b' ') {
        cursor += 1;
    }
    if cursor == spaces_start || cursor == bytes.len() {
        return None;
    }

    let left = &version[..left_end];
    let right = &version[cursor..];
    if is_alias_token(left) && is_alias_token(right) {
        Some(left)
    } else {
        None
    }
}

fn is_alias_token(token: &str) -> bool {
    !token.is_empty()
        && !token
            .chars()
            .any(|character| character == ',' || character.is_whitespace())
}

fn strip_build_metadata(version: &str) -> Option<&str> {
    let plus = version.find('+')?;
    let core = &version[..plus];
    let metadata = &version[plus + 1..];
    if core.is_empty()
        || metadata.is_empty()
        || core
            .chars()
            .any(|character| character == ',' || character.is_whitespace())
        || metadata.chars().any(char::is_whitespace)
    {
        return None;
    }
    Some(core)
}

fn split_or_constraints(constraints: &str) -> Vec<&str> {
    let mut groups = Vec::new();
    let mut start = 0;

    while let Some(relative_pipe) = constraints[start..].find('|') {
        let pipe = start + relative_pipe;
        groups.push(constraints[start..pipe].trim_end());

        let mut after = pipe + 1;
        if constraints.as_bytes().get(after) == Some(&b'|') {
            after += 1;
        }
        let remainder = &constraints[after..];
        start = after + (remainder.len() - remainder.trim_start().len());
    }

    groups.push(&constraints[start..]);
    groups
}

fn strip_dev_suffix(version: &str) -> Option<&str> {
    let suffix_start = version.len().checked_sub(3)?;
    if !version.as_bytes()[suffix_start..].eq_ignore_ascii_case(b"dev") {
        return None;
    }

    let branch = &version[..suffix_start];
    Some(
        branch
            .strip_suffix('-')
            .or_else(|| branch.strip_suffix('.'))
            .unwrap_or(branch),
    )
}

fn normalize_numeric_branch(name: &str) -> Option<String> {
    let name = name.trim();
    let name = name
        .strip_prefix('v')
        .or_else(|| name.strip_prefix('V'))
        .unwrap_or(name);
    if name.is_empty() {
        return None;
    }

    let mut normalized_parts = ["9999999"; 4];
    for (part_count, part) in name.split('.').enumerate() {
        if part_count == normalized_parts.len() || part.is_empty() {
            return None;
        }

        let is_numeric = part.bytes().all(|byte| byte.is_ascii_digit());
        let is_wildcard = part.len() == 1 && matches!(part.as_bytes()[0], b'x' | b'X' | b'*');
        if (!is_numeric && !is_wildcard) || (part_count == 0 && is_wildcard) {
            return None;
        }

        normalized_parts[part_count] = if is_wildcard { "9999999" } else { part };
    }

    Some(format!(
        "{}.{}.{}.{}-dev",
        normalized_parts[0], normalized_parts[1], normalized_parts[2], normalized_parts[3]
    ))
}

#[derive(Debug, PartialEq, Eq)]
struct RangeVersion<'a> {
    parts: [Option<&'a str>; 4],
    part_count: usize,
    has_explicit_stability: bool,
    wildcard_dev: bool,
}

fn parse_range_version(version: &str) -> Option<RangeVersion<'_>> {
    let core = if let Some(plus) = version.find('+') {
        let metadata = &version[plus + 1..];
        if metadata.is_empty() || metadata.chars().any(char::is_whitespace) {
            return None;
        }
        &version[..plus]
    } else {
        version
    };

    let bytes = core.as_bytes();
    let mut cursor = usize::from(matches!(bytes.first(), Some(b'v' | b'V')));
    let mut parts = [None; 4];
    let mut part_count = 0;

    loop {
        let start = cursor;
        while bytes.get(cursor).is_some_and(|byte| byte.is_ascii_digit()) {
            cursor += 1;
        }
        if cursor == start || part_count == parts.len() {
            return None;
        }
        parts[part_count] = Some(&core[start..cursor]);
        part_count += 1;

        if part_count == parts.len()
            || bytes.get(cursor) != Some(&b'.')
            || !bytes
                .get(cursor + 1)
                .is_some_and(|byte| byte.is_ascii_digit())
        {
            break;
        }
        cursor += 1;
    }

    let modifier = &core[cursor..];
    if is_range_wildcard_dev(modifier) {
        return Some(RangeVersion {
            parts,
            part_count,
            has_explicit_stability: true,
            wildcard_dev: true,
        });
    }

    let mut rest = modifier;
    if rest
        .as_bytes()
        .first()
        .is_some_and(|byte| matches!(byte, b'.' | b'_' | b'-'))
    {
        rest = &rest[1..];
    }

    let mut has_explicit_stability = false;
    if let Some((_, stability_len)) = match_stability(rest) {
        has_explicit_stability = true;
        rest = &rest[stability_len..];

        loop {
            let before_number = rest;
            if rest.starts_with('.') || rest.starts_with('-') {
                rest = &rest[1..];
            }
            let digit_count = rest
                .as_bytes()
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if digit_count == 0 {
                rest = before_number;
                break;
            }
            rest = &rest[digit_count..];
        }
    }

    if !rest.is_empty() {
        if rest.starts_with('.') || rest.starts_with('-') {
            rest = &rest[1..];
        }
        if rest.len() != 3 || !rest.as_bytes().eq_ignore_ascii_case(b"dev") {
            return None;
        }
        has_explicit_stability = true;
    }

    Some(RangeVersion {
        parts,
        part_count,
        has_explicit_stability,
        wildcard_dev: false,
    })
}

fn is_range_wildcard_dev(modifier: &str) -> bool {
    let Some(rest) = modifier.strip_prefix('.') else {
        return false;
    };
    let Some(&wildcard) = rest.as_bytes().first() else {
        return false;
    };
    if !matches!(wildcard, b'x' | b'X' | b'*') {
        return false;
    }

    let rest = &rest[1..];
    let rest = rest
        .strip_prefix('.')
        .or_else(|| rest.strip_prefix('-'))
        .unwrap_or(rest);
    rest.len() == 3 && rest.as_bytes().eq_ignore_ascii_case(b"dev")
}

/// Version parser for normalizing and parsing version strings
pub struct VersionParser;

impl VersionParser {
    /// Composer's normalized alias for the default development branch.
    pub const DEFAULT_BRANCH_ALIAS: &'static str = "9999999-dev";

    /// Create a new version parser
    pub fn new() -> Self {
        VersionParser
    }

    /// Check if a version string is valid
    pub fn is_valid(&self, version: &str) -> bool {
        self.normalize(version).is_ok()
    }

    /// Parse command-line package names followed by optional constraints.
    pub fn parse_name_version_pairs(
        &self,
        pairs: &[impl AsRef<str>],
    ) -> Vec<(String, Option<String>)> {
        let mut result = Vec::new();
        let mut index = 0;
        while index < pairs.len() {
            let value = pairs[index].as_ref().trim();
            if let Some(separator) = value.find(['=', ':', ' ']) {
                let (name, version) = value.split_at(separator);
                result.push((name.to_string(), Some(version[1..].to_string())));
                index += 1;
                continue;
            }

            let constraint = pairs.get(index + 1).map(AsRef::as_ref).filter(|candidate| {
                !candidate.contains('/')
                    && !wildcard_looks_like_package(candidate)
                    && !is_platform_package_name(candidate)
            });
            if let Some(constraint) = constraint {
                result.push((value.to_string(), Some(constraint.to_string())));
                index += 2;
            } else {
                result.push((value.to_string(), None));
                index += 1;
            }
        }
        result
    }

    /// Determine whether moving between two normalized versions is an upgrade.
    pub fn is_upgrade(from: &str, to: &str) -> bool {
        if from == to {
            return true;
        }
        let from = normalize_default_branch_for_upgrade(from);
        let to = normalize_default_branch_for_upgrade(to);
        if from.starts_with("dev-") || to.starts_with("dev-") {
            return true;
        }
        crate::Comparator::less_than(from, to)
    }

    /// Returns the stability of a version
    pub fn parse_stability(version: &str) -> Stability {
        // Strip off any hash reference
        let version = if let Some(pos) = version.find('#') {
            &version[..pos]
        } else {
            version
        };

        // Check for dev- prefix or -dev suffix
        if version.starts_with("dev-") || version.ends_with("-dev") {
            return Stability::Dev;
        }

        let version_lower = version.to_lowercase();
        if let Some(caps) = STABILITY_PARSE_RE.captures(&version_lower) {
            // Check for -dev suffix in modifier
            if caps.get(3).is_some_and(|m| !m.as_str().is_empty()) {
                return Stability::Dev;
            }

            if let Some(modifier) = caps.get(1) {
                let m = modifier.as_str();
                if m == "beta" || m == "b" {
                    return Stability::Beta;
                }
                if m == "alpha" || m == "a" {
                    return Stability::Alpha;
                }
                if m == "rc" {
                    return Stability::RC;
                }
            }
        }

        Stability::Stable
    }

    /// Normalize a stability string
    pub fn normalize_stability(stability: &str) -> Result<Stability, VersionParserError> {
        let stability_lower = stability.to_lowercase();
        match stability_lower.as_str() {
            "stable" => Ok(Stability::Stable),
            "rc" => Ok(Stability::RC),
            "beta" => Ok(Stability::Beta),
            "alpha" => Ok(Stability::Alpha),
            "dev" => Ok(Stability::Dev),
            _ => Err(VersionParserError::InvalidStability(stability.to_string())),
        }
    }

    /// Normalizes a version string to be able to perform comparisons on it
    pub fn normalize(&self, version: &str) -> Result<String, VersionParserError> {
        self.normalize_with_context(version, None)
    }

    /// Normalizes a version string with optional full version context for error messages
    pub fn normalize_with_context(
        &self,
        version: &str,
        full_version: Option<&str>,
    ) -> Result<String, VersionParserError> {
        let version = version.trim();
        let orig_version = version;
        let full_version = full_version.unwrap_or(version);

        if version.is_empty() {
            return Err(VersionParserError::InvalidVersion(orig_version.to_string()));
        }

        if let Some(normalized) = fast_normalize_simple(version) {
            return Ok(normalized);
        }

        // Strip off aliasing
        let version = strip_alias(version).unwrap_or(version);

        // Strip off stability flag
        let version = strip_stability_flag(version);

        // Normalize master/trunk/default branches
        if version == "master" || version == "trunk" || version == "default" {
            return Ok(format!("dev-{}", version));
        }

        // If requirement is branch-like, use full name
        if version
            .as_bytes()
            .get(..4)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"dev-"))
        {
            return Ok(format!("dev-{}", &version[4..]));
        }

        // Strip off build metadata
        let version = strip_build_metadata(version).unwrap_or(version);

        // Numeric wildcard branches are common in root aliases. Handle them
        // before the general version regexes so one `x-dev` does not compile
        // the classical, date, and dev-branch automata for the whole process.
        if let Some(branch) = strip_dev_suffix(version) {
            let has_wildcard = branch
                .as_bytes()
                .iter()
                .any(|byte| matches!(byte, b'*' | b'x' | b'X'));
            if has_wildcard {
                if let Some(normalized) = normalize_numeric_branch(branch) {
                    return Ok(normalized);
                }
            }
        }

        // Match classical versioning
        if let Some(caps) = CLASSICAL_VERSION_RE.captures(version) {
            let mut result = caps.get(1).unwrap().as_str().to_string();
            result.push_str(caps.get(2).map_or(".0", |m| m.as_str()));
            result.push_str(caps.get(3).map_or(".0", |m| m.as_str()));
            result.push_str(caps.get(4).map_or(".0", |m| m.as_str()));

            let index = 5;
            return self.add_version_modifiers(&caps, &mut result, index, orig_version);
        }

        // Match date(time) based versioning
        if let Some(caps) = DATE_VERSION_RE.captures(version) {
            let date_part = caps.get(1).unwrap().as_str();
            // Replace non-digits with dots
            let mut result: String = date_part
                .chars()
                .map(|c| if c.is_ascii_digit() { c } else { '.' })
                .collect();

            let index = 2;
            return self.add_version_modifiers(&caps, &mut result, index, orig_version);
        }

        // A branch ending with -dev is only valid if it is numeric.
        if let Some(branch) = strip_dev_suffix(version) {
            if let Some(normalized) = normalize_numeric_branch(branch) {
                return Ok(normalized);
            }
        }

        // Build error message
        let extra = self.build_alias_error_message(orig_version, full_version);
        Err(VersionParserError::InvalidVersionWithContext {
            version: orig_version.to_string(),
            extra,
        })
    }

    fn add_version_modifiers(
        &self,
        caps: &regex::Captures,
        result: &mut String,
        index: usize,
        _orig_version: &str,
    ) -> Result<String, VersionParserError> {
        if let Some(modifier) = caps.get(index) {
            let modifier_str = modifier.as_str();
            if modifier_str == "stable" {
                return Ok(result.clone());
            }

            let expanded = self.expand_stability(modifier_str);
            result.push('-');
            result.push_str(&expanded);

            if let Some(num) = caps.get(index + 1) {
                let num_str = num.as_str();
                // Remove leading dots and dashes
                let num_str = num_str.trim_start_matches(['.', '-']);
                result.push_str(num_str);
            }
        }

        if let Some(dev) = caps.get(index + 2) {
            if !dev.as_str().is_empty() {
                result.push_str("-dev");
            }
        }

        Ok(result.clone())
    }

    fn build_alias_error_message(&self, orig_version: &str, full_version: &str) -> String {
        // Check for alias issues
        let alias_pattern = format!(
            r" +as +{}(?:@(?:{}))?$",
            regex::escape(orig_version),
            *STABILITIES_REGEX
        );
        if let Ok(re) = Regex::new(&alias_pattern) {
            if re.is_match(full_version) {
                return format!(
                    " in \"{}\", the alias must be an exact version",
                    full_version
                );
            }
        }

        let aliasee_pattern = format!(
            r"^{}(?:@(?:{}))? +as +",
            regex::escape(orig_version),
            *STABILITIES_REGEX
        );
        if let Ok(re) = Regex::new(&aliasee_pattern) {
            if re.is_match(full_version) {
                return format!(
                    " in \"{}\", the alias source must be an exact version, if it is a branch name you should prefix it with dev-",
                    full_version
                );
            }
        }

        String::new()
    }

    /// Extract numeric prefix from alias
    pub fn parse_numeric_alias_prefix(&self, branch: &str) -> Option<String> {
        if let Some(caps) = NUMERIC_ALIAS_RE.captures(branch) {
            if let Some(version) = caps.name("version") {
                return Some(format!("{}.", version.as_str()));
            }
        }
        None
    }

    /// Normalizes a branch name
    pub fn normalize_branch(&self, name: &str) -> Result<String, VersionParserError> {
        let name = name.trim();

        if let Some(normalized) = normalize_numeric_branch(name) {
            return Ok(normalized);
        }

        Ok(format!("dev-{}", name))
    }

    /// Normalizes a default branch name (master/default/trunk) to 9999999-dev
    pub fn normalize_default_branch(&self, name: &str) -> String {
        if name == "dev-master" || name == "dev-default" || name == "dev-trunk" {
            "9999999-dev".to_string()
        } else {
            name.to_string()
        }
    }

    /// Expand shorthand stability strings
    fn expand_stability(&self, stability: &str) -> String {
        match stability.to_lowercase().as_str() {
            "a" => "alpha".to_string(),
            "b" => "beta".to_string(),
            "p" | "pl" => "patch".to_string(),
            "rc" => "RC".to_string(),
            other => other.to_string(),
        }
    }

    /// Parse a constraint string into constraint objects
    pub fn parse_constraints(
        &self,
        constraints: &str,
    ) -> Result<Box<dyn ConstraintInterface>, VersionParserError> {
        let pretty_constraint = constraints.to_string();
        let constraints = constraints.trim();

        if constraints.is_empty() {
            return Err(VersionParserError::InvalidVersion(String::new()));
        }

        // Most constraints have no disjunction. Avoid initializing and running the
        // regex splitter for the common case.
        let or_constraints: Vec<&str> = if constraints.as_bytes().contains(&b'|') {
            split_or_constraints(constraints)
        } else {
            vec![constraints]
        };

        // Check for leading/trailing operators
        if or_constraints.first().is_some_and(|s| s.is_empty()) {
            return Err(VersionParserError::ConstraintParseError {
                constraint: constraints.to_string(),
                reason: "leading operator".to_string(),
            });
        }
        if or_constraints.last().is_some_and(|s| s.is_empty()) {
            return Err(VersionParserError::ConstraintParseError {
                constraint: constraints.to_string(),
                reason: "trailing operator".to_string(),
            });
        }

        let mut or_groups: Vec<Box<dyn ConstraintInterface>> = Vec::new();

        for or_constraint in or_constraints {
            // Split by AND (, or space) - manually handle since Rust regex doesn't support look-behind
            let and_constraints = self.split_and_constraints(or_constraint);

            let constraint_objects: Vec<Box<dyn ConstraintInterface>> = if and_constraints.len() > 1
            {
                let mut objects: Vec<Box<dyn ConstraintInterface>> = Vec::new();
                for and_constraint in and_constraints {
                    let parsed = self.parse_constraint(and_constraint)?;
                    objects.extend(parsed);
                }
                objects
            } else {
                self.parse_constraint(and_constraints[0])?
            };

            let constraint: Box<dyn ConstraintInterface> = if constraint_objects.len() == 1 {
                constraint_objects.into_iter().next().unwrap()
            } else {
                Box::new(MultiConstraint::new(constraint_objects, true)?)
            };

            or_groups.push(constraint);
        }

        let mut parsed_constraint = MultiConstraint::create(or_groups, false)?;
        parsed_constraint.set_pretty_string(Some(pretty_constraint));

        Ok(parsed_constraint)
    }

    /// Split constraint string by AND operators (comma or space)
    fn split_and_constraints<'a>(&self, input: &'a str) -> Vec<&'a str> {
        let mut parts = Vec::new();
        let mut current_start = 0;
        let chars: Vec<char> = input.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            let c = chars[i];

            // Check for separator (comma or space)
            if c == ',' || c == ' ' {
                // Skip if at start
                if i == 0 {
                    current_start = i + 1;
                    i += 1;
                    continue;
                }

                // Check previous character - don't split after operator or special char
                let prev = chars[i - 1];
                if prev == '>'
                    || prev == '<'
                    || prev == '='
                    || prev == '!'
                    || prev == '-'
                    || prev == ','
                {
                    i += 1;
                    continue;
                }

                // Check if before this separator we only have operators (no version yet)
                // This handles cases like ">=  1.0.0" where there are multiple spaces after operator
                let before = input[current_start..i].trim();
                if before.is_empty()
                    || before.chars().all(|c| {
                        c == '>' || c == '<' || c == '=' || c == '!' || c == '^' || c == '~'
                    })
                {
                    i += 1;
                    continue;
                }

                // Check next non-space character for hyphen range (don't split before " - ")
                let mut next_non_space = i + 1;
                while next_non_space < chars.len() && chars[next_non_space] == ' ' {
                    next_non_space += 1;
                }
                if next_non_space < chars.len() {
                    let next = chars[next_non_space];
                    // Only skip for hyphen range (space-dash-space pattern like "1.0 - 2.0")
                    // Don't skip for operators like <, >, =, ! - those indicate a new constraint
                    if next == '-' {
                        // Check if this is really a hyphen range (dash followed by space)
                        if next_non_space + 1 < chars.len() && chars[next_non_space + 1] == ' ' {
                            i += 1;
                            continue;
                        }
                    }
                }

                // Check if this is within an "as" alias expression
                if before.ends_with(" as") || before == "as" {
                    i += 1;
                    continue;
                }

                // Skip consecutive separators
                while i < chars.len() && (chars[i] == ',' || chars[i] == ' ') {
                    i += 1;
                }

                // Check for "as" after separator
                if i + 2 < chars.len() && input[i..].starts_with("as ") {
                    continue;
                }

                // Add the part - find the actual end of the previous part
                let mut end = i;
                while end > current_start && (chars[end - 1] == ',' || chars[end - 1] == ' ') {
                    end -= 1;
                }
                if end > current_start {
                    let part = input[current_start..end].trim();
                    if !part.is_empty() && part != "," {
                        parts.push(part);
                    }
                }
                current_start = i;
            }

            i += 1;
        }

        // Add remaining part
        if current_start < input.len() {
            let remaining = input[current_start..].trim();
            if !remaining.is_empty() {
                parts.push(remaining);
            }
        }

        if parts.is_empty() {
            parts.push(input.trim());
        }

        parts
    }

    fn parse_constraint(
        &self,
        constraint: &str,
    ) -> Result<Vec<Box<dyn ConstraintInterface>>, VersionParserError> {
        let constraint = constraint.trim();

        // Strip off aliasing
        let constraint = if constraint.contains(" as ") {
            strip_alias(constraint).unwrap_or(constraint)
        } else {
            constraint
        };

        // Strip @stability flags
        let (constraint, _stability_modifier) = if constraint.as_bytes().contains(&b'@') {
            if let Some(caps) = CONSTRAINT_STABILITY_RE.captures(constraint) {
                let c = caps.get(1).map_or("*", |m| {
                    if m.as_str().is_empty() {
                        "*"
                    } else {
                        m.as_str()
                    }
                });
                let s = caps.get(2).map(|m| m.as_str());
                (c, s)
            } else {
                (constraint, None)
            }
        } else {
            (constraint, None)
        };

        // Strip #refs
        let constraint = if constraint.as_bytes().contains(&b'#') {
            if let Some(caps) = CONSTRAINT_REF_RE.captures(constraint) {
                caps.get(1).unwrap().as_str()
            } else {
                constraint
            }
        } else {
            constraint
        };

        let has_wildcard = constraint
            .as_bytes()
            .iter()
            .any(|byte| matches!(byte, b'*' | b'x' | b'X'));

        // Match any wildcard
        if has_wildcard && WILDCARD_RE.is_match(constraint) {
            let has_v = constraint.starts_with('v') || constraint.starts_with('V');
            let has_dots = constraint.contains('.');
            if has_v || has_dots {
                return Ok(vec![Box::new(Constraint::new(
                    Operator::GreaterThanOrEqual,
                    "0.0.0.0-dev".to_string(),
                )?)]);
            }
            return Ok(vec![Box::new(MatchAllConstraint::new())]);
        }

        // Tilde Range
        if constraint.starts_with('~') {
            let version = constraint
                .strip_prefix("~>")
                .or_else(|| constraint.strip_prefix('~'))
                .expect("tilde prefix was checked above");
            let Some(range) = parse_range_version(version) else {
                return Err(VersionParserError::ConstraintParseError {
                    constraint: constraint.to_string(),
                    reason: "Could not parse tilde constraint".to_string(),
                });
            };
            if constraint.starts_with("~>") {
                return Err(VersionParserError::ConstraintParseError {
                    constraint: constraint.to_string(),
                    reason: "Invalid operator \"~>\", you probably meant to use the \"~\" operator"
                        .to_string(),
                });
            }
            return self.parse_tilde_constraint(&range, constraint);
        }

        // Caret Range
        if constraint.starts_with('^') {
            let Some(range) = constraint.strip_prefix('^').and_then(parse_range_version) else {
                return Err(VersionParserError::ConstraintParseError {
                    constraint: constraint.to_string(),
                    reason: "Could not parse caret constraint".to_string(),
                });
            };
            return self.parse_caret_constraint(&range, constraint);
        }

        // X Range
        if has_wildcard {
            if let Some(caps) = X_RANGE_RE.captures(constraint) {
                return self.parse_x_range_constraint(&caps);
            }
        }

        // Hyphen Range
        if constraint.contains(" - ") {
            if let Some(caps) = HYPHEN_RE.captures(constraint) {
                return self.parse_hyphen_constraint(&caps);
            }
        }

        // Basic comparators are simple enough to parse without a regex.
        let operator_len = if constraint.starts_with("<>")
            || constraint.starts_with("!=")
            || constraint.starts_with(">=")
            || constraint.starts_with("<=")
            || constraint.starts_with("==")
        {
            2
        } else if constraint.starts_with('>')
            || constraint.starts_with('<')
            || constraint.starts_with('=')
        {
            1
        } else {
            0
        };
        let operator = if operator_len == 0 {
            "="
        } else {
            &constraint[..operator_len]
        };
        let version_str = constraint[operator_len..].trim();

        if version_str.is_empty() {
            return Err(VersionParserError::ConstraintParseError {
                constraint: constraint.to_string(),
                reason: "empty version".to_string(),
            });
        }

        // Try to normalize the version
        let version = match self.normalize(version_str) {
            Ok(v) => v,
            Err(_) => {
                // Try to recover from invalid constraint like foobar-dev
                if version_str.ends_with("-dev")
                    && version_str
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '-' || c == '.' || c == '/')
                {
                    let dev_name = &version_str[..version_str.len() - 4];
                    self.normalize(&format!("dev-{}", dev_name))?
                } else {
                    return Err(VersionParserError::ConstraintParseError {
                        constraint: constraint.to_string(),
                        reason: format!("Invalid version \"{}\"", version_str),
                    });
                }
            }
        };

        let op = match operator {
            "=" | "==" => Operator::Equal,
            "!=" | "<>" => Operator::NotEqual,
            ">" => Operator::GreaterThan,
            ">=" => Operator::GreaterThanOrEqual,
            "<" => Operator::LessThan,
            "<=" => Operator::LessThanOrEqual,
            _ => return Err(VersionParserError::InvalidOperator(operator.to_string())),
        };

        // Append -dev for < and >= operators on stable versions
        let version = if (op == Operator::LessThan || op == Operator::GreaterThanOrEqual)
            && !version.contains('-')
            && !version.starts_with("dev-")
        {
            format!("{}-dev", version)
        } else {
            version
        };

        Ok(vec![Box::new(Constraint::new(op, version)?)])
    }

    fn parse_tilde_constraint(
        &self,
        range: &RangeVersion<'_>,
        constraint: &str,
    ) -> Result<Vec<Box<dyn ConstraintInterface>>, VersionParserError> {
        let position = range.part_count + usize::from(range.wildcard_dev);

        // Calculate stability suffix
        let stability_suffix = if range.has_explicit_stability {
            ""
        } else {
            "-dev"
        };

        let constraint_without_tilde = &constraint[1..];
        let low_version =
            self.normalize(&format!("{}{}", constraint_without_tilde, stability_suffix))?;
        let lower_bound = Constraint::new(Operator::GreaterThanOrEqual, low_version)?;

        let high_position = std::cmp::max(1, position - 1);
        let parts = range.parts.map(|part| part.unwrap_or("0"));
        let high_version = format!(
            "{}-dev",
            self.manipulate_version_array(&parts, high_position, 1)?
        );
        let upper_bound = Constraint::new(Operator::LessThan, high_version)?;

        Ok(vec![Box::new(lower_bound), Box::new(upper_bound)])
    }

    fn parse_caret_constraint(
        &self,
        range: &RangeVersion<'_>,
        constraint: &str,
    ) -> Result<Vec<Box<dyn ConstraintInterface>>, VersionParserError> {
        // Determine position based on leading zeros
        let position = if range.parts[0].unwrap_or("") != "0" || range.parts[1].is_none() {
            1
        } else if range.parts[1].unwrap_or("") != "0" || range.parts[2].is_none() {
            2
        } else {
            3
        };

        // Calculate stability suffix
        let stability_suffix = if range.has_explicit_stability {
            ""
        } else {
            "-dev"
        };

        let constraint_without_caret = &constraint[1..];
        let low_version =
            self.normalize(&format!("{}{}", constraint_without_caret, stability_suffix))?;
        let lower_bound = Constraint::new(Operator::GreaterThanOrEqual, low_version)?;

        let parts = range.parts.map(|part| part.unwrap_or("0"));
        let high_version = format!(
            "{}-dev",
            self.manipulate_version_array(&parts, position, 1)?
        );
        let upper_bound = Constraint::new(Operator::LessThan, high_version)?;

        Ok(vec![Box::new(lower_bound), Box::new(upper_bound)])
    }

    fn parse_x_range_constraint(
        &self,
        caps: &regex::Captures,
    ) -> Result<Vec<Box<dyn ConstraintInterface>>, VersionParserError> {
        let position = if caps.get(3).is_some_and(|m| !m.as_str().is_empty()) {
            3
        } else if caps.get(2).is_some_and(|m| !m.as_str().is_empty()) {
            2
        } else {
            1
        };

        let low_version = format!("{}-dev", self.manipulate_version_string(caps, position, 0)?);
        let high_version = format!("{}-dev", self.manipulate_version_string(caps, position, 1)?);

        if low_version == "0.0.0.0-dev" {
            return Ok(vec![Box::new(Constraint::new(
                Operator::LessThan,
                high_version,
            )?)]);
        }

        Ok(vec![
            Box::new(Constraint::new(Operator::GreaterThanOrEqual, low_version)?),
            Box::new(Constraint::new(Operator::LessThan, high_version)?),
        ])
    }

    fn parse_hyphen_constraint(
        &self,
        caps: &regex::Captures,
    ) -> Result<Vec<Box<dyn ConstraintInterface>>, VersionParserError> {
        let from = caps.name("from").map(|m| m.as_str()).unwrap_or("");
        let to = caps.name("to").map(|m| m.as_str()).unwrap_or("");

        // Calculate low stability suffix
        let low_stability_suffix =
            if caps.get(6).is_none() && caps.get(8).is_none() && caps.get(9).is_none() {
                "-dev"
            } else {
                ""
            };

        let low_version = self.normalize(from)?;
        let lower_bound = Constraint::new(
            Operator::GreaterThanOrEqual,
            format!("{}{}", low_version, low_stability_suffix),
        )?;

        // For upper bound, check if we have a complete version
        let has_patch = caps.get(12).is_some_and(|m| !m.as_str().is_empty())
            && caps.get(13).is_some_and(|m| !m.as_str().is_empty());
        let has_stability =
            caps.get(15).is_some() || caps.get(17).is_some() || caps.get(18).is_some();

        let upper_bound = if has_patch || has_stability {
            let high_version = self.normalize(to)?;
            Constraint::new(Operator::LessThanOrEqual, high_version)?
        } else {
            // Validate to version first
            let _ = self.normalize(to)?;

            // Increment the version for upper bound
            let position = if caps.get(12).is_none_or(|m| m.as_str().is_empty()) {
                1
            } else {
                2
            };

            // Build matches array for manipulate_version_string
            let v1 = caps.get(11).map_or("", |m| m.as_str());
            let v2 = caps.get(12).map_or("", |m| m.as_str());
            let v3 = caps.get(13).map_or("", |m| m.as_str());
            let v4 = caps.get(14).map_or("", |m| m.as_str());

            let high_version = format!(
                "{}-dev",
                self.manipulate_version_array(&[v1, v2, v3, v4], position, 1)?
            );
            Constraint::new(Operator::LessThan, high_version)?
        };

        Ok(vec![Box::new(lower_bound), Box::new(upper_bound)])
    }

    fn manipulate_version_string(
        &self,
        caps: &regex::Captures,
        position: usize,
        increment: i32,
    ) -> Result<String, VersionParserError> {
        let v1 = caps.get(1).map_or("0", |m| m.as_str());
        let v2 = caps.get(2).map_or("0", |m| m.as_str());
        let v3 = caps.get(3).map_or("0", |m| m.as_str());
        let v4 = caps.get(4).map_or("0", |m| m.as_str());

        self.manipulate_version_array(&[v1, v2, v3, v4], position, increment)
    }

    fn manipulate_version_array(
        &self,
        matches: &[&str],
        position: usize,
        increment: i32,
    ) -> Result<String, VersionParserError> {
        let mut parts: Vec<i64> = vec![0; 4];

        for (i, &s) in matches.iter().enumerate().take(4) {
            parts[i] = s.parse().unwrap_or(0);
        }

        for i in (0..4).rev() {
            if i + 1 > position {
                parts[i] = 0;
            } else if i + 1 == position && increment != 0 {
                parts[i] += increment as i64;
                if parts[i] < 0 {
                    parts[i] = 0;
                    if i == 0 {
                        return Err(VersionParserError::InvalidVersion(
                            "carry overflow".to_string(),
                        ));
                    }
                }
            }
        }

        Ok(format!(
            "{}.{}.{}.{}",
            parts[0], parts[1], parts[2], parts[3]
        ))
    }

    /// Parse constraints and return a reusable, pre-parsed representation.
    pub fn parse_constraints_cached(
        &self,
        constraints: &str,
    ) -> Result<ParsedConstraints, VersionParserError> {
        let parsed = self.parse_constraints(constraints)?;
        Ok(ParsedConstraints {
            constraints: parsed,
        })
    }
}

fn wildcard_looks_like_package(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.iter().enumerate().any(|(index, byte)| {
        if *byte != b'*' {
            return false;
        }
        index
            .checked_sub(1)
            .and_then(|previous| bytes.get(previous))
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'/' | b'-'))
            || bytes.get(index + 1).is_some_and(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'/' | b'-')
            })
    })
}

fn is_platform_package_name(name: &str) -> bool {
    matches!(
        name,
        "php"
            | "php-64bit"
            | "php-ipv6"
            | "php-zts"
            | "php-debug"
            | "hhvm"
            | "composer"
            | "composer-runtime-api"
            | "composer-plugin-api"
    ) || name.starts_with("ext-")
        || name.starts_with("lib-")
}

fn normalize_default_branch_for_upgrade(version: &str) -> &str {
    if matches!(version, "dev-master" | "dev-trunk" | "dev-default") {
        VersionParser::DEFAULT_BRANCH_ALIAS
    } else {
        version
    }
}

/// Reusable, pre-parsed constraints for repeated checks.
pub struct ParsedConstraints {
    constraints: Box<dyn ConstraintInterface>,
}

impl ParsedConstraints {
    /// Return whether parsing simplified the constraint to an impossible set.
    pub fn is_match_none(&self) -> bool {
        if self.constraints.is_match_none() {
            return true;
        }
        let lower = self.constraints.lower_bound();
        let upper = self.constraints.upper_bound();
        lower.compare_to(&upper, ">")
            || (lower.version() == upper.version()
                && (!lower.is_inclusive() || !upper.is_inclusive()))
    }

    /// Check a normalized version string against the parsed constraints.
    pub fn matches_normalized(&self, normalized_version: &str) -> bool {
        self.constraints
            .matches_prepared_version(&NormalizedVersion::new(normalized_version.to_owned()))
    }

    /// Normalize the version and check against the parsed constraints.
    pub fn satisfies(&self, version: &str) -> bool {
        let parser = VersionParser::new();
        let normalized = match parser.normalize(version) {
            Ok(v) => v,
            Err(_) => return false,
        };
        self.matches_normalized(&normalized)
    }

    /// Check whether another constraint has at least one version in common.
    pub fn intersects(&self, other: &str) -> Result<bool, VersionParserError> {
        let parser = VersionParser::new();
        let other = parser.parse_constraints(other)?;
        Ok(self.constraints.matches(other.as_ref()))
    }
}

impl Default for VersionParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composer_version_parser_name_version_pairs_data_provider() {
        type ExpectedPair<'a> = (&'a str, Option<&'a str>);
        type Case<'a> = (&'a [&'a str], &'a [ExpectedPair<'a>]);

        let parser = VersionParser::new();
        let cases: &[Case<'_>] = &[
            (&["php:^7.0"], &[("php", Some("^7.0"))]),
            (&["php", "^7.0"], &[("php", Some("^7.0"))]),
            (&["php", "ext-apcu"], &[("php", None), ("ext-apcu", None)]),
            (
                &["foo/*", "bar*", "acme/baz", "*@dev"],
                &[("foo/*", None), ("bar*", None), ("acme/baz", Some("*@dev"))],
            ),
            (&["php", "*"], &[("php", Some("*"))]),
        ];

        for (input, expected) in cases {
            let expected: Vec<_> = expected
                .iter()
                .map(|(name, version)| {
                    (
                        (*name).to_string(),
                        version.map(std::string::ToString::to_string),
                    )
                })
                .collect();
            assert_eq!(parser.parse_name_version_pairs(input), expected);
        }
    }

    #[test]
    fn composer_version_parser_is_upgrade_data_provider() {
        for (from, to, expected) in [
            ("0.9.0.0", "1.0.0.0", true),
            ("1.0.0.0", "0.9.0.0", false),
            ("1.0.0.0", VersionParser::DEFAULT_BRANCH_ALIAS, true),
            (
                VersionParser::DEFAULT_BRANCH_ALIAS,
                VersionParser::DEFAULT_BRANCH_ALIAS,
                true,
            ),
            (VersionParser::DEFAULT_BRANCH_ALIAS, "1.0.0.0", false),
            ("1.0.0.0", "dev-foo", true),
            ("dev-foo", "dev-foo", true),
            ("dev-foo", "1.0.0.0", true),
        ] {
            assert_eq!(VersionParser::is_upgrade(from, to), expected);
        }
    }

    #[test]
    fn direct_normalized_version_matching_matches_equality_provider() {
        let parser = VersionParser::new();
        let constraints = [
            "*",
            "1.2.3",
            "= 1.2.3",
            "!= 1.2.3",
            "<> 2.0",
            "> 1.0",
            ">= 1.2",
            "< 3.0",
            "<= 2.4",
            "^1.2.3",
            "~1.2.3",
            "1.2.*",
            "1.0 - 2.0",
            ">=1.0 <2.0",
            "^1.0 || ^3.0",
            "dev-main",
            "!= dev-main",
            "dev-main || ^2.0",
        ];
        let versions = [
            "0.0.0.0",
            "0.9.9.0",
            "1.0.0.0",
            "1.2.3.0",
            "1.9.9.0",
            "2.0.0.0",
            "2.4.0.0",
            "3.0.0.0",
            "9999999-dev",
            "dev-main",
            "dev-feature",
        ];

        for constraint in constraints {
            let parsed = parser.parse_constraints(constraint).unwrap();
            for version in versions {
                let provider = Constraint::new(Operator::Equal, version.to_string()).unwrap();
                assert_eq!(
                    parsed.matches_normalized_version(version),
                    parsed.matches(&provider),
                    "direct match differs for constraint {constraint:?} and version {version:?}"
                );
            }
        }
    }

    #[test]
    fn test_split_or_constraints_matches_composer_separators() {
        assert_eq!(split_or_constraints("^1|^2"), vec!["^1", "^2"]);
        assert_eq!(split_or_constraints("^1 || ^2"), vec!["^1", "^2"]);
        assert_eq!(
            split_or_constraints("^1\u{2003}||\u{2003}^2"),
            vec!["^1", "^2"]
        );
        assert_eq!(split_or_constraints("^1|||^2"), vec!["^1", "", "^2"]);
        assert_eq!(split_or_constraints("||^1"), vec!["", "^1"]);
        assert_eq!(split_or_constraints("^1||"), vec!["^1", ""]);
    }

    #[test]
    fn test_strip_stability_flag_matches_known_suffixes_only() {
        assert_eq!(strip_stability_flag("1.0@dev"), "1.0");
        assert_eq!(strip_stability_flag("DEV-main@STABLE"), "DEV-main");
        assert_eq!(strip_stability_flag("1.0@Rc"), "1.0");
        assert_eq!(strip_stability_flag("1.0@preview"), "1.0@preview");
        assert_eq!(strip_stability_flag("1.0@dev+meta"), "1.0@dev+meta");
    }

    #[test]
    fn alias_scanner_matches_previous_regex() {
        let previous = Regex::new(r"^([^,\s]+) +as +([^,\s]+)$").unwrap();
        let left_tokens = ["1.0", "dev-main", "a,b", "a\tb", "a\u{2003}b", "", "+"];
        let right_tokens = ["2.0", "^2", "x,y", "x\ny", "x\u{00a0}y", "", "+"];
        let left_spaces = [" ", "  ", "", "\t", " \t"];
        let right_spaces = [" ", "   ", "", "\t", " \t"];

        for left in left_tokens {
            for left_space in left_spaces {
                for right_space in right_spaces {
                    for right in right_tokens {
                        let alias = format!("{left}{left_space}as{right_space}{right}");
                        let expected = previous
                            .captures(&alias)
                            .map(|captures| captures.get(1).unwrap().as_str());
                        assert_eq!(
                            strip_alias(&alias),
                            expected,
                            "alias handling differs for {alias:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn build_metadata_scanner_matches_previous_regex() {
        let previous = Regex::new(r"^([^,\s+]+)\+[^\s]+$").unwrap();
        let cores = ["1.0.0", "dev-main", "a,b", "a b", "a\u{2003}b", "", "v1"];
        let metadata = ["build", "build.1", "a+b", "a,b", "a b", "a\tb", "", "+"];

        for core in cores {
            for metadata in metadata {
                let version = format!("{core}+{metadata}");
                let expected = previous
                    .captures(&version)
                    .map(|captures| captures.get(1).unwrap().as_str());
                assert_eq!(
                    strip_build_metadata(&version),
                    expected,
                    "metadata handling differs for {version:?}"
                );
            }
        }

        for version in ["1.0.0", "dev-main", "+", "", "1.0.0\u{00a0}"] {
            assert_eq!(strip_build_metadata(version), None);
        }
    }

    #[test]
    fn test_range_version_scanner_matches_previous_regex_captures() {
        let previous = Regex::new(&format!(r"(?i)^{}$", *VERSION_REGEX)).unwrap();
        let prefixes = [
            "1",
            "v1",
            "V0.2",
            "0.0.3",
            "1.2.3.4",
            "01.002.0003",
            "123456",
            "1.2.3.4.5",
            "v",
            "",
        ];
        let suffixes = [
            "",
            "-stable",
            ".BETA2",
            "_b-1.2",
            "RC1",
            "-alpha.3-dev",
            "-patch-2",
            "-pl2",
            "-p.4",
            "-dev",
            "--dev",
            ".x-dev",
            ".X.dev",
            ".*-DEV",
            "+build.1",
            "-beta2+build.1",
            "-",
            "_",
            ".",
            "-bogus",
            ".x",
            ".x-beta",
            "+",
            "+bad metadata",
        ];

        for prefix in prefixes {
            for suffix in suffixes {
                let version = format!("{prefix}{suffix}");
                let captures = previous.captures(&version);
                let parsed = parse_range_version(&version);
                assert_eq!(
                    parsed.is_some(),
                    captures.is_some(),
                    "acceptance differs for {version:?}"
                );

                if let (Some(parsed), Some(captures)) = (parsed, captures) {
                    let expected_parts =
                        std::array::from_fn(|index| captures.get(index + 1).map(|m| m.as_str()));
                    assert_eq!(parsed.parts, expected_parts, "parts differ for {version:?}");
                    assert_eq!(
                        parsed.part_count,
                        expected_parts.iter().flatten().count(),
                        "part count differs for {version:?}"
                    );
                    assert_eq!(
                        parsed.has_explicit_stability,
                        captures.get(5).is_some()
                            || captures.get(7).is_some()
                            || captures.get(8).is_some(),
                        "stability differs for {version:?}"
                    );
                    assert_eq!(
                        parsed.wildcard_dev,
                        captures.get(8).is_some(),
                        "wildcard differs for {version:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn test_parse_stability() {
        // Full test suite from PHP
        assert_eq!(VersionParser::parse_stability("1"), Stability::Stable);
        assert_eq!(VersionParser::parse_stability("1.0"), Stability::Stable);
        assert_eq!(VersionParser::parse_stability("3.2.1"), Stability::Stable);
        assert_eq!(VersionParser::parse_stability("v3.2.1"), Stability::Stable);
        assert_eq!(VersionParser::parse_stability("v2.0.x-dev"), Stability::Dev);
        assert_eq!(
            VersionParser::parse_stability("v2.0.x-dev#abc123"),
            Stability::Dev
        );
        assert_eq!(
            VersionParser::parse_stability("v2.0.x-dev#trunk/@123"),
            Stability::Dev
        );
        assert_eq!(VersionParser::parse_stability("3.0-RC2"), Stability::RC);
        assert_eq!(VersionParser::parse_stability("dev-master"), Stability::Dev);
        assert_eq!(VersionParser::parse_stability("3.1.2-dev"), Stability::Dev);
        assert_eq!(
            VersionParser::parse_stability("dev-feature+issue-1"),
            Stability::Dev
        );
        assert_eq!(
            VersionParser::parse_stability("3.1.2-p1"),
            Stability::Stable
        );
        assert_eq!(
            VersionParser::parse_stability("3.1.2-pl2"),
            Stability::Stable
        );
        assert_eq!(
            VersionParser::parse_stability("3.1.2-patch"),
            Stability::Stable
        );
        assert_eq!(
            VersionParser::parse_stability("3.1.2-alpha5"),
            Stability::Alpha
        );
        assert_eq!(
            VersionParser::parse_stability("3.1.2-beta"),
            Stability::Beta
        );
        assert_eq!(VersionParser::parse_stability("2.0B1"), Stability::Beta);
        assert_eq!(VersionParser::parse_stability("1.2.0a1"), Stability::Alpha);
        assert_eq!(VersionParser::parse_stability("1.2_a1"), Stability::Alpha);
        assert_eq!(VersionParser::parse_stability("2.0.0rc1"), Stability::RC);
        assert_eq!(
            VersionParser::parse_stability("1.0.0-alpha11+cs-1.1.0"),
            Stability::Alpha
        );
    }

    #[test]
    fn test_normalize_versions() {
        let parser = VersionParser::new();

        // Basic versions
        assert_eq!(parser.normalize("1.0.0").unwrap(), "1.0.0.0");
        assert_eq!(parser.normalize("1.2.3.4").unwrap(), "1.2.3.4");
        assert_eq!(parser.normalize("1.0.0RC1dev").unwrap(), "1.0.0.0-RC1-dev");
        assert_eq!(
            parser.normalize("1.0.0-rC15-dev").unwrap(),
            "1.0.0.0-RC15-dev"
        );
        assert_eq!(
            parser.normalize("1.0.0.RC.15-dev").unwrap(),
            "1.0.0.0-RC15-dev"
        );
        assert_eq!(parser.normalize("1.0.0-rc1").unwrap(), "1.0.0.0-RC1");
        assert_eq!(
            parser.normalize("1.0.0.pl3-dev").unwrap(),
            "1.0.0.0-patch3-dev"
        );
        assert_eq!(parser.normalize("1.0-dev").unwrap(), "1.0.0.0-dev");
        assert_eq!(parser.normalize("0").unwrap(), "0.0.0.0");
        assert_eq!(parser.normalize("99999").unwrap(), "99999.0.0.0");
        assert_eq!(parser.normalize("10.4.13-beta").unwrap(), "10.4.13.0-beta");
        assert_eq!(parser.normalize("10.4.13beta2").unwrap(), "10.4.13.0-beta2");
        assert_eq!(
            parser.normalize("10.4.13beta.2").unwrap(),
            "10.4.13.0-beta2"
        );
        assert_eq!(
            parser.normalize("v1.13.11-beta.0").unwrap(),
            "1.13.11.0-beta0"
        );
        assert_eq!(
            parser.normalize("1.13.11.0-beta0").unwrap(),
            "1.13.11.0-beta0"
        );
        assert_eq!(parser.normalize("10.4.13-b").unwrap(), "10.4.13.0-beta");
        assert_eq!(parser.normalize("10.4.13-b5").unwrap(), "10.4.13.0-beta5");
        assert_eq!(parser.normalize("v1.0.0").unwrap(), "1.0.0.0");

        // Date parsing
        assert_eq!(parser.normalize("2010.01").unwrap(), "2010.01.0.0");
        assert_eq!(parser.normalize("2010.01.02").unwrap(), "2010.01.02.0");
        assert_eq!(parser.normalize("2010.1.555").unwrap(), "2010.1.555.0");
        assert_eq!(parser.normalize("2010.10.200").unwrap(), "2010.10.200.0");
        assert_eq!(parser.normalize("v20100102").unwrap(), "20100102");
        assert_eq!(parser.normalize("20100102").unwrap(), "20100102");
        assert_eq!(parser.normalize("20100102.0").unwrap(), "20100102.0");
        assert_eq!(parser.normalize("20100102.1.0").unwrap(), "20100102.1.0");
        assert_eq!(parser.normalize("20100102.0.3").unwrap(), "20100102.0.3");
        assert_eq!(parser.normalize("2010-01-02").unwrap(), "2010.01.02");

        // Dev branches
        assert_eq!(parser.normalize("dev-master").unwrap(), "dev-master");
        assert_eq!(parser.normalize("master").unwrap(), "dev-master");
        assert_eq!(parser.normalize("dev-trunk").unwrap(), "dev-trunk");
        assert_eq!(
            parser.normalize("1.x-dev").unwrap(),
            "1.9999999.9999999.9999999-dev"
        );
        assert_eq!(
            parser.normalize("V1.X-DEV").unwrap(),
            "1.9999999.9999999.9999999-dev"
        );
        assert_eq!(
            parser.normalize("1.2.*dev").unwrap(),
            "1.2.9999999.9999999-dev"
        );
        assert_eq!(parser.normalize("1.2-dev").unwrap(), "1.2.0.0-dev");
        assert_eq!(
            parser.normalize("dev-feature-foo").unwrap(),
            "dev-feature-foo"
        );
        assert_eq!(parser.normalize("DEV-FOOBAR").unwrap(), "dev-FOOBAR");
        assert_eq!(
            parser.normalize("dev-feature/foo").unwrap(),
            "dev-feature/foo"
        );
        assert_eq!(
            parser.normalize("dev-feature+issue-1").unwrap(),
            "dev-feature+issue-1"
        );

        // Aliases
        assert_eq!(
            parser.normalize("dev-master as 1.0.0").unwrap(),
            "dev-master"
        );
        assert_eq!(
            parser
                .normalize("dev-load-varnish-only-when-used as ^2.0")
                .unwrap(),
            "dev-load-varnish-only-when-used"
        );

        // Stability flags
        assert_eq!(parser.normalize("1.0.0+foo@dev").unwrap(), "1.0.0.0");
        assert_eq!(
            parser
                .normalize("dev-load-varnish-only-when-used@stable")
                .unwrap(),
            "dev-load-varnish-only-when-used"
        );

        // Semver metadata
        assert_eq!(
            parser.normalize("1.0.0-beta.5+foo").unwrap(),
            "1.0.0.0-beta5"
        );
        assert_eq!(parser.normalize("1.0.0+foo").unwrap(), "1.0.0.0");
        assert_eq!(
            parser.normalize("1.0.0-alpha.3.1+foo").unwrap(),
            "1.0.0.0-alpha3.1"
        );
        assert_eq!(
            parser.normalize("1.0.0-alpha2.1+foo").unwrap(),
            "1.0.0.0-alpha2.1"
        );
        assert_eq!(parser.normalize("1.0.0+foo as 2.0").unwrap(), "1.0.0.0");

        // Zero padding
        assert_eq!(parser.normalize("00.01.03.04").unwrap(), "00.01.03.04");
        assert_eq!(
            parser.normalize("000.001.003.004").unwrap(),
            "000.001.003.004"
        );

        // Space padding
        assert_eq!(parser.normalize(" 1.0.0").unwrap(), "1.0.0.0");
        assert_eq!(parser.normalize("1.0.0 ").unwrap(), "1.0.0.0");
    }

    #[test]
    fn test_normalize_fails() {
        let parser = VersionParser::new();

        assert!(parser.normalize("").is_err());
        assert!(parser.normalize("a").is_err());
        assert!(parser.normalize("1.0.0-meh").is_err());
        assert!(parser.normalize("1.0.0.0.0").is_err());
        assert!(parser.normalize("feature-foo").is_err());
        assert!(parser.normalize("1.0.0+foo bar").is_err());
        assert!(parser.normalize("1.0.0-SNAPSHOT").is_err());
        assert!(parser.normalize("1.0 .2").is_err());
        assert!(parser.normalize(" as ").is_err());
        assert!(parser.normalize(" as 1.2").is_err());
        assert!(parser.normalize("^").is_err());
        assert!(parser.normalize("~").is_err());
        assert!(parser.normalize("~1").is_err());
        assert!(parser.normalize("^1").is_err());
        assert!(parser.normalize("1.*").is_err());
    }

    #[test]
    fn test_is_valid() {
        let parser = VersionParser::new();

        assert!(parser.is_valid("0.x-dev"));
        assert!(parser.is_valid("dev-develop"));
        assert!(parser.is_valid("1.0.2"));
        assert!(parser.is_valid("1.0.2.5"));
        assert!(!parser.is_valid("1.0.2.5.5"));
        assert!(!parser.is_valid("foo"));
    }

    #[test]
    fn test_parse_numeric_alias_prefix() {
        let parser = VersionParser::new();

        assert_eq!(
            parser.parse_numeric_alias_prefix("0.x-dev"),
            Some("0.".to_string())
        );
        assert_eq!(
            parser.parse_numeric_alias_prefix("1.0.x-dev"),
            Some("1.0.".to_string())
        );
        assert_eq!(
            parser.parse_numeric_alias_prefix("1.x-dev"),
            Some("1.".to_string())
        );
        assert_eq!(
            parser.parse_numeric_alias_prefix("1.2.x-dev"),
            Some("1.2.".to_string())
        );
        assert_eq!(
            parser.parse_numeric_alias_prefix("1.2-dev"),
            Some("1.2.".to_string())
        );
        assert_eq!(
            parser.parse_numeric_alias_prefix("1-dev"),
            Some("1.".to_string())
        );
        assert_eq!(parser.parse_numeric_alias_prefix("dev-develop"), None);
        assert_eq!(parser.parse_numeric_alias_prefix("dev-master"), None);
    }

    #[test]
    fn test_normalize_branch() {
        let parser = VersionParser::new();

        assert_eq!(
            parser.normalize_branch("v1.x").unwrap(),
            "1.9999999.9999999.9999999-dev"
        );
        assert_eq!(
            parser.normalize_branch("v1.*").unwrap(),
            "1.9999999.9999999.9999999-dev"
        );
        assert_eq!(
            parser.normalize_branch("v1.0").unwrap(),
            "1.0.9999999.9999999-dev"
        );
        assert_eq!(
            parser.normalize_branch("2.0").unwrap(),
            "2.0.9999999.9999999-dev"
        );
        assert_eq!(
            parser.normalize_branch("v1.0.x").unwrap(),
            "1.0.9999999.9999999-dev"
        );
        assert_eq!(
            parser.normalize_branch("v1.0.3.*").unwrap(),
            "1.0.3.9999999-dev"
        );
        assert_eq!(
            parser.normalize_branch("V1.X").unwrap(),
            "1.9999999.9999999.9999999-dev"
        );
        assert_eq!(parser.normalize_branch("v1..x").unwrap(), "dev-v1..x");
        assert_eq!(parser.normalize_branch("x.1").unwrap(), "dev-x.1");
        assert_eq!(
            parser.normalize_branch("v2.4.0").unwrap(),
            "2.4.0.9999999-dev"
        );
        assert_eq!(
            parser.normalize_branch("2.4.4").unwrap(),
            "2.4.4.9999999-dev"
        );
        assert_eq!(parser.normalize_branch("master").unwrap(), "dev-master");
        assert_eq!(parser.normalize_branch("trunk").unwrap(), "dev-trunk");
        assert_eq!(
            parser.normalize_branch("feature-a").unwrap(),
            "dev-feature-a"
        );
        assert_eq!(parser.normalize_branch("FOOBAR").unwrap(), "dev-FOOBAR");
        assert_eq!(
            parser.normalize_branch("feature+issue-1").unwrap(),
            "dev-feature+issue-1"
        );
    }

    #[test]
    fn test_parse_constraints_simple() {
        let parser = VersionParser::new();

        // Match any
        assert!(parser.parse_constraints("*").is_ok());

        // Not equal
        assert_eq!(
            parser.parse_constraints("<>1.0.0").unwrap().to_string(),
            "!= 1.0.0.0"
        );
        assert_eq!(
            parser.parse_constraints("!=1.0.0").unwrap().to_string(),
            "!= 1.0.0.0"
        );

        // Comparisons
        assert_eq!(
            parser.parse_constraints(">1.0.0").unwrap().to_string(),
            "> 1.0.0.0"
        );
        assert_eq!(
            parser.parse_constraints("<1.2.3.4").unwrap().to_string(),
            "< 1.2.3.4-dev"
        );
        assert_eq!(
            parser.parse_constraints("<=1.2.3").unwrap().to_string(),
            "<= 1.2.3.0"
        );
        assert_eq!(
            parser.parse_constraints(">=1.2.3").unwrap().to_string(),
            ">= 1.2.3.0-dev"
        );
        assert_eq!(
            parser.parse_constraints("=1.2.3").unwrap().to_string(),
            "== 1.2.3.0"
        );
        assert_eq!(
            parser.parse_constraints("==1.2.3").unwrap().to_string(),
            "== 1.2.3.0"
        );
        assert_eq!(
            parser.parse_constraints("1.2.3").unwrap().to_string(),
            "== 1.2.3.0"
        );

        // Shorthand stability
        assert_eq!(
            parser.parse_constraints("1.2.3b5").unwrap().to_string(),
            "== 1.2.3.0-beta5"
        );
        assert_eq!(
            parser.parse_constraints("1.2.3a1").unwrap().to_string(),
            "== 1.2.3.0-alpha1"
        );
        assert_eq!(
            parser.parse_constraints("1.2.3p1234").unwrap().to_string(),
            "== 1.2.3.0-patch1234"
        );

        // With spaces
        assert_eq!(
            parser.parse_constraints(">= 1.2.3").unwrap().to_string(),
            ">= 1.2.3.0-dev"
        );
        assert_eq!(
            parser.parse_constraints("< 1.2.3").unwrap().to_string(),
            "< 1.2.3.0-dev"
        );
        assert_eq!(
            parser.parse_constraints("> 1.2.3").unwrap().to_string(),
            "> 1.2.3.0"
        );

        // Dev branches
        assert_eq!(
            parser
                .parse_constraints(">=dev-master")
                .unwrap()
                .to_string(),
            ">= dev-master"
        );
        assert_eq!(
            parser.parse_constraints("dev-master").unwrap().to_string(),
            "== dev-master"
        );
        assert_eq!(
            parser
                .parse_constraints("dev-feature-a")
                .unwrap()
                .to_string(),
            "== dev-feature-a"
        );

        // Aliases
        assert_eq!(
            parser
                .parse_constraints("dev-master as 1.0.0")
                .unwrap()
                .to_string(),
            "== dev-master"
        );
    }

    #[test]
    fn test_parse_constraints_wildcard() {
        let parser = VersionParser::new();

        assert_eq!(
            parser.parse_constraints("v2.*").unwrap().to_string(),
            "[>= 2.0.0.0-dev < 3.0.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("2.*.*").unwrap().to_string(),
            "[>= 2.0.0.0-dev < 3.0.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("20.*").unwrap().to_string(),
            "[>= 20.0.0.0-dev < 21.0.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("2.0.*").unwrap().to_string(),
            "[>= 2.0.0.0-dev < 2.1.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("2.x").unwrap().to_string(),
            "[>= 2.0.0.0-dev < 3.0.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("2.x.x").unwrap().to_string(),
            "[>= 2.0.0.0-dev < 3.0.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("2.2.x").unwrap().to_string(),
            "[>= 2.2.0.0-dev < 2.3.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("2.10.X").unwrap().to_string(),
            "[>= 2.10.0.0-dev < 2.11.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("2.1.3.*").unwrap().to_string(),
            "[>= 2.1.3.0-dev < 2.1.4.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("0.*").unwrap().to_string(),
            "< 1.0.0.0-dev"
        );
        assert_eq!(
            parser.parse_constraints("0.x").unwrap().to_string(),
            "< 1.0.0.0-dev"
        );
    }

    #[test]
    fn test_parse_constraints_tilde() {
        let parser = VersionParser::new();

        assert_eq!(
            parser.parse_constraints("~v1").unwrap().to_string(),
            "[>= 1.0.0.0-dev < 2.0.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("~1.0").unwrap().to_string(),
            "[>= 1.0.0.0-dev < 2.0.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("~1.0.0").unwrap().to_string(),
            "[>= 1.0.0.0-dev < 1.1.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("~1.2").unwrap().to_string(),
            "[>= 1.2.0.0-dev < 2.0.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("~1.2.3").unwrap().to_string(),
            "[>= 1.2.3.0-dev < 1.3.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("~1.2.3.4").unwrap().to_string(),
            "[>= 1.2.3.4-dev < 1.2.4.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("~1.2-beta").unwrap().to_string(),
            "[>= 1.2.0.0-beta < 2.0.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("~1.2-b2").unwrap().to_string(),
            "[>= 1.2.0.0-beta2 < 2.0.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("~1.2-BETA2").unwrap().to_string(),
            "[>= 1.2.0.0-beta2 < 2.0.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("~1.2.2-dev").unwrap().to_string(),
            "[>= 1.2.2.0-dev < 1.3.0.0-dev]"
        );
    }

    #[test]
    fn test_parse_constraints_caret() {
        let parser = VersionParser::new();

        assert_eq!(
            parser.parse_constraints("^v1").unwrap().to_string(),
            "[>= 1.0.0.0-dev < 2.0.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("^0").unwrap().to_string(),
            "[>= 0.0.0.0-dev < 1.0.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("^0.0").unwrap().to_string(),
            "[>= 0.0.0.0-dev < 0.1.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("^1.2").unwrap().to_string(),
            "[>= 1.2.0.0-dev < 2.0.0.0-dev]"
        );
        assert_eq!(
            parser
                .parse_constraints("^1.2.3-beta.2")
                .unwrap()
                .to_string(),
            "[>= 1.2.3.0-beta2 < 2.0.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("^1.2.3.4").unwrap().to_string(),
            "[>= 1.2.3.4-dev < 2.0.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("^1.2.3").unwrap().to_string(),
            "[>= 1.2.3.0-dev < 2.0.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("^0.2.3").unwrap().to_string(),
            "[>= 0.2.3.0-dev < 0.3.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("^0.2").unwrap().to_string(),
            "[>= 0.2.0.0-dev < 0.3.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("^0.2.0").unwrap().to_string(),
            "[>= 0.2.0.0-dev < 0.3.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("^0.0.3").unwrap().to_string(),
            "[>= 0.0.3.0-dev < 0.0.4.0-dev]"
        );
        assert_eq!(
            parser
                .parse_constraints("^0.0.3-alpha")
                .unwrap()
                .to_string(),
            "[>= 0.0.3.0-alpha < 0.0.4.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("^0.0.3-dev").unwrap().to_string(),
            "[>= 0.0.3.0-dev < 0.0.4.0-dev]"
        );
    }

    #[test]
    fn test_parse_constraints_hyphen() {
        let parser = VersionParser::new();

        assert_eq!(
            parser.parse_constraints("v1 - v2").unwrap().to_string(),
            "[>= 1.0.0.0-dev < 3.0.0.0-dev]"
        );
        assert_eq!(
            parser
                .parse_constraints("1.2.3 - 2.3.4.5")
                .unwrap()
                .to_string(),
            "[>= 1.2.3.0-dev <= 2.3.4.5]"
        );
        assert_eq!(
            parser
                .parse_constraints("1.2-beta - 2.3")
                .unwrap()
                .to_string(),
            "[>= 1.2.0.0-beta < 2.4.0.0-dev]"
        );
        assert_eq!(
            parser
                .parse_constraints("1.2-beta - 2.3-dev")
                .unwrap()
                .to_string(),
            "[>= 1.2.0.0-beta <= 2.3.0.0-dev]"
        );
        assert_eq!(
            parser
                .parse_constraints("1.2-RC - 2.3.1")
                .unwrap()
                .to_string(),
            "[>= 1.2.0.0-RC <= 2.3.1.0]"
        );
        assert_eq!(
            parser
                .parse_constraints("1.2.3-alpha - 2.3-RC")
                .unwrap()
                .to_string(),
            "[>= 1.2.3.0-alpha <= 2.3.0.0-RC]"
        );
        assert_eq!(
            parser.parse_constraints("1 - 2.0").unwrap().to_string(),
            "[>= 1.0.0.0-dev < 2.1.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("1 - 2.1").unwrap().to_string(),
            "[>= 1.0.0.0-dev < 2.2.0.0-dev]"
        );
        assert_eq!(
            parser.parse_constraints("1.2 - 2.1.0").unwrap().to_string(),
            "[>= 1.2.0.0-dev <= 2.1.0.0]"
        );
        assert_eq!(
            parser.parse_constraints("1.3 - 2.1.3").unwrap().to_string(),
            "[>= 1.3.0.0-dev <= 2.1.3.0]"
        );
    }

    #[test]
    fn test_parse_constraints_multi() {
        let parser = VersionParser::new();

        // Various AND constraint formats
        let expected = "[> 2.0.0.0 <= 3.0.0.0]";
        assert_eq!(
            parser.parse_constraints(">2.0,<=3.0").unwrap().to_string(),
            expected
        );
        assert_eq!(
            parser.parse_constraints(">2.0 <=3.0").unwrap().to_string(),
            expected
        );
        assert_eq!(
            parser.parse_constraints(">2.0  <=3.0").unwrap().to_string(),
            expected
        );
        assert_eq!(
            parser.parse_constraints(">2.0, <=3.0").unwrap().to_string(),
            expected
        );
        assert_eq!(
            parser.parse_constraints(">2.0 ,<=3.0").unwrap().to_string(),
            expected
        );
        assert_eq!(
            parser
                .parse_constraints(">2.0 , <=3.0")
                .unwrap()
                .to_string(),
            expected
        );
        assert_eq!(
            parser
                .parse_constraints("> 2.0   <=  3.0")
                .unwrap()
                .to_string(),
            expected
        );
        assert_eq!(
            parser
                .parse_constraints("> 2.0  ,  <=  3.0")
                .unwrap()
                .to_string(),
            expected
        );
        assert_eq!(
            parser
                .parse_constraints("  > 2.0  ,  <=  3.0 ")
                .unwrap()
                .to_string(),
            expected
        );
    }

    #[test]
    fn test_parse_constraints_multi_disjunctive() {
        let parser = VersionParser::new();

        // OR has priority over AND
        let result = parser
            .parse_constraints(">2.0,<2.0.5 | >2.0.6")
            .unwrap()
            .to_string();
        assert!(result.contains("> 2.0.0.0"));
        assert!(result.contains("< 2.0.5.0-dev"));
        assert!(result.contains("> 2.0.6.0"));
        assert!(result.contains("||"));
    }

    #[test]
    fn test_parse_constraints_fails() {
        let parser = VersionParser::new();

        // Empty
        assert!(parser.parse_constraints("").is_err());

        // Invalid version
        assert!(parser.parse_constraints("1.0.0-meh").is_err());

        // Leading/trailing operators
        assert!(parser.parse_constraints("|| ^1@dev").is_err());
        assert!(parser.parse_constraints("^1@dev ||").is_err());

        // Just an operator
        assert!(parser.parse_constraints("^").is_err());
        assert!(parser.parse_constraints("^8 || ^").is_err());
        assert!(parser.parse_constraints("~").is_err());
    }

    #[test]
    fn test_parse_constraints_nudges_ruby_devs() {
        let parser = VersionParser::new();

        let result = parser.parse_constraints("~>1.2");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("~>"));
    }

    #[test]
    fn test_parse_constraints_ignores_stability_flag() {
        let parser = VersionParser::new();

        assert_eq!(
            parser.parse_constraints("1.0@dev").unwrap().to_string(),
            "== 1.0.0.0"
        );
        assert_eq!(
            parser
                .parse_constraints("dev-load-varnish-only-when-used as ^2.0@dev")
                .unwrap()
                .to_string(),
            "== dev-load-varnish-only-when-used"
        );
    }

    #[test]
    fn test_parse_constraints_ignores_reference_on_dev_version() {
        let parser = VersionParser::new();

        assert_eq!(
            parser
                .parse_constraints("1.0.x-dev#abcd123")
                .unwrap()
                .to_string(),
            "== 1.0.9999999.9999999-dev"
        );
        assert_eq!(
            parser
                .parse_constraints("1.0.x-dev#trunk/@123")
                .unwrap()
                .to_string(),
            "== 1.0.9999999.9999999-dev"
        );
    }

    #[test]
    fn test_tilde_constraint_matching() {
        use crate::constraint::{Constraint, Operator};

        let parser = VersionParser::new();

        // Parse ~7.3.0 constraint
        let constraint = parser.parse_constraints("~7.3.0").unwrap();
        println!("~7.3.0 parsed as: {}", constraint);

        // Test if 7.3.8.0 matches - should match because ~7.3.0 means >=7.3.0 <7.4.0
        let v738 = Constraint::new(Operator::Equal, "7.3.8.0".to_string()).unwrap();
        assert!(constraint.matches(&v738), "7.3.8.0 should match ~7.3.0");

        // Test lower bound
        let v730 = Constraint::new(Operator::Equal, "7.3.0.0".to_string()).unwrap();
        assert!(constraint.matches(&v730), "7.3.0.0 should match ~7.3.0");

        // Test upper bound (7.4 should NOT match)
        let v740 = Constraint::new(Operator::Equal, "7.4.0.0".to_string()).unwrap();
        assert!(
            !constraint.matches(&v740),
            "7.4.0.0 should NOT match ~7.3.0"
        );

        // Test v7.3.8 normalized
        let normalized = parser.normalize("v7.3.8").unwrap();
        println!("v7.3.8 normalized: {}", normalized);
        let v738_normalized = Constraint::new(Operator::Equal, normalized).unwrap();
        assert!(
            constraint.matches(&v738_normalized),
            "Normalized v7.3.8 should match ~7.3.0"
        );
    }

    #[test]
    fn test_constraint_to_constraint_matching_dev_version() {
        // This tests how Composer handles provide/replace constraint matching
        // Root package has replace: {"shopware/core": "=6.7.9999999.9999999-dev"}
        // Other package requires "shopware/core": ">=6.7.2.0"
        // These should match because =6.7.9999999.9999999-dev satisfies >=6.7.2.0

        let parser = VersionParser::new();

        // The require constraint: >=6.7.2.0
        let require_constraint = parser.parse_constraints(">=6.7.2.0").unwrap();
        println!(">=6.7.2.0 parsed as: {}", require_constraint);

        // The provide/replace constraint: =6.7.9999999.9999999-dev
        let provide_constraint = parser
            .parse_constraints("=6.7.9999999.9999999-dev")
            .unwrap();
        println!("=6.7.9999999.9999999-dev parsed as: {}", provide_constraint);

        // These constraints should match (intersect)
        let matches = require_constraint.matches(provide_constraint.as_ref());
        println!(">=6.7.2.0 matches =6.7.9999999.9999999-dev? {}", matches);

        assert!(matches, ">=6.7.2.0 should match =6.7.9999999.9999999-dev");
    }

    #[test]
    fn test_caret_constraint_matching() {
        use crate::constraint::{Constraint, Operator};

        let parser = VersionParser::new();

        // Parse ^9.3 constraint
        let constraint = parser.parse_constraints("^9.3").unwrap();
        println!("^9.3 parsed as: {}", constraint);

        // Test if 9.3.0.0 matches - should match
        let v930 = Constraint::new(Operator::Equal, "9.3.0.0".to_string()).unwrap();
        assert!(constraint.matches(&v930), "9.3.0.0 should match ^9.3");

        // Test if 9.4.0.0 matches - should match (^9.3 means >=9.3.0 <10.0.0)
        let v940 = Constraint::new(Operator::Equal, "9.4.0.0".to_string()).unwrap();
        assert!(constraint.matches(&v940), "9.4.0.0 should match ^9.3");

        // Test if 10.0.0.0 matches - should NOT match
        let v1000 = Constraint::new(Operator::Equal, "10.0.0.0".to_string()).unwrap();
        assert!(
            !constraint.matches(&v1000),
            "10.0.0.0 should NOT match ^9.3"
        );
    }

    #[test]
    fn test_or_constraint_matching() {
        use crate::constraint::{Constraint, Operator};

        let parser = VersionParser::new();

        // Parse ^2.3 || ^3.0 constraint (like lcobucci/clock requirement)
        let constraint = parser.parse_constraints("^2.3 || ^3.0").unwrap();
        println!("^2.3 || ^3.0 parsed as: {}", constraint);

        // Test if 3.5.0.0 matches - should match
        let v350 = Constraint::new(Operator::Equal, "3.5.0.0".to_string()).unwrap();
        assert!(
            constraint.matches(&v350),
            "3.5.0.0 should match ^2.3 || ^3.0"
        );

        // Test if 2.5.0.0 matches - should match
        let v250 = Constraint::new(Operator::Equal, "2.5.0.0".to_string()).unwrap();
        assert!(
            constraint.matches(&v250),
            "2.5.0.0 should match ^2.3 || ^3.0"
        );

        // Test if 1.0.0.0 matches - should NOT match
        let v100 = Constraint::new(Operator::Equal, "1.0.0.0".to_string()).unwrap();
        assert!(
            !constraint.matches(&v100),
            "1.0.0.0 should NOT match ^2.3 || ^3.0"
        );
    }
}
