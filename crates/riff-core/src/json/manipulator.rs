use std::cmp::Ordering;

use serde_json::{Map, Value};

/// A lossless editor for the parts of `composer.json` commonly changed by package-manager
/// commands.
///
/// Values are parsed to validate them, but edits are applied to byte ranges in the original
/// document. This deliberately preserves whitespace, key order, and unrelated nested objects.
#[derive(Debug, Clone)]
pub struct JsonManipulator {
    contents: String,
    newline: &'static str,
    indent: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ManipulatorError {
    #[error("the JSON document must have an object at its root")]
    RootMustBeObject,
    #[error("invalid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("expected {expected} at {path}")]
    UnexpectedType {
        path: String,
        expected: &'static str,
    },
    #[error("list index {index} is out of bounds for {path}")]
    InvalidIndex { path: String, index: usize },
}

#[derive(Debug, Clone)]
struct MemberSpan {
    key: String,
    key_start: usize,
    value_start: usize,
    value_end: usize,
}

#[derive(Debug, Clone, Copy)]
struct ValueSpan {
    start: usize,
    end: usize,
}

impl JsonManipulator {
    pub fn new(contents: &str) -> Result<Self, ManipulatorError> {
        let contents = contents.trim();
        let contents = if contents.is_empty() { "{}" } else { contents };
        let parsed: Value = serde_json::from_str(contents)?;
        if !parsed.is_object() {
            return Err(ManipulatorError::RootMustBeObject);
        }
        let newline = if contents.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let indent = detect_indent(contents).unwrap_or_else(|| "    ".to_owned());
        let contents = if contents == "{}" {
            format!("{{{newline}}}")
        } else {
            contents.to_owned()
        };
        Ok(Self {
            contents,
            newline,
            indent,
        })
    }

    /// Returns the edited document with the original newline convention and one final newline.
    pub fn contents(&self) -> String {
        format!("{}{}", self.contents.trim_end(), self.newline)
    }

    pub fn add_main_key(&mut self, key: &str, value: Value) -> Result<bool, ManipulatorError> {
        self.set_object_member(0, key, &value, false)?;
        Ok(true)
    }

    pub fn remove_main_key(&mut self, key: &str) -> Result<bool, ManipulatorError> {
        self.remove_object_member(0, key, false)?;
        if serde_json::from_str::<Map<String, Value>>(&self.contents)?.is_empty() {
            self.contents = format!("{{{}}}", self.newline);
        }
        Ok(true)
    }

    pub fn remove_main_key_if_empty(&mut self, key: &str) -> Result<bool, ManipulatorError> {
        let parsed: Value = serde_json::from_str(&self.contents)?;
        if parsed.get(key).is_some_and(|value| {
            matches!(value, Value::Array(v) if v.is_empty())
                || matches!(value, Value::Object(v) if v.is_empty())
        }) {
            self.remove_main_key(key)?;
        }
        Ok(true)
    }

    pub fn add_sub_node(
        &mut self,
        main_node: &str,
        name: &str,
        value: Value,
    ) -> Result<bool, ManipulatorError> {
        let parsed: Value = serde_json::from_str(&self.contents)?;
        if parsed
            .get(main_node)
            .is_some_and(|value| !value.is_object())
        {
            return Ok(false);
        }
        let mut path = vec![main_node.to_owned()];
        if matches!(main_node, "config" | "extra" | "scripts") {
            if let Some((head, tail)) = name.split_once('.') {
                path.push(head.to_owned());
                path.push(tail.to_owned());
            } else {
                path.push(name.to_owned());
            }
        } else {
            path.push(name.to_owned());
        }
        self.set_path(&path, value)?;
        Ok(true)
    }

    pub fn remove_sub_node(
        &mut self,
        main_node: &str,
        name: &str,
    ) -> Result<bool, ManipulatorError> {
        let parsed: Value = serde_json::from_str(&self.contents)?;
        if parsed
            .get(main_node)
            .is_some_and(|value| !value.is_object())
        {
            return Ok(false);
        }
        let mut path = vec![main_node.to_owned()];
        if matches!(main_node, "config" | "extra" | "scripts") {
            if let Some((head, tail)) = name.split_once('.') {
                path.push(head.to_owned());
                path.push(tail.to_owned());
            } else {
                path.push(name.to_owned());
            }
        } else {
            path.push(name.to_owned());
        }
        self.remove_path(&path, true)?;
        Ok(true)
    }

    pub fn add_config_setting(
        &mut self,
        name: &str,
        value: Value,
    ) -> Result<bool, ManipulatorError> {
        let path = if let Some(rest) = name.strip_prefix("policy.") {
            let Some((list, field)) = rest.split_once('.') else {
                return self.add_sub_node("config", name, value);
            };
            if field.contains('.') {
                return Ok(false);
            }
            vec!["config", "policy", list, field]
        } else if let Some((head, tail)) = name.split_once('.') {
            vec!["config", head, tail]
        } else {
            vec!["config", name]
        };
        self.set_path(
            &path.into_iter().map(str::to_owned).collect::<Vec<_>>(),
            value,
        )?;
        Ok(true)
    }

    pub fn remove_config_setting(&mut self, name: &str) -> Result<bool, ManipulatorError> {
        let policy_list = name
            .strip_prefix("policy.")
            .and_then(|rest| rest.split_once('.'))
            .map(|(list, _)| list.to_owned());
        let path = if let Some(rest) = name.strip_prefix("policy.") {
            let Some((list, field)) = rest.split_once('.') else {
                return self.remove_sub_node("config", name);
            };
            if field.contains('.') {
                return Ok(false);
            }
            vec!["config", "policy", list, field]
        } else if let Some((head, tail)) = name.split_once('.') {
            vec!["config", head, tail]
        } else {
            vec!["config", name]
        };
        self.remove_path(
            &path.into_iter().map(str::to_owned).collect::<Vec<_>>(),
            false,
        )?;
        if let Some(list) = policy_list {
            let parsed: Value = serde_json::from_str(&self.contents)?;
            if parsed["config"]["policy"][list.as_str()]
                .as_object()
                .is_some_and(Map::is_empty)
            {
                self.remove_path(&["config".to_owned(), "policy".to_owned(), list], true)?;
            }
        }
        Ok(true)
    }

    pub fn add_link(
        &mut self,
        link_type: &str,
        package: &str,
        constraint: &str,
        sort_packages: bool,
    ) -> Result<bool, ManipulatorError> {
        let parsed: Value = serde_json::from_str(&self.contents)?;
        let mut links = parsed
            .get(link_type)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let existing = links
            .keys()
            .find(|name| name.eq_ignore_ascii_case(package))
            .cloned();
        if let Some(existing) = existing {
            links.shift_remove(&existing);
        }
        links.insert(package.to_owned(), Value::String(constraint.to_owned()));
        if sort_packages {
            let mut entries = links.into_iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| package_compare(left, right));
            links = entries.into_iter().collect();
            self.set_path(&[link_type.to_owned()], Value::Object(links))?;
        } else {
            if self.find_path(&[link_type.to_owned()]).is_none() {
                self.set_path(&[link_type.to_owned()], Value::Object(Map::new()))?;
            }
            let span = self
                .find_path(&[link_type.to_owned()])
                .expect("link object was just ensured");
            let members = scan_object(&self.contents, span.start).unwrap_or_default();
            if let Some(member) = members
                .iter()
                .find(|member| member.key.eq_ignore_ascii_case(package))
            {
                let key_json =
                    serde_json::to_string(&member.key).expect("string serialization cannot fail");
                let value_json =
                    serde_json::to_string(constraint).expect("string serialization cannot fail");
                self.contents
                    .replace_range(member.value_start..member.value_end, &value_json);
                let key_end = scan_string(self.contents.as_bytes(), member.key_start)
                    .expect("member key is a JSON string");
                self.contents
                    .replace_range(member.key_start..key_end, &key_json);
            } else {
                self.set_object_member(
                    span.start,
                    package,
                    &Value::String(constraint.to_owned()),
                    false,
                )?;
            }
        }
        Ok(true)
    }

    pub fn remove_link(
        &mut self,
        link_type: &str,
        package: &str,
    ) -> Result<bool, ManipulatorError> {
        self.remove_sub_node(link_type, package)?;
        self.remove_main_key_if_empty(link_type)
    }

    pub fn add_list_item(
        &mut self,
        main_node: &str,
        value: Value,
        append: bool,
    ) -> Result<bool, ManipulatorError> {
        self.ensure_main_array(main_node)?;
        let span = self
            .find_path(&[main_node.to_owned()])
            .expect("array was just ensured");
        self.insert_array_value(span.start, value, if append { usize::MAX } else { 0 })?;
        Ok(true)
    }

    pub fn insert_list_item(
        &mut self,
        main_node: &str,
        value: Value,
        index: usize,
    ) -> Result<bool, ManipulatorError> {
        self.ensure_main_array(main_node)?;
        let span = self
            .find_path(&[main_node.to_owned()])
            .expect("array was just ensured");
        self.insert_array_value(span.start, value, index)?;
        Ok(true)
    }

    pub fn remove_list_item(
        &mut self,
        main_node: &str,
        index: usize,
    ) -> Result<bool, ManipulatorError> {
        let Some(span) = self.find_path(&[main_node.to_owned()]) else {
            return Ok(true);
        };
        self.remove_array_value(span.start, index)?;
        Ok(true)
    }

    pub fn add_repository(
        &mut self,
        name: &str,
        config: Value,
        append: bool,
    ) -> Result<bool, ManipulatorError> {
        self.remove_repository(name)?;
        self.repositories_to_list()?;
        let repository = repository_value(name, config);
        self.add_list_item("repositories", repository, append)
    }

    pub fn insert_repository(
        &mut self,
        name: &str,
        config: Value,
        reference_name: &str,
        offset: usize,
    ) -> Result<bool, ManipulatorError> {
        self.remove_repository(name)?;
        self.repositories_to_list()?;
        let parsed: Value = serde_json::from_str(&self.contents)?;
        let Some(repositories) = parsed.get("repositories").and_then(Value::as_array) else {
            return Ok(false);
        };
        let Some(index) = repositories.iter().position(|repository| {
            repository_name(repository).is_some_and(|n| n == reference_name)
        }) else {
            return Ok(false);
        };
        self.insert_list_item(
            "repositories",
            repository_value(name, config),
            index + offset,
        )
    }

    pub fn remove_repository(&mut self, name: &str) -> Result<bool, ManipulatorError> {
        let parsed: Value = serde_json::from_str(&self.contents)?;
        match parsed.get("repositories") {
            Some(Value::Object(repositories)) if repositories.contains_key(name) => {
                let Some(span) = self.find_path(&["repositories".to_owned()]) else {
                    return Ok(true);
                };
                self.remove_object_member(span.start, name, false)?;
            }
            Some(Value::Array(repositories)) => {
                if let Some(index) = repositories.iter().position(|repository| {
                    repository_name(repository).is_some_and(|repo_name| repo_name == name)
                }) {
                    self.remove_list_item("repositories", index)?;
                }
            }
            _ => {}
        }
        self.remove_main_key_if_empty("repositories")
    }

    pub fn set_repository_url(&mut self, name: &str, url: &str) -> Result<bool, ManipulatorError> {
        let parsed: Value = serde_json::from_str(&self.contents)?;
        let Some(repositories) = parsed.get("repositories") else {
            return Ok(false);
        };
        match repositories {
            Value::Object(entries) if entries.contains_key(name) => {
                self.set_path(
                    &["repositories".to_owned(), name.to_owned(), "url".to_owned()],
                    Value::String(url.to_owned()),
                )?;
            }
            Value::Array(entries) => {
                let Some(index) = entries.iter().position(|repository| {
                    repository_name(repository).is_some_and(|repo_name| repo_name == name)
                }) else {
                    return Ok(false);
                };
                let repo_span = self.repository_array_item_span(index)?;
                self.set_object_member(
                    repo_span.start,
                    "url",
                    &Value::String(url.to_owned()),
                    false,
                )?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn ensure_main_array(&mut self, main_node: &str) -> Result<(), ManipulatorError> {
        let parsed: Value = serde_json::from_str(&self.contents)?;
        match parsed.get(main_node) {
            None => {
                self.add_main_key(main_node, Value::Array(Vec::new()))?;
            }
            Some(Value::Array(_)) => {}
            Some(_) => {
                return Err(ManipulatorError::UnexpectedType {
                    path: main_node.to_owned(),
                    expected: "array",
                });
            }
        }
        Ok(())
    }

    fn repositories_to_list(&mut self) -> Result<(), ManipulatorError> {
        let parsed: Value = serde_json::from_str(&self.contents)?;
        let list = match parsed.get("repositories") {
            Some(Value::Object(repositories)) => repositories
                .iter()
                .map(|(name, repository)| repository_value(name, repository.clone()))
                .collect::<Vec<_>>(),
            Some(Value::Array(repositories)) if repositories.is_empty() => Vec::new(),
            None => {
                self.add_main_key("repositories", Value::Array(Vec::new()))?;
                Vec::new()
            }
            _ => return Ok(()),
        };
        let Some(span) = self.find_path(&["repositories".to_owned()]) else {
            return Ok(());
        };
        let key_indent = self.line_indent(span.start);
        let rendered = if list.is_empty() {
            format!("[{}{}]", self.newline, key_indent)
        } else {
            self.render_multiline_array(&list, &key_indent)
        };
        self.contents.replace_range(span.start..span.end, &rendered);
        Ok(())
    }

    fn repository_array_item_span(&self, index: usize) -> Result<ValueSpan, ManipulatorError> {
        let span = self
            .find_path(&["repositories".to_owned()])
            .ok_or_else(|| ManipulatorError::UnexpectedType {
                path: "repositories".to_owned(),
                expected: "array",
            })?;
        let items = scan_array(&self.contents, span.start).ok_or_else(|| {
            ManipulatorError::UnexpectedType {
                path: "repositories".to_owned(),
                expected: "array",
            }
        })?;
        items
            .get(index)
            .copied()
            .ok_or_else(|| ManipulatorError::InvalidIndex {
                path: "repositories".to_owned(),
                index,
            })
    }

    fn set_path(&mut self, path: &[String], value: Value) -> Result<(), ManipulatorError> {
        debug_assert!(!path.is_empty());
        let mut object_start = 0;
        for (index, key) in path.iter().enumerate() {
            let last = index + 1 == path.len();
            let members = scan_object(&self.contents, object_start).ok_or_else(|| {
                ManipulatorError::UnexpectedType {
                    path: path[..index].join("."),
                    expected: "object",
                }
            })?;
            if last {
                self.set_object_member(object_start, key, &value, false)?;
                return Ok(());
            }
            if let Some(member) = members.iter().find(|member| member.key == *key) {
                if self.contents.as_bytes().get(member.value_start) == Some(&b'{') {
                    object_start = member.value_start;
                } else {
                    let nested = nested_object(&path[index + 1..], value);
                    self.set_object_member(object_start, key, &nested, false)?;
                    return Ok(());
                }
            } else {
                let nested = nested_object(&path[index + 1..], value);
                self.set_object_member(object_start, key, &nested, false)?;
                return Ok(());
            }
        }
        Ok(())
    }

    fn remove_path(
        &mut self,
        path: &[String],
        preserve_empty_object: bool,
    ) -> Result<(), ManipulatorError> {
        if path.is_empty() {
            return Ok(());
        }
        let mut object_start = 0;
        for (index, key) in path.iter().enumerate() {
            let Some(members) = scan_object(&self.contents, object_start) else {
                return Ok(());
            };
            let Some(member) = members.iter().find(|member| member.key == *key) else {
                return Ok(());
            };
            if index + 1 == path.len() {
                self.remove_object_member(object_start, key, false)?;
                if preserve_empty_object
                    && scan_object(&self.contents, object_start).is_some_and(|m| m.is_empty())
                {
                    let close = scan_value(&self.contents, object_start).expect("valid object") - 1;
                    let close_indent = self.line_indent(close);
                    self.contents.replace_range(
                        object_start..=close,
                        &format!("{{{}{close_indent}}}", self.newline),
                    );
                }
                return Ok(());
            }
            if self.contents.as_bytes().get(member.value_start) != Some(&b'{') {
                return Ok(());
            }
            object_start = member.value_start;
        }
        Ok(())
    }

    fn find_path(&self, path: &[String]) -> Option<ValueSpan> {
        let mut object_start = 0;
        for (index, key) in path.iter().enumerate() {
            let members = scan_object(&self.contents, object_start)?;
            let member = members.iter().find(|member| member.key == *key)?;
            if index + 1 == path.len() {
                return Some(ValueSpan {
                    start: member.value_start,
                    end: member.value_end,
                });
            }
            object_start = member.value_start;
        }
        None
    }

    fn set_object_member(
        &mut self,
        object_start: usize,
        key: &str,
        value: &Value,
        case_insensitive: bool,
    ) -> Result<(), ManipulatorError> {
        let members = scan_object(&self.contents, object_start).ok_or_else(|| {
            ManipulatorError::UnexpectedType {
                path: key.to_owned(),
                expected: "object",
            }
        })?;
        if let Some(member) = members.iter().find(|member| {
            member.key == key || (case_insensitive && member.key.eq_ignore_ascii_case(key))
        }) {
            let key_indent = self.line_indent(member.key_start);
            let rendered = self.render_value(value, &key_indent);
            self.contents
                .replace_range(member.value_start..member.value_end, &rendered);
            return Ok(());
        }
        let object_end = scan_value(&self.contents, object_start).expect("valid object") - 1;
        let key_json = serde_json::to_string(key).expect("string serialization cannot fail");
        if let Some(last) = members.last() {
            let key_indent = self.line_indent(last.key_start);
            let rendered = self.render_value(value, &key_indent);
            self.contents.replace_range(
                last.value_end..last.value_end,
                &format!(",{}{key_indent}{key_json}: {rendered}", self.newline),
            );
        } else {
            let close_indent = self.line_indent(object_end);
            let key_indent = format!("{close_indent}{}", self.indent);
            let rendered = self.render_value(value, &key_indent);
            self.contents.replace_range(
                object_start + 1..object_end,
                &format!(
                    "{}{key_indent}{key_json}: {rendered}{}{close_indent}",
                    self.newline, self.newline
                ),
            );
        }
        Ok(())
    }

    fn remove_object_member(
        &mut self,
        object_start: usize,
        key: &str,
        case_insensitive: bool,
    ) -> Result<(), ManipulatorError> {
        let Some(members) = scan_object(&self.contents, object_start) else {
            return Ok(());
        };
        let Some(index) = members.iter().position(|member| {
            member.key == key || (case_insensitive && member.key.eq_ignore_ascii_case(key))
        }) else {
            return Ok(());
        };
        let member = &members[index];
        if index > 0 {
            self.contents
                .replace_range(members[index - 1].value_end..member.value_end, "");
        } else if let Some(next) = members.get(1) {
            self.contents
                .replace_range(member.key_start..next.key_start, "");
        } else {
            self.contents
                .replace_range(member.key_start..member.value_end, "");
        }
        Ok(())
    }

    fn insert_array_value(
        &mut self,
        array_start: usize,
        value: Value,
        index: usize,
    ) -> Result<(), ManipulatorError> {
        let items = scan_array(&self.contents, array_start).ok_or_else(|| {
            ManipulatorError::UnexpectedType {
                path: "list".to_owned(),
                expected: "array",
            }
        })?;
        let array_end = scan_value(&self.contents, array_start).expect("valid array") - 1;
        let index = if index == usize::MAX {
            items.len()
        } else {
            index
        };
        if index > items.len() {
            return Err(ManipulatorError::InvalidIndex {
                path: "list".to_owned(),
                index,
            });
        }
        let multiline = self.contents[array_start..array_end].contains(self.newline);
        let close_indent = self.line_indent(array_end);
        let item_indent = items
            .first()
            .map(|item| self.line_indent(item.start))
            .filter(|indent| !indent.is_empty())
            .unwrap_or_else(|| {
                if multiline {
                    format!("{close_indent}{}", self.indent)
                } else {
                    close_indent.clone()
                }
            });
        let rendered = self.render_value(&value, &item_indent);
        if items.is_empty() {
            let replacement = if multiline {
                format!(
                    "{}{item_indent}{rendered}{}{close_indent}",
                    self.newline, self.newline
                )
            } else {
                rendered
            };
            self.contents
                .replace_range(array_start + 1..array_end, &replacement);
        } else if index == items.len() {
            let separator = if multiline {
                format!(",{}{item_indent}", self.newline)
            } else {
                ", ".to_owned()
            };
            self.contents.replace_range(
                items[index - 1].end..items[index - 1].end,
                &(separator + &rendered),
            );
        } else if index == 0 {
            let separator = if multiline {
                format!(",{}{item_indent}", self.newline)
            } else {
                ", ".to_owned()
            };
            self.contents
                .replace_range(items[0].start..items[0].start, &(rendered + &separator));
        } else {
            let whitespace = if multiline {
                format!("{}{item_indent}", self.newline)
            } else {
                " ".to_owned()
            };
            self.contents.replace_range(
                items[index].start..items[index].start,
                &format!("{rendered},{whitespace}"),
            );
        }
        Ok(())
    }

    fn remove_array_value(
        &mut self,
        array_start: usize,
        index: usize,
    ) -> Result<(), ManipulatorError> {
        let Some(items) = scan_array(&self.contents, array_start) else {
            return Ok(());
        };
        let Some(item) = items.get(index) else {
            return Ok(());
        };
        if index > 0 {
            self.contents
                .replace_range(items[index - 1].end..item.end, "");
        } else if let Some(next) = items.get(1) {
            self.contents.replace_range(item.start..next.start, "");
        } else {
            self.contents.replace_range(item.start..item.end, "");
        }
        Ok(())
    }

    fn render_value(&self, value: &Value, key_indent: &str) -> String {
        match value {
            Value::Object(object) => {
                if object.is_empty() {
                    return format!("{{{}{key_indent}}}", self.newline);
                }
                let child_indent = format!("{key_indent}{}", self.indent);
                let members = object
                    .iter()
                    .map(|(key, value)| {
                        let key =
                            serde_json::to_string(key).expect("string serialization cannot fail");
                        format!(
                            "{child_indent}{key}: {}",
                            self.render_value(value, &child_indent)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(&format!(",{}", self.newline));
                let newline = self.newline;
                format!("{{{newline}{members}{newline}{key_indent}}}")
            }
            Value::Array(items) => format!(
                "[{}]",
                items
                    .iter()
                    .map(|value| self.render_value(value, key_indent))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            _ => serde_json::to_string(value).expect("JSON value serialization cannot fail"),
        }
    }

    fn render_multiline_array(&self, items: &[Value], key_indent: &str) -> String {
        if items.is_empty() {
            return "[]".to_owned();
        }
        let item_indent = format!("{key_indent}{}", self.indent);
        let rendered = items
            .iter()
            .map(|item| format!("{item_indent}{}", self.render_value(item, &item_indent)))
            .collect::<Vec<_>>()
            .join(&format!(",{}", self.newline));
        format!("[{}{rendered}{}{key_indent}]", self.newline, self.newline)
    }

    fn line_indent(&self, position: usize) -> String {
        let line_start = self.contents[..position]
            .rfind('\n')
            .map_or(0, |newline| newline + 1);
        self.contents[line_start..position]
            .chars()
            .take_while(|character| matches!(character, ' ' | '\t'))
            .collect()
    }
}

fn repository_value(name: &str, config: Value) -> Value {
    match config {
        Value::Object(config) if !name.is_empty() && name.parse::<usize>().is_err() => {
            let mut repository = Map::new();
            repository.insert("name".to_owned(), Value::String(name.to_owned()));
            repository.extend(config);
            Value::Object(repository)
        }
        Value::Bool(false) => {
            let mut repository = Map::new();
            repository.insert(name.to_owned(), Value::Bool(false));
            Value::Object(repository)
        }
        value => value,
    }
}

fn repository_name(repository: &Value) -> Option<&str> {
    let repository = repository.as_object()?;
    repository.get("name").and_then(Value::as_str).or_else(|| {
        (repository.len() == 1)
            .then(|| repository.iter().next())
            .flatten()
            .and_then(|(name, value)| (value == &Value::Bool(false)).then_some(name.as_str()))
    })
}

fn nested_object(path: &[String], value: Value) -> Value {
    path.iter().rev().fold(value, |value, key| {
        let mut object = Map::new();
        object.insert(key.clone(), value);
        Value::Object(object)
    })
}

fn detect_indent(contents: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let indent = line
            .chars()
            .take_while(|character| matches!(character, ' ' | '\t'))
            .collect::<String>();
        (!indent.is_empty() && line[indent.len()..].starts_with('"')).then_some(indent)
    })
}

fn package_compare(left: &str, right: &str) -> Ordering {
    natural_parts(&package_sort_key(left)).cmp(&natural_parts(&package_sort_key(right)))
}

fn package_sort_key(package: &str) -> String {
    if package == "php" || package.starts_with("php-") {
        format!("0-{package}")
    } else if package == "hhvm" || package.starts_with("hhvm-") {
        format!("1-{package}")
    } else if package.starts_with("ext-") {
        format!("2-{package}")
    } else if package.starts_with("lib-") {
        format!("3-{package}")
    } else {
        format!("5-{package}")
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum NaturalPart {
    Text(String),
    Number(u64),
}

fn natural_parts(value: &str) -> Vec<NaturalPart> {
    let mut parts = Vec::new();
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.peek().copied() {
        if character.is_ascii_digit() {
            let mut number = String::new();
            while chars.peek().is_some_and(char::is_ascii_digit) {
                number.push(chars.next().expect("peeked character exists"));
            }
            parts.push(NaturalPart::Number(number.parse().unwrap_or(u64::MAX)));
        } else {
            let mut text = String::new();
            while chars.peek().is_some_and(|next| !next.is_ascii_digit()) {
                text.push(chars.next().expect("peeked character exists"));
            }
            parts.push(NaturalPart::Text(text));
        }
    }
    parts
}

fn scan_object(contents: &str, start: usize) -> Option<Vec<MemberSpan>> {
    let bytes = contents.as_bytes();
    if bytes.get(start) != Some(&b'{') {
        return None;
    }
    let mut position = skip_ws(bytes, start + 1);
    let mut members = Vec::new();
    if bytes.get(position) == Some(&b'}') {
        return Some(members);
    }
    loop {
        let key_start = position;
        let key_end = scan_string(bytes, position)?;
        let key = serde_json::from_str(&contents[key_start..key_end]).ok()?;
        position = skip_ws(bytes, key_end);
        if bytes.get(position) != Some(&b':') {
            return None;
        }
        let value_start = skip_ws(bytes, position + 1);
        let value_end = scan_value(contents, value_start)?;
        members.push(MemberSpan {
            key,
            key_start,
            value_start,
            value_end,
        });
        position = skip_ws(bytes, value_end);
        match bytes.get(position) {
            Some(b',') => position = skip_ws(bytes, position + 1),
            Some(b'}') => return Some(members),
            _ => return None,
        }
    }
}

fn scan_array(contents: &str, start: usize) -> Option<Vec<ValueSpan>> {
    let bytes = contents.as_bytes();
    if bytes.get(start) != Some(&b'[') {
        return None;
    }
    let mut position = skip_ws(bytes, start + 1);
    let mut items = Vec::new();
    if bytes.get(position) == Some(&b']') {
        return Some(items);
    }
    loop {
        let end = scan_value(contents, position)?;
        items.push(ValueSpan {
            start: position,
            end,
        });
        position = skip_ws(bytes, end);
        match bytes.get(position) {
            Some(b',') => position = skip_ws(bytes, position + 1),
            Some(b']') => return Some(items),
            _ => return None,
        }
    }
}

fn scan_value(contents: &str, start: usize) -> Option<usize> {
    let bytes = contents.as_bytes();
    match bytes.get(start)? {
        b'"' => scan_string(bytes, start),
        b'{' => scan_container(contents, start, b'}'),
        b'[' => scan_container(contents, start, b']'),
        _ => {
            let mut position = start;
            while position < bytes.len()
                && !matches!(
                    bytes[position],
                    b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n'
                )
            {
                position += 1;
            }
            (position > start).then_some(position)
        }
    }
}

fn scan_container(contents: &str, start: usize, close: u8) -> Option<usize> {
    let bytes = contents.as_bytes();
    let mut stack = vec![close];
    let mut position = start + 1;
    while position < bytes.len() {
        match bytes[position] {
            b'"' => position = scan_string(bytes, position)?,
            b'{' => {
                stack.push(b'}');
                position += 1;
            }
            b'[' => {
                stack.push(b']');
                position += 1;
            }
            byte if Some(&byte) == stack.last() => {
                stack.pop();
                position += 1;
                if stack.is_empty() {
                    return Some(position);
                }
            }
            _ => position += 1,
        }
    }
    None
}

fn scan_string(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'"') {
        return None;
    }
    let mut position = start + 1;
    while position < bytes.len() {
        match bytes[position] {
            b'\\' => position += 2,
            b'"' => return Some(position + 1),
            _ => position += 1,
        }
    }
    None
}

fn skip_ws(bytes: &[u8], mut position: usize) -> usize {
    while bytes
        .get(position)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
    {
        position += 1;
    }
    position
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn composer_json_manipulator_adds_updates_and_removes_main_keys_losslessly() {
        let mut document = JsonManipulator::new("{\n  \"foo\": \"bar\"\n}").unwrap();
        document.add_main_key("bar", json!("$1 baz")).unwrap();
        assert_eq!(
            document.contents(),
            "{\n  \"foo\": \"bar\",\n  \"bar\": \"$1 baz\"\n}\n"
        );
        document
            .add_main_key("foo", json!({"nested": true}))
            .unwrap();
        assert_eq!(
            document.contents(),
            "{\n  \"foo\": {\n    \"nested\": true\n  },\n  \"bar\": \"$1 baz\"\n}\n"
        );
        document.remove_main_key("bar").unwrap();
        assert_eq!(
            document.contents(),
            "{\n  \"foo\": {\n    \"nested\": true\n  }\n}\n"
        );

        let mut empty = JsonManipulator::new("{}").unwrap();
        empty.add_main_key("foo", json!("$1bar")).unwrap();
        assert_eq!(empty.contents(), "{\n    \"foo\": \"$1bar\"\n}\n");

        let mut nested = JsonManipulator::new(
            "{\n    \"a\": {\"foo\": \"nested\"},\n    \"foo\": \"root\",\n    \"empty\": [],\n    \"null\": null\n}",
        )
        .unwrap();
        nested.add_main_key("foo", json!("updated")).unwrap();
        nested.remove_main_key_if_empty("empty").unwrap();
        nested.remove_main_key("null").unwrap();
        let parsed: Value = serde_json::from_str(&nested.contents()).unwrap();
        assert_eq!(parsed["a"]["foo"], "nested");
        assert_eq!(parsed["foo"], "updated");
        assert!(parsed.get("empty").is_none());
        assert!(parsed.get("null").is_none());
    }

    #[test]
    fn composer_json_manipulator_preserves_crlf_and_detected_indent() {
        let mut document = JsonManipulator::new("{\r\n\t\"foo\": \"bar\"\r\n}").unwrap();
        document.add_main_key("bar", json!("baz")).unwrap();
        assert_eq!(
            document.contents(),
            "{\r\n\t\"foo\": \"bar\",\r\n\t\"bar\": \"baz\"\r\n}\r\n"
        );
    }

    #[test]
    fn composer_json_manipulator_edits_only_the_selected_subnode() {
        let input = r#"{
    "repositories": [{
        "type": "package",
        "package": {
            "require": {"nested/pkg": "1.0"}
        }
    }],
    "require":
    {
        "vendor/old": "1.0"
    }
}"#;
        let mut document = JsonManipulator::new(input).unwrap();
        document
            .add_sub_node("require", "vendor/new", json!("^2.0"))
            .unwrap();
        document.remove_sub_node("require", "vendor/old").unwrap();
        assert!(document.contents().contains("\"nested/pkg\": \"1.0\""));
        assert!(document
            .contents()
            .contains("\"require\":\n    {\n        \"vendor/new\": \"^2.0\"\n    }"));

        let mut escaped = JsonManipulator::new(
            "{\n    \"repositories\": {\n        \"foo\\/bar\": {\"type\": \"vcs\"}\n    }\n}",
        )
        .unwrap();
        assert!(escaped.remove_sub_node("repositories", "foo/bar").unwrap());
        assert_eq!(escaped.contents(), "{\n    \"repositories\": {\n    }\n}\n");

        let mut wrong_type = JsonManipulator::new("{\n    \"repositories\": []\n}").unwrap();
        assert!(!wrong_type
            .remove_sub_node("repositories", "missing")
            .unwrap());
    }

    #[test]
    fn composer_json_manipulator_preserves_object_type_when_last_subnode_is_removed() {
        let mut document = JsonManipulator::new(
            "{\n    \"require\": {\n        \"vendor/package\": \"1.0\"\n    }\n}",
        )
        .unwrap();
        document
            .remove_sub_node("require", "vendor/package")
            .unwrap();
        assert_eq!(document.contents(), "{\n    \"require\": {\n    }\n}\n");
    }

    #[test]
    fn composer_json_manipulator_adds_links_case_insensitively_and_sorts_platform_first() {
        let mut document = JsonManipulator::new(
            r#"{
    "require": {
        "vendor\/Package": "1.0",
        "vendor/z": "1.0"
    }
}"#,
        )
        .unwrap();
        document
            .add_link("require", "vendor/package", "^2.0", false)
            .unwrap();
        assert!(document.contents().contains("\"vendor/Package\": \"^2.0\""));
        document
            .add_link("require", "ext-10gd", "*", false)
            .unwrap();
        document
            .add_link("require", "ext-2mcrypt", "*", false)
            .unwrap();
        document.add_link("require", "php", "^8.3", true).unwrap();
        let output = document.contents();
        assert!(output.find("\"php\"").unwrap() < output.find("\"ext-2mcrypt\"").unwrap());
        assert!(output.find("\"ext-2mcrypt\"").unwrap() < output.find("\"ext-10gd\"").unwrap());
        assert!(output.find("\"ext-10gd\"").unwrap() < output.find("\"vendor/Package\"").unwrap());
    }

    #[test]
    fn composer_json_manipulator_adds_extra_config_and_suggest_without_nested_collisions() {
        let mut document = JsonManipulator::new(
            r#"{
    "repositories": [{"type": "package", "package": {"extra": {"x": 1}}}]
}"#,
        )
        .unwrap();
        document.add_sub_node("extra", "x", json!(2)).unwrap();
        document
            .add_sub_node("suggest", "vendor/tool", json!("Useful"))
            .unwrap();
        document.add_config_setting("foo.bar", json!(true)).unwrap();
        let parsed: Value = serde_json::from_str(&document.contents()).unwrap();
        assert_eq!(parsed["extra"]["x"], 2);
        assert_eq!(parsed["suggest"]["vendor/tool"], "Useful");
        assert_eq!(parsed["config"]["foo"]["bar"], true);
        assert_eq!(parsed["repositories"][0]["package"]["extra"]["x"], 1);
    }

    #[test]
    fn composer_json_manipulator_surgically_edits_nested_policy_config() {
        let input = r#"{
    "name": "vendor/pkg",
    "config": {
        "vendor-dir": "vendor",
        "policy": {
            "advisories": {
                "audit": "report"
            },
            "abandoned": {"block": true}
        },
        "sort-packages": true
    }
}"#;
        let mut document = JsonManipulator::new(input).unwrap();
        document
            .add_config_setting("policy.advisories.block", json!(true))
            .unwrap();
        assert!(document.contents().contains(
            "\"advisories\": {\n                \"audit\": \"report\",\n                \"block\": true\n            }"
        ));
        assert!(document
            .contents()
            .contains("\"abandoned\": {\"block\": true}"));
        document
            .remove_config_setting("policy.advisories.audit")
            .unwrap();
        document
            .remove_config_setting("policy.advisories.absent")
            .unwrap();
        let parsed: Value = serde_json::from_str(&document.contents()).unwrap();
        assert_eq!(
            parsed["config"]["policy"]["advisories"],
            json!({"block": true})
        );

        let mut missing = JsonManipulator::new(
            "{\n    \"config\": {\n        \"policy\": {\n        }\n    }\n}",
        )
        .unwrap();
        missing
            .add_config_setting("policy.advisories.block", json!(false))
            .unwrap();
        let parsed: Value = serde_json::from_str(&missing.contents()).unwrap();
        assert_eq!(parsed["config"]["policy"]["advisories"]["block"], false);
    }

    #[test]
    fn composer_json_manipulator_adds_overwrites_and_removes_config_values() {
        let mut document = JsonManipulator::new("{\n    \"config\": {\n    }\n}").unwrap();
        document
            .add_config_setting("github-oauth.github.com", json!("token"))
            .unwrap();
        document
            .add_config_setting("escaped", json!("a\\b\nc\u{c}"))
            .unwrap();
        document
            .add_config_setting("github-protocols", json!(["https", "http"]))
            .unwrap();
        document
            .add_config_setting("process-timeout", json!(50))
            .unwrap();
        document
            .add_config_setting("github-oauth2.a.bar", json!("literal-tail"))
            .unwrap();
        document
            .add_config_setting("github-oauth.github.com", json!("new-token"))
            .unwrap();
        document
            .remove_config_setting("github-oauth.github.com")
            .unwrap();
        let parsed: Value = serde_json::from_str(&document.contents()).unwrap();
        assert_eq!(
            parsed["config"]["github-protocols"],
            json!(["https", "http"])
        );
        assert_eq!(parsed["config"]["github-oauth"], json!({}));
        assert_eq!(parsed["config"]["escaped"], "a\\b\nc\u{c}");
        assert_eq!(parsed["config"]["process-timeout"], 50);
        assert_eq!(parsed["config"]["github-oauth2"]["a.bar"], "literal-tail");

        let mut dotted_root = JsonManipulator::new(
            "{\n    \"github-oauth\": {\n        \"github.com\": \"token\"\n    }\n}",
        )
        .unwrap();
        dotted_root
            .add_sub_node("github-oauth", "bar", json!("baz"))
            .unwrap();
        let parsed: Value = serde_json::from_str(&dotted_root.contents()).unwrap();
        assert_eq!(parsed["github-oauth"]["github.com"], "token");
        assert_eq!(parsed["github-oauth"]["bar"], "baz");
    }

    #[test]
    fn composer_json_manipulator_preserves_single_and_multiline_list_layout() {
        let mut single = JsonManipulator::new("{\n    \"main\": [ 1 ]\n}").unwrap();
        single.add_list_item("main", json!(2), true).unwrap();
        single.insert_list_item("main", json!(0), 0).unwrap();
        assert_eq!(single.contents(), "{\n    \"main\": [ 0, 1, 2 ]\n}\n");

        let mut multi = JsonManipulator::new("{\n    \"main\": [\n        1\n    ]\n}").unwrap();
        multi.add_list_item("main", json!(2), true).unwrap();
        assert_eq!(
            multi.contents(),
            "{\n    \"main\": [\n        1,\n        2\n    ]\n}\n"
        );
        multi.remove_list_item("main", 0).unwrap();
        assert_eq!(
            multi.contents(),
            "{\n    \"main\": [\n        2\n    ]\n}\n"
        );

        let mut objects = JsonManipulator::new("{}").unwrap();
        objects
            .insert_list_item("main", json!({"value": 1}), 0)
            .unwrap();
        assert_eq!(
            objects.contents(),
            "{\n    \"main\": [{\n        \"value\": 1\n    }]\n}\n"
        );

        let mut positions = JsonManipulator::new("{\n    \"main\": [1, 2, 3]\n}").unwrap();
        positions.remove_list_item("main", 1).unwrap();
        assert_eq!(positions.contents(), "{\n    \"main\": [1, 3]\n}\n");
        positions.remove_list_item("main", 1).unwrap();
        assert_eq!(positions.contents(), "{\n    \"main\": [1]\n}\n");
    }

    #[test]
    fn composer_json_manipulator_initializes_and_normalizes_repositories() {
        let mut document = JsonManipulator::new(
            r#"{
    "repositories": {
        "baz": {"type": "package", "package": {}},
        "packagist.org": false
    }
}"#,
        )
        .unwrap();
        document
            .add_repository("foo", json!({"type": "composer"}), true)
            .unwrap();
        let parsed: Value = serde_json::from_str(&document.contents()).unwrap();
        assert_eq!(parsed["repositories"][0]["name"], "baz");
        assert_eq!(parsed["repositories"][1], json!({"packagist.org": false}));
        assert_eq!(parsed["repositories"][2]["name"], "foo");
        assert_eq!(parsed["repositories"][2]["type"], "composer");

        document
            .add_repository("baz", json!({"type": "composer"}), true)
            .unwrap();
        let parsed: Value = serde_json::from_str(&document.contents()).unwrap();
        let baz = parsed["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .find(|repository| repository["name"] == "baz")
            .unwrap();
        assert_eq!(baz["type"], "composer");
        assert!(baz.get("package").is_none());

        let mut scratch = JsonManipulator::new("{}").unwrap();
        scratch
            .add_repository("bar", json!({"type": "composer"}), true)
            .unwrap();
        let parsed: Value = serde_json::from_str(&scratch.contents()).unwrap();
        assert_eq!(parsed["repositories"][0]["name"], "bar");
    }

    #[test]
    fn composer_json_manipulator_prepends_inserts_updates_and_removes_repositories() {
        let mut document = JsonManipulator::new(
            r#"{
    "repositories": [
        {"name": "alpha", "type": "vcs", "url": "old"},
        {"name": "omega", "type": "vcs", "url": "other"}
    ]
}"#,
        )
        .unwrap();
        document
            .add_repository("first", json!({"type": "composer"}), false)
            .unwrap();
        document
            .insert_repository("beta", json!({"type": "vcs", "url": "b"}), "omega", 0)
            .unwrap();
        document
            .insert_repository("gamma", json!({"type": "vcs", "url": "g"}), "alpha", 1)
            .unwrap();
        document.set_repository_url("alpha", "new").unwrap();
        document.remove_repository("omega").unwrap();
        let parsed: Value = serde_json::from_str(&document.contents()).unwrap();
        let repositories = parsed["repositories"].as_array().unwrap();
        assert_eq!(repositories[0]["name"], "first");
        assert_eq!(repositories[1]["name"], "alpha");
        assert_eq!(repositories[1]["url"], "new");
        assert_eq!(repositories[2]["name"], "gamma");
        assert_eq!(repositories[3]["name"], "beta");
        assert_eq!(repositories.len(), 4);

        let mut associative = JsonManipulator::new(
            "{\n    \"repositories\": {\n        \"alpha\": {\"type\": \"vcs\", \"url\": \"old\"},\n        \"keep\": false\n    }\n}",
        )
        .unwrap();
        associative.set_repository_url("alpha", "new").unwrap();
        associative.remove_repository("alpha").unwrap();
        assert_eq!(
            associative.contents(),
            "{\n    \"repositories\": {\n        \"keep\": false\n    }\n}\n"
        );
    }

    const JSON_CONFIG_SOURCE_REPOSITORIES: &str = r#"{
    "name": "my-vend/my-app",
    "license": "MIT",
    "repositories": {
    }
}"#;

    // Ported from Composer\Test\Config\JsonConfigSourceTest::testAddRepository.
    #[test]
    fn composer_json_config_source_adds_a_named_repository() {
        let mut document = JsonManipulator::new(JSON_CONFIG_SOURCE_REPOSITORIES).unwrap();
        document
            .add_repository(
                "example_tld",
                json!({"type": "git", "url": "example.tld"}),
                true,
            )
            .unwrap();

        assert_eq!(
            document.contents(),
            r#"{
    "name": "my-vend/my-app",
    "license": "MIT",
    "repositories": [
        {
            "name": "example_tld",
            "type": "git",
            "url": "example.tld"
        }
    ]
}
"#
        );
    }

    // Ported from Composer\Test\Config\JsonConfigSourceTest::testAddRepositoryAsList.
    #[test]
    fn composer_json_config_source_adds_an_anonymous_repository() {
        let mut document = JsonManipulator::new(JSON_CONFIG_SOURCE_REPOSITORIES).unwrap();
        document
            .add_repository("", json!({"type": "git", "url": "example.tld"}), true)
            .unwrap();

        let parsed: Value = serde_json::from_str(&document.contents()).unwrap();
        assert_eq!(
            parsed["repositories"],
            json!([{"type": "git", "url": "example.tld"}])
        );
        assert!(document.contents().contains(
            "\"repositories\": [\n        {\n            \"type\": \"git\",\n            \"url\": \"example.tld\"\n        }\n    ]"
        ));
    }

    // Ported from Composer\Test\Config\JsonConfigSourceTest::testAddRepositoryWithOptions.
    #[test]
    fn composer_json_config_source_preserves_nested_repository_options() {
        let mut document = JsonManipulator::new(JSON_CONFIG_SOURCE_REPOSITORIES).unwrap();
        document
            .add_repository(
                "example_tld",
                json!({
                    "type": "composer",
                    "url": "https://example.tld",
                    "options": {"ssl": {"local_cert": "/home/composer/.ssl/composer.pem"}}
                }),
                true,
            )
            .unwrap();

        let parsed: Value = serde_json::from_str(&document.contents()).unwrap();
        assert_eq!(parsed["repositories"][0]["name"], "example_tld");
        assert_eq!(
            parsed["repositories"][0]["options"]["ssl"]["local_cert"],
            "/home/composer/.ssl/composer.pem"
        );
        assert!(document
            .contents()
            .contains("                    \"local_cert\": \"/home/composer/.ssl/composer.pem\""));
    }

    // Ported from Composer\Test\Config\JsonConfigSourceTest::testRemoveRepository.
    #[test]
    fn composer_json_config_source_removes_the_last_named_repository_section() {
        let mut document = JsonManipulator::new(
            r#"{
    "name": "my-vend/my-app",
    "license": "MIT",
    "repositories": [
        {
            "name": "example_tld",
            "type": "git",
            "url": "example.tld"
        }
    ]
}"#,
        )
        .unwrap();
        document.remove_repository("example_tld").unwrap();

        assert_eq!(
            document.contents(),
            "{\n    \"name\": \"my-vend/my-app\",\n    \"license\": \"MIT\"\n}\n"
        );
    }

    // Ported from Composer\Test\Config\JsonConfigSourceTest::
    // testAddPackagistRepositoryWithFalseValue.
    #[test]
    fn composer_json_config_source_adds_a_disabled_packagist_repository() {
        let mut document = JsonManipulator::new(JSON_CONFIG_SOURCE_REPOSITORIES).unwrap();
        document
            .add_repository("packagist", Value::Bool(false), true)
            .unwrap();

        let parsed: Value = serde_json::from_str(&document.contents()).unwrap();
        assert_eq!(parsed["repositories"], json!([{"packagist": false}]));
        assert!(document.contents().contains(
            "\"repositories\": [\n        {\n            \"packagist\": false\n        }\n    ]"
        ));
    }

    // Ported from Composer\Test\Config\JsonConfigSourceTest::testRemovePackagist.
    #[test]
    fn composer_json_config_source_removes_a_disabled_packagist_repository() {
        let mut document = JsonManipulator::new(
            r#"{
    "name": "my-vend/my-app",
    "license": "MIT",
    "repositories": [
        {
            "packagist": false
        }
    ]
}"#,
        )
        .unwrap();
        document.remove_repository("packagist").unwrap();

        assert_eq!(
            document.contents(),
            "{\n    \"name\": \"my-vend/my-app\",\n    \"license\": \"MIT\"\n}\n"
        );
    }

    // Ported from Composer\Test\Config\JsonConfigSourceTest::
    // testAddPolicyListFieldPreservesFormatting.
    #[test]
    fn composer_json_config_source_adds_a_policy_field_losslessly() {
        let original = r#"{
    "name": "vendor/pkg", "config": {
        "vendor-dir": "vendor", "sort-packages": true,
        "policy": {
            "advisories": {
                "audit": "report"
            },
            "abandoned": {
                "block": true
            }
        }
    }
}"#;
        let mut document = JsonManipulator::new(original).unwrap();
        document
            .add_config_setting("policy.advisories.block", json!(true))
            .unwrap();

        assert_eq!(
            document.contents(),
            r#"{
    "name": "vendor/pkg", "config": {
        "vendor-dir": "vendor", "sort-packages": true,
        "policy": {
            "advisories": {
                "audit": "report",
                "block": true
            },
            "abandoned": {
                "block": true
            }
        }
    }
}
"#
        );
    }

    // Ported from Composer\Test\Config\JsonConfigSourceTest::
    // testRemovePolicyListFieldPreservesFormatting.
    #[test]
    fn composer_json_config_source_removes_a_policy_field_losslessly() {
        let original = r#"{
    "name": "vendor/pkg", "config": {
        "vendor-dir": "vendor", "sort-packages": true,
        "policy": {
            "advisories": {
                "audit": "report",
                "block": true
            }
        }
    }
}"#;
        let mut document = JsonManipulator::new(original).unwrap();
        document
            .remove_config_setting("policy.advisories.audit")
            .unwrap();

        assert_eq!(
            document.contents(),
            r#"{
    "name": "vendor/pkg", "config": {
        "vendor-dir": "vendor", "sort-packages": true,
        "policy": {
            "advisories": {
                "block": true
            }
        }
    }
}
"#
        );
    }

    // Ported from Composer\Test\Config\JsonConfigSourceTest::
    // testRemovePolicyListFieldCascadesEmptyAncestors.
    #[test]
    fn composer_json_config_source_prunes_an_empty_policy_list_losslessly() {
        let original = r#"{
    "name": "vendor/pkg", "config": {
        "vendor-dir": "vendor", "sort-packages": true,
        "policy": {
            "advisories": {
                "block": true
            }
        }
    }
}"#;
        let mut document = JsonManipulator::new(original).unwrap();
        document
            .remove_config_setting("policy.advisories.block")
            .unwrap();

        assert_eq!(
            document.contents(),
            r#"{
    "name": "vendor/pkg", "config": {
        "vendor-dir": "vendor", "sort-packages": true,
        "policy": {
        }
    }
}
"#
        );
    }

    fn json_config_source_link_document(entries_per_type: usize) -> String {
        let types = [
            ("require", "my-vend/my-other-lib"),
            ("require-dev", "my-vend/my-other-lib-tests"),
            ("provide", "my-vend/my-other-interface"),
            ("suggest", "my-vend/my-other-optional-extension"),
            ("replace", "other-vend/other-app"),
            ("conflict", "my-vend/my-other-old-app"),
        ];
        let mut root = Map::from_iter([
            ("name".to_owned(), json!("my-vend/my-app")),
            ("license".to_owned(), json!("MIT")),
        ]);
        for (link_type, first) in types {
            let mut links = Map::new();
            if entries_per_type > 0 {
                links.insert(first.to_owned(), json!("1.*"));
            }
            if entries_per_type > 1 {
                links.insert(first.replacen("other", "yet-another", 1), json!("1.*"));
            }
            if !links.is_empty() {
                root.insert(link_type.to_owned(), Value::Object(links));
            }
        }
        serde_json::to_string_pretty(&Value::Object(root)).unwrap()
    }

    // Ported from Composer\Test\Config\JsonConfigSourceTest::testAddLink.
    #[test]
    fn composer_json_config_source_adds_all_link_types_to_each_manifest_shape() {
        for (link_type, package) in [
            ("require", "my-vend/my-lib"),
            ("require-dev", "my-vend/my-lib-tests"),
            ("provide", "my-vend/my-lib-interface"),
            ("suggest", "my-vend/my-optional-extension"),
            ("replace", "my-vend/other-app"),
            ("conflict", "my-vend/my-old-app"),
        ] {
            for existing in 0..=2 {
                let mut document =
                    JsonManipulator::new(&json_config_source_link_document(existing)).unwrap();
                document.add_link(link_type, package, "1.*", false).unwrap();
                let parsed: Value = serde_json::from_str(&document.contents()).unwrap();
                assert_eq!(parsed[link_type][package], "1.*");
                assert_eq!(parsed[link_type].as_object().unwrap().len(), existing + 1);
                assert_eq!(parsed["name"], "my-vend/my-app");
                assert_eq!(parsed["license"], "MIT");
            }
        }
    }

    // Ported from Composer\Test\Config\JsonConfigSourceTest::testRemoveLink.
    #[test]
    fn composer_json_config_source_removes_all_link_types_and_empty_sections() {
        for (link_type, package) in [
            ("require", "my-vend/my-lib"),
            ("require-dev", "my-vend/my-lib-tests"),
            ("provide", "my-vend/my-lib-interface"),
            ("suggest", "my-vend/my-optional-extension"),
            ("replace", "my-vend/other-app"),
            ("conflict", "my-vend/my-old-app"),
        ] {
            for remaining in 0..=2 {
                let mut document =
                    JsonManipulator::new(&json_config_source_link_document(remaining)).unwrap();
                document.add_link(link_type, package, "1.*", false).unwrap();
                document.remove_link(link_type, package).unwrap();
                let parsed: Value = serde_json::from_str(&document.contents()).unwrap();
                if remaining == 0 {
                    assert!(parsed.get(link_type).is_none());
                } else {
                    assert_eq!(parsed[link_type].as_object().unwrap().len(), remaining);
                }
                assert_eq!(parsed["name"], "my-vend/my-app");
                assert_eq!(parsed["license"], "MIT");
            }
        }
    }

    #[test]
    fn composer_json_manipulator_scanner_handles_escaped_unicode_and_large_documents_linearly() {
        let padding = "x".repeat(250_000);
        let input = format!(
            "{{\n    \"description\": \"{padding} \\uD83D\\uDE00\",\n    \"require\": {{}}\n}}"
        );
        let mut document = JsonManipulator::new(&input).unwrap();
        document
            .add_link("require", "vendor/package", "^1.0", false)
            .unwrap();
        let output = document.contents();
        assert!(output.contains("\\uD83D\\uDE00"));
        assert!(output.contains("\"vendor/package\": \"^1.0\""));
    }
}
