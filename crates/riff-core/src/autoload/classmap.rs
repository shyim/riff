//! Classmap generator - scans PHP files for class definitions.

use memchr::{memchr2, memmem};
use regex::Regex;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::Result;

/// Generates a classmap by scanning PHP files.
pub struct ClassMapGenerator;

impl ClassMapGenerator {
    /// Create a new classmap generator
    pub fn new() -> Self {
        Self
    }

    /// Generate classmap for a directory
    pub fn generate(&self, path: &Path) -> Result<HashMap<String, PathBuf>> {
        self.generate_with_excludes(path, &[])
    }

    /// Generate classmap for a directory with exclusion patterns
    pub fn generate_with_excludes(
        &self,
        path: &Path,
        excludes: &[Regex],
    ) -> Result<HashMap<String, PathBuf>> {
        let mut classmap = HashMap::new();

        if !path.exists() {
            return Ok(classmap);
        }

        for entry in WalkDir::new(path)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let file_path = entry.path();

            // Only process PHP files
            if !Self::is_php_file(file_path) {
                continue;
            }

            // Check if path matches any exclusion pattern
            if self.is_excluded(file_path, excludes) {
                continue;
            }

            // Read and parse the file
            if let Ok(content) = std::fs::read(file_path) {
                let content = php_source_text(&content);
                let classes = self.extract_classes(content.as_ref());
                for class in classes {
                    classmap.insert(class, file_path.to_path_buf());
                }
            }
        }

        Ok(classmap)
    }

    /// Check if a path matches any exclusion pattern
    fn is_excluded(&self, path: &Path, excludes: &[Regex]) -> bool {
        if excludes.is_empty() {
            return false;
        }

        // Normalize path to forward slashes for matching
        let path_str = path.to_string_lossy().replace('\\', "/");

        for pattern in excludes {
            if pattern.is_match(&path_str) {
                return true;
            }
        }

        false
    }

    /// Generate classmap for multiple directories
    pub fn generate_from_paths(&self, paths: &[PathBuf]) -> Result<HashMap<String, PathBuf>> {
        self.generate_from_paths_with_excludes(paths, &[])
    }

    /// Generate classmap for multiple directories with exclusion patterns
    pub fn generate_from_paths_with_excludes(
        &self,
        paths: &[PathBuf],
        excludes: &[Regex],
    ) -> Result<HashMap<String, PathBuf>> {
        let mut classmap = HashMap::new();

        for path in paths {
            let map = self.generate_with_excludes(path, excludes)?;
            classmap.extend(map);
        }

        Ok(classmap)
    }

    /// Extract class names from PHP content
    fn extract_classes(&self, content: &str) -> Vec<String> {
        let mut tokens = PhpTokens::new(content).peekable();
        let mut classes = Vec::new();
        let mut namespace = String::new();
        let mut previous = None;

        while let Some(token) = tokens.next() {
            let PhpTokenKind::Identifier(keyword) = token.kind else {
                previous = Some(token);
                continue;
            };
            if keyword.eq_ignore_ascii_case("namespace") && declaration_keyword(previous.as_ref()) {
                previous = Some(token);
                if tokens.peek().is_some_and(|next| next.start > token.end) {
                    let mut candidate = String::new();
                    while let Some(next) = tokens.peek().copied() {
                        match next.kind {
                            PhpTokenKind::Identifier(part) => candidate.push_str(part),
                            PhpTokenKind::Symbol('\\') => candidate.push('\\'),
                            PhpTokenKind::Symbol(';') | PhpTokenKind::Symbol('{') => {
                                namespace = candidate.trim_matches('\\').to_string();
                                previous = tokens.next();
                                break;
                            }
                            _ => break,
                        }
                        previous = tokens.next();
                    }
                }
                continue;
            } else if matches_ignore_ascii_case(keyword, &["class", "interface", "trait", "enum"])
                && declaration_keyword(previous.as_ref())
            {
                if let Some(&PhpToken {
                    kind: PhpTokenKind::Identifier(name),
                    start,
                    ..
                }) = tokens.peek()
                {
                    if start > token.end
                        && !matches_ignore_ascii_case(name, &["extends", "implements"])
                    {
                        let name = if name.starts_with(':') {
                            let encoded = name.replace('-', "_").replace(':', "__");
                            format!("xhp{}", &encoded[1..])
                        } else {
                            name.split(':').next().unwrap_or(name).to_string()
                        };
                        classes.push(if namespace.is_empty() {
                            name
                        } else {
                            format!("{namespace}\\{name}")
                        });
                    }
                    previous = tokens.next();
                    continue;
                }
            }
            previous = Some(token);
        }

        classes
    }

    /// Check if a file is a PHP file
    fn is_php_file(path: &Path) -> bool {
        path.extension()
            .map(|ext| {
                ext.eq_ignore_ascii_case("php")
                    || ext.eq_ignore_ascii_case("inc")
                    || ext.eq_ignore_ascii_case("hh")
            })
            .unwrap_or(false)
    }
}

#[derive(Clone, Copy, Debug)]
struct PhpToken<'a> {
    kind: PhpTokenKind<'a>,
    start: usize,
    end: usize,
}

#[derive(Clone, Copy, Debug)]
enum PhpTokenKind<'a> {
    Identifier(&'a str),
    Symbol(char),
}

fn declaration_keyword(previous: Option<&PhpToken<'_>>) -> bool {
    !matches!(
        previous,
        Some(PhpToken {
            kind: PhpTokenKind::Symbol('\\' | ':' | '>' | '$'),
            ..
        })
    )
}

struct PhpTokens<'a> {
    content: &'a str,
    index: usize,
    in_php: bool,
}

impl<'a> PhpTokens<'a> {
    fn new(content: &'a str) -> Self {
        Self {
            content,
            index: 0,
            in_php: false,
        }
    }
}

impl<'a> Iterator for PhpTokens<'a> {
    type Item = PhpToken<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.index < self.content.len() {
            let bytes = self.content.as_bytes();
            if !self.in_php {
                let relative = memmem::find(&bytes[self.index..], b"<?")?;
                self.index += relative + 2;
                if bytes[self.index..]
                    .get(..3)
                    .is_some_and(|tag| tag.eq_ignore_ascii_case(b"php"))
                {
                    self.index += 3;
                }
                self.in_php = true;
                continue;
            }

            if bytes[self.index..].starts_with(b"?>") {
                self.index += 2;
                self.in_php = false;
                continue;
            }
            if bytes[self.index..].starts_with(b"//")
                || (bytes[self.index] == b'#' && !bytes[self.index..].starts_with(b"#["))
            {
                self.index = memchr2(b'\r', b'\n', &bytes[self.index..])
                    .map(|relative| self.index + relative)
                    .unwrap_or(self.content.len());
                continue;
            }
            if bytes[self.index..].starts_with(b"/*") {
                self.index = memmem::find(&bytes[self.index + 2..], b"*/")
                    .map(|relative| self.index + 2 + relative + 2)
                    .unwrap_or(self.content.len());
                continue;
            }
            if bytes[self.index..].starts_with(b"<<<") {
                self.index = skip_heredoc(self.content, self.index);
                continue;
            }

            let byte = bytes[self.index];
            if matches!(byte, b'\'' | b'"' | b'`') {
                self.index = skip_quoted(self.content, self.index, byte);
                continue;
            }
            if byte.is_ascii_whitespace() {
                self.index += simd_scan::ascii_whitespace_prefix(&bytes[self.index..]);
                continue;
            }
            if is_ascii_identifier_start(byte) || !byte.is_ascii() {
                let start = self.index;
                self.index += simd_scan::identifier_prefix(&bytes[self.index..]);
                return Some(PhpToken {
                    kind: PhpTokenKind::Identifier(&self.content[start..self.index]),
                    start,
                    end: self.index,
                });
            }

            let character = char::from(byte);
            let start = self.index;
            self.index += 1;
            return Some(PhpToken {
                kind: PhpTokenKind::Symbol(character),
                start,
                end: self.index,
            });
        }

        None
    }
}

fn skip_quoted(content: &str, mut index: usize, delimiter: u8) -> usize {
    let bytes = content.as_bytes();
    index += 1;
    while index < content.len() {
        let Some(relative) = memchr2(b'\\', delimiter, &bytes[index..]) else {
            return content.len();
        };
        index += relative + 1;
        if bytes[index - 1] == delimiter {
            return index;
        }
        if index < content.len() {
            let escaped = content[index..].chars().next().unwrap();
            index += escaped.len_utf8();
        }
    }
    index
}

fn skip_heredoc(content: &str, index: usize) -> usize {
    let bytes = content.as_bytes();
    let line_end = memchr2(b'\r', b'\n', &bytes[index..])
        .map(|relative| index + relative)
        .unwrap_or(content.len());
    let declaration = content[index + 3..line_end].trim();
    let delimiter = declaration.trim_matches(['\'', '"']);
    if delimiter.is_empty() {
        return line_end;
    }
    let mut cursor = line_end;
    while cursor < content.len() {
        cursor += content[cursor..]
            .chars()
            .take_while(|character| matches!(character, '\r' | '\n'))
            .map(char::len_utf8)
            .sum::<usize>();
        let next_end = memchr2(b'\r', b'\n', &bytes[cursor..])
            .map(|relative| cursor + relative)
            .unwrap_or(content.len());
        let line = content[cursor..next_end].trim_start();
        if line.strip_prefix(delimiter).is_some_and(|remainder| {
            remainder.is_empty()
                || matches!(
                    remainder.as_bytes().first(),
                    Some(b';' | b',' | b')' | b']')
                )
        }) {
            return next_end;
        }
        cursor = next_end;
    }
    content.len()
}

fn is_ascii_identifier_start(byte: u8) -> bool {
    byte == b'_' || byte == b':' || byte.is_ascii_alphabetic()
}

const INVALID_BYTE_MARKER_START: u32 = 0xE000;

fn php_source_text(bytes: &[u8]) -> Cow<'_, str> {
    if let Ok(valid) = simdutf8::basic::from_utf8(bytes) {
        return Cow::Borrowed(valid);
    }

    let mut source = String::new();
    let mut remaining = bytes;
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(valid) => {
                source.push_str(valid);
                break;
            }
            Err(error) => {
                let valid = &remaining[..error.valid_up_to()];
                source.push_str(std::str::from_utf8(valid).expect("validated UTF-8 prefix"));
                let invalid_len = error.error_len().unwrap_or(remaining.len() - valid.len());
                for byte in &remaining[valid.len()..valid.len() + invalid_len] {
                    source.push(
                        char::from_u32(INVALID_BYTE_MARKER_START + u32::from(*byte))
                            .expect("private-use invalid byte marker"),
                    );
                }
                remaining = &remaining[valid.len() + invalid_len..];
            }
        }
    }
    Cow::Owned(source)
}

mod simd_scan {
    const WIDTH: usize = 16;

    pub(super) fn identifier_prefix(bytes: &[u8]) -> usize {
        #[cfg(target_arch = "x86_64")]
        {
            x86_64::identifier_prefix(bytes)
        }
        #[cfg(target_arch = "aarch64")]
        {
            aarch64::identifier_prefix(bytes)
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        scalar_identifier_prefix(bytes)
    }

    pub(super) fn ascii_whitespace_prefix(bytes: &[u8]) -> usize {
        #[cfg(target_arch = "x86_64")]
        {
            x86_64::ascii_whitespace_prefix(bytes)
        }
        #[cfg(target_arch = "aarch64")]
        {
            aarch64::ascii_whitespace_prefix(bytes)
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        scalar_ascii_whitespace_prefix(bytes)
    }

    fn scalar_identifier_prefix(bytes: &[u8]) -> usize {
        bytes
            .iter()
            .position(|&byte| !is_identifier_continue(byte))
            .unwrap_or(bytes.len())
    }

    fn scalar_ascii_whitespace_prefix(bytes: &[u8]) -> usize {
        bytes
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .unwrap_or(bytes.len())
    }

    pub(super) fn is_identifier_continue(byte: u8) -> bool {
        byte == b'_'
            || byte == b':'
            || byte == b'-'
            || byte.is_ascii_alphanumeric()
            || !byte.is_ascii()
    }

    #[cfg(target_arch = "x86_64")]
    mod x86_64 {
        use super::{scalar_ascii_whitespace_prefix, scalar_identifier_prefix, WIDTH};
        use std::arch::x86_64::{
            __m128i, _mm_and_si128, _mm_andnot_si128, _mm_cmpeq_epi8, _mm_cmpgt_epi8,
            _mm_loadu_si128, _mm_movemask_epi8, _mm_or_si128, _mm_set1_epi8, _mm_setzero_si128,
        };

        pub(super) fn identifier_prefix(bytes: &[u8]) -> usize {
            let mut offset = 0;
            while bytes.len() - offset >= WIDTH {
                // SAFETY: `offset + WIDTH <= bytes.len()` guarantees a valid unaligned load.
                let mask = unsafe { identifier_mask(bytes.as_ptr().add(offset)) };
                if mask != u16::MAX as u32 {
                    return offset + (!mask & u16::MAX as u32).trailing_zeros() as usize;
                }
                offset += WIDTH;
            }
            offset + scalar_identifier_prefix(&bytes[offset..])
        }

        pub(super) fn ascii_whitespace_prefix(bytes: &[u8]) -> usize {
            let mut offset = 0;
            while bytes.len() - offset >= WIDTH {
                // SAFETY: `offset + WIDTH <= bytes.len()` guarantees a valid unaligned load.
                let mask = unsafe { ascii_whitespace_mask(bytes.as_ptr().add(offset)) };
                if mask != u16::MAX as u32 {
                    return offset + (!mask & u16::MAX as u32).trailing_zeros() as usize;
                }
                offset += WIDTH;
            }
            offset + scalar_ascii_whitespace_prefix(&bytes[offset..])
        }

        unsafe fn identifier_mask(pointer: *const u8) -> u32 {
            // SSE2 is part of the x86-64 baseline, so no runtime feature check is required.
            let bytes = unsafe { _mm_loadu_si128(pointer.cast::<__m128i>()) };
            let lowercase = _mm_or_si128(bytes, _mm_set1_epi8(0x20));
            let letters = _mm_and_si128(
                _mm_cmpgt_epi8(lowercase, _mm_set1_epi8(b'a' as i8 - 1)),
                _mm_cmpgt_epi8(_mm_set1_epi8(b'z' as i8 + 1), lowercase),
            );
            let digits = _mm_and_si128(
                _mm_cmpgt_epi8(bytes, _mm_set1_epi8(b'0' as i8 - 1)),
                _mm_cmpgt_epi8(_mm_set1_epi8(b'9' as i8 + 1), bytes),
            );
            let punctuation = _mm_or_si128(
                _mm_cmpeq_epi8(bytes, _mm_set1_epi8(b'_' as i8)),
                _mm_or_si128(
                    _mm_cmpeq_epi8(bytes, _mm_set1_epi8(b':' as i8)),
                    _mm_cmpeq_epi8(bytes, _mm_set1_epi8(b'-' as i8)),
                ),
            );
            let non_ascii = _mm_cmpgt_epi8(_mm_setzero_si128(), bytes);
            let valid = _mm_or_si128(
                _mm_or_si128(letters, digits),
                _mm_or_si128(punctuation, non_ascii),
            );
            _mm_movemask_epi8(valid) as u32
        }

        unsafe fn ascii_whitespace_mask(pointer: *const u8) -> u32 {
            // SSE2 is part of the x86-64 baseline, so no runtime feature check is required.
            let bytes = unsafe { _mm_loadu_si128(pointer.cast::<__m128i>()) };
            let horizontal = _mm_and_si128(
                _mm_cmpgt_epi8(bytes, _mm_set1_epi8(8)),
                _mm_cmpgt_epi8(_mm_set1_epi8(14), bytes),
            );
            let horizontal =
                _mm_andnot_si128(_mm_cmpeq_epi8(bytes, _mm_set1_epi8(0x0B)), horizontal);
            let spaces = _mm_cmpeq_epi8(bytes, _mm_set1_epi8(b' ' as i8));
            _mm_movemask_epi8(_mm_or_si128(horizontal, spaces)) as u32
        }
    }

    #[cfg(target_arch = "aarch64")]
    mod aarch64 {
        use super::{scalar_ascii_whitespace_prefix, scalar_identifier_prefix, WIDTH};
        use std::arch::aarch64::{
            uint8x16_t, vandq_u8, vbicq_u8, vceqq_u8, vcgeq_u8, vcleq_u8, vld1q_u8, vorrq_u8,
            vst1q_u8,
        };

        pub(super) fn identifier_prefix(bytes: &[u8]) -> usize {
            vector_prefix(bytes, identifier_lanes, scalar_identifier_prefix)
        }

        pub(super) fn ascii_whitespace_prefix(bytes: &[u8]) -> usize {
            vector_prefix(
                bytes,
                ascii_whitespace_lanes,
                scalar_ascii_whitespace_prefix,
            )
        }

        fn vector_prefix(
            bytes: &[u8],
            classify: unsafe fn(*const u8, *mut u8),
            scalar: fn(&[u8]) -> usize,
        ) -> usize {
            let mut offset = 0;
            while bytes.len() - offset >= WIDTH {
                let mut lanes = [0_u8; WIDTH];
                // SAFETY: both buffers contain at least `WIDTH` addressable bytes.
                unsafe { classify(bytes.as_ptr().add(offset), lanes.as_mut_ptr()) };
                if let Some(invalid) = lanes.iter().position(|&lane| lane == 0) {
                    return offset + invalid;
                }
                offset += WIDTH;
            }
            offset + scalar(&bytes[offset..])
        }

        unsafe fn identifier_lanes(pointer: *const u8, output: *mut u8) {
            let bytes = unsafe { vld1q_u8(pointer) };
            let lowercase = vorrq_u8(bytes, duplicate(0x20));
            let letters = vandq_u8(
                vcgeq_u8(lowercase, duplicate(b'a')),
                vcleq_u8(lowercase, duplicate(b'z')),
            );
            let digits = vandq_u8(
                vcgeq_u8(bytes, duplicate(b'0')),
                vcleq_u8(bytes, duplicate(b'9')),
            );
            let punctuation = vorrq_u8(
                vceqq_u8(bytes, duplicate(b'_')),
                vorrq_u8(
                    vceqq_u8(bytes, duplicate(b':')),
                    vceqq_u8(bytes, duplicate(b'-')),
                ),
            );
            let non_ascii = vcgeq_u8(bytes, duplicate(0x80));
            let valid = vorrq_u8(vorrq_u8(letters, digits), vorrq_u8(punctuation, non_ascii));
            unsafe { vst1q_u8(output, valid) };
        }

        unsafe fn ascii_whitespace_lanes(pointer: *const u8, output: *mut u8) {
            let bytes = unsafe { vld1q_u8(pointer) };
            let horizontal = vandq_u8(
                vcgeq_u8(bytes, duplicate(9)),
                vcleq_u8(bytes, duplicate(13)),
            );
            let horizontal = vbicq_u8(horizontal, vceqq_u8(bytes, duplicate(0x0B)));
            let spaces = vceqq_u8(bytes, duplicate(b' '));
            unsafe { vst1q_u8(output, vorrq_u8(horizontal, spaces)) };
        }

        fn duplicate(value: u8) -> uint8x16_t {
            // SAFETY: duplicating a byte has no preconditions.
            unsafe { std::arch::aarch64::vdupq_n_u8(value) }
        }
    }
}

fn matches_ignore_ascii_case(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

impl Default for ClassMapGenerator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_extract_class() {
        let gen = ClassMapGenerator::new();
        let content = r#"<?php
class MyClass {
}
"#;
        let classes = gen.extract_classes(content);
        assert_eq!(classes, vec!["MyClass"]);
    }

    #[test]
    fn test_extract_namespaced_class() {
        let gen = ClassMapGenerator::new();
        let content = r#"<?php
namespace Vendor\Package;

class MyClass {
}
"#;
        let classes = gen.extract_classes(content);
        assert_eq!(classes, vec!["Vendor\\Package\\MyClass"]);
    }

    #[test]
    fn test_extract_interface() {
        let gen = ClassMapGenerator::new();
        let content = r#"<?php
namespace App;

interface MyInterface {
}
"#;
        let classes = gen.extract_classes(content);
        assert_eq!(classes, vec!["App\\MyInterface"]);
    }

    #[test]
    fn test_extract_trait() {
        let gen = ClassMapGenerator::new();
        let content = r#"<?php
namespace App\Traits;

trait MyTrait {
}
"#;
        let classes = gen.extract_classes(content);
        assert_eq!(classes, vec!["App\\Traits\\MyTrait"]);
    }

    #[test]
    fn test_extract_enum() {
        let gen = ClassMapGenerator::new();
        let content = r#"<?php
namespace App\Enums;

enum Status {
    case Active;
    case Inactive;
}
"#;
        let classes = gen.extract_classes(content);
        assert_eq!(classes, vec!["App\\Enums\\Status"]);
    }

    #[test]
    fn test_extract_abstract_class() {
        let gen = ClassMapGenerator::new();
        let content = r#"<?php
namespace App;

abstract class AbstractBase {
}
"#;
        let classes = gen.extract_classes(content);
        assert_eq!(classes, vec!["App\\AbstractBase"]);
    }

    #[test]
    fn test_extract_final_class() {
        let gen = ClassMapGenerator::new();
        let content = r#"<?php
namespace App;

final class FinalClass {
}
"#;
        let classes = gen.extract_classes(content);
        assert_eq!(classes, vec!["App\\FinalClass"]);
    }

    #[test]
    fn extracts_modern_declarations_after_declare_and_comments() {
        let gen = ClassMapGenerator::new();
        let content = r#"<?php declare(strict_types=1);
/* class NotAClass {} */
namespace PHPUnit\TextUI;

#[SomeAttribute(Fixture::class)]
final readonly class Application {}
"#;
        assert_eq!(
            gen.extract_classes(content),
            vec!["PHPUnit\\TextUI\\Application"]
        );
    }

    #[test]
    fn extracts_declaration_after_nowdoc_attribute_argument() {
        let gen = ClassMapGenerator::new();
        let content = r#"<?php
namespace App\Command;

#[SomeAttribute(
    help: <<<'HELP'
        class NotADeclaration {}
        HELP,
)]
final class RealCommand {}
"#;

        assert_eq!(
            gen.extract_classes(content),
            vec!["App\\Command\\RealCommand"]
        );
    }

    #[test]
    fn simd_prefix_scanners_stop_at_every_vector_position() {
        for byte in u8::MIN..=u8::MAX {
            let bytes = [byte; 33];
            let identifier_expected = if simd_scan::is_identifier_continue(byte) {
                bytes.len()
            } else {
                0
            };
            let whitespace_expected = if byte.is_ascii_whitespace() {
                bytes.len()
            } else {
                0
            };
            assert_eq!(simd_scan::identifier_prefix(&bytes), identifier_expected);
            assert_eq!(
                simd_scan::ascii_whitespace_prefix(&bytes),
                whitespace_expected
            );
        }

        for invalid_at in 0..33 {
            let mut identifier = [b'a'; 33];
            identifier[invalid_at] = b'/';
            assert_eq!(simd_scan::identifier_prefix(&identifier), invalid_at);

            let mut whitespace = [b' '; 33];
            whitespace[invalid_at] = b'a';
            assert_eq!(simd_scan::ascii_whitespace_prefix(&whitespace), invalid_at);
        }

        assert_eq!(simd_scan::identifier_prefix(&[0xC3; 33]), 33);
        assert_eq!(
            simd_scan::identifier_prefix(b"Az09_:-identifier"),
            b"Az09_:-identifier".len()
        );
        assert_eq!(simd_scan::ascii_whitespace_prefix(b" \t\r\n\x0C"), 5);
        assert_eq!(simd_scan::ascii_whitespace_prefix(b"\x0B"), 0);
    }

    #[test]
    fn tracks_multiple_namespaces_and_conditional_declarations() {
        let gen = ClassMapGenerator::new();
        let content = r#"<?php
namespace Nette\Utils;
if (false) { interface IHtmlString {} }
namespace Nette\Localization;
if (false) { interface ITranslator {} }
namespace { enum GlobalStatus: string { case Ready = 'ready'; } }
"#;
        assert_eq!(
            gen.extract_classes(content),
            vec![
                "Nette\\Utils\\IHtmlString",
                "Nette\\Localization\\ITranslator",
                "GlobalStatus",
            ]
        );
    }

    #[test]
    fn ignores_anonymous_classes_class_constants_and_non_php_text() {
        let gen = ClassMapGenerator::new();
        let content = r#"class OutsidePhp {}
<?php
$one = new class {};
$two = new class extends ParentClass {};
$name = Existing::class;
$template = 'class InAString {}';
trait RealTrait {}
?>
class OutsideAgain {}
"#;
        assert_eq!(gen.extract_classes(content), vec!["RealTrait"]);
    }

    #[test]
    fn test_extract_one_line_namespaced_class() {
        let gen = ClassMapGenerator::new();
        let content = r#"<?php namespace App; final class OneLine {}"#;
        let classes = gen.extract_classes(content);
        assert_eq!(classes, vec!["App\\OneLine"]);
    }

    #[test]
    fn test_generate_from_directory() {
        let temp_dir = TempDir::new().unwrap();

        // Create test PHP file
        let php_content = r#"<?php
namespace Test;

class TestClass {
}
"#;
        let file_path = temp_dir.path().join("TestClass.php");
        fs::write(&file_path, php_content).unwrap();

        let gen = ClassMapGenerator::new();
        let classmap = gen.generate(temp_dir.path()).unwrap();

        assert_eq!(classmap.len(), 1);
        assert!(classmap.contains_key("Test\\TestClass"));
    }

    #[test]
    fn invalid_utf8_class_names_match_composer_replacement_character() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("Invalid.php");
        fs::write(&file_path, b"<?php class \xA9 {}").unwrap();

        let classmap = ClassMapGenerator::new().generate(temp_dir.path()).unwrap();
        assert_eq!(classmap.get("\u{e0a9}"), Some(&file_path));
    }

    #[test]
    fn test_is_php_file() {
        assert!(ClassMapGenerator::is_php_file(Path::new("test.php")));
        assert!(ClassMapGenerator::is_php_file(Path::new("test.PHP")));
        assert!(ClassMapGenerator::is_php_file(Path::new("legacy.inc")));
        assert!(ClassMapGenerator::is_php_file(Path::new("hack.hh")));
        assert!(!ClassMapGenerator::is_php_file(Path::new("test.txt")));
        assert!(!ClassMapGenerator::is_php_file(Path::new("test")));
    }

    #[test]
    fn test_generate_with_excludes() {
        let temp_dir = TempDir::new().unwrap();

        // Create src directory with a class
        let src_dir = temp_dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(
            src_dir.join("MyClass.php"),
            r#"<?php
namespace App;
class MyClass {}
"#,
        )
        .unwrap();

        // Create tests directory with a test class
        let tests_dir = temp_dir.path().join("tests");
        fs::create_dir_all(&tests_dir).unwrap();
        fs::write(
            tests_dir.join("MyClassTest.php"),
            r#"<?php
namespace App\Tests;
class MyClassTest {}
"#,
        )
        .unwrap();

        let gen = ClassMapGenerator::new();

        // Without excludes - should find both classes
        let classmap = gen.generate(temp_dir.path()).unwrap();
        assert_eq!(classmap.len(), 2);
        assert!(classmap.contains_key("App\\MyClass"));
        assert!(classmap.contains_key("App\\Tests\\MyClassTest"));

        // With exclude for tests directory
        let exclude_pattern = Regex::new(&format!(
            "{}/tests",
            temp_dir.path().to_string_lossy().replace('\\', "/")
        ))
        .unwrap();
        let classmap = gen
            .generate_with_excludes(temp_dir.path(), &[exclude_pattern])
            .unwrap();
        assert_eq!(classmap.len(), 1);
        assert!(classmap.contains_key("App\\MyClass"));
        assert!(!classmap.contains_key("App\\Tests\\MyClassTest"));
    }

    #[test]
    fn test_generate_with_wildcard_excludes() {
        let temp_dir = TempDir::new().unwrap();

        // Create files in various directories
        let src_dir = temp_dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("Class1.php"), "<?php\nclass Class1 {}\n").unwrap();

        let fixtures_dir = temp_dir.path().join("src").join("Fixtures");
        fs::create_dir_all(&fixtures_dir).unwrap();
        fs::write(
            fixtures_dir.join("TestFixture.php"),
            "<?php\nclass TestFixture {}\n",
        )
        .unwrap();

        let nested_fixtures = temp_dir.path().join("src").join("Sub").join("Fixtures");
        fs::create_dir_all(&nested_fixtures).unwrap();
        fs::write(
            nested_fixtures.join("NestedFixture.php"),
            "<?php\nclass NestedFixture {}\n",
        )
        .unwrap();

        let gen = ClassMapGenerator::new();

        // Without excludes - should find all 3 classes
        let classmap = gen.generate(temp_dir.path()).unwrap();
        assert_eq!(classmap.len(), 3);

        // With exclude for **/Fixtures/** (exclude all Fixtures directories)
        let pattern = format!(
            "{}/.*Fixtures",
            temp_dir.path().to_string_lossy().replace('\\', "/")
        );
        let exclude_pattern = Regex::new(&pattern).unwrap();
        let classmap = gen
            .generate_with_excludes(temp_dir.path(), &[exclude_pattern])
            .unwrap();
        assert_eq!(classmap.len(), 1);
        assert!(classmap.contains_key("Class1"));
    }
}
