use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value};
use walkdir::WalkDir;

use super::lock::FlexLock;
use super::recipe::{Recipe, RecipeJob};
use super::FlexOptions;

pub(crate) struct Configurator<'a> {
    working_dir: &'a Path,
    options: FlexOptions,
    vendor_dir: PathBuf,
    installed_packages: HashSet<String>,
    force: bool,
    output: crate::output::Output,
}

pub(crate) struct RenderedRecipe {
    pub(crate) files: BTreeMap<String, Option<Vec<u8>>>,
    pub(crate) lock_entry: Value,
    pub(crate) copies_from_package: bool,
}

impl<'a> Configurator<'a> {
    pub(crate) fn new(
        working_dir: &'a Path,
        manifest: &crate::json::RiffManifest,
        vendor_dir: PathBuf,
        installed_packages: impl IntoIterator<Item = String>,
        force: bool,
    ) -> Self {
        Self {
            working_dir,
            options: FlexOptions::from_manifest(manifest),
            vendor_dir,
            installed_packages: installed_packages.into_iter().collect(),
            force,
            output: crate::output::Output::silent(),
        }
    }

    pub(crate) fn with_output(mut self, output: crate::output::Output) -> Self {
        self.output = output;
        self
    }

    pub(crate) fn apply(&self, recipes: &[Recipe], lock: &mut FlexLock) -> Result<()> {
        let allow_contrib = allow_contrib(self.working_dir)?;
        let mut post_install = Vec::new();
        for recipe in recipes {
            if recipe.job == RecipeJob::Install && recipe.is_contrib && !allow_contrib {
                crate::warnln!(
                    self.output,
                    "Warning: Ignoring community recipe {} in non-interactive mode; set extra.symfony.allow-contrib to true to allow it",
                    recipe.name
                );
                lock.set(
                    recipe.name.clone(),
                    serde_json::json!({"version": package_version(recipe.package.pretty_version())}),
                );
                continue;
            }
            match recipe.job {
                RecipeJob::Install => {
                    crate::outln!(self.output, "  - Configuring {}", recipe_origin(recipe));
                    lock.set(recipe.name.clone(), recipe.lock.clone());
                    self.install(recipe, lock)?;
                    post_install.push(recipe);
                }
                RecipeJob::Uninstall => {
                    crate::outln!(self.output, "  - Unconfiguring {}", recipe_origin(recipe));
                    self.uninstall(recipe, lock)?;
                    lock.remove(&recipe.name);
                }
            }
        }
        for recipe in post_install {
            if let Some(config) = recipe.manifest.get("add-lines") {
                self.configure_add_lines(recipe, config, false)?;
            }
            if let Some(lines) = recipe
                .manifest
                .get("post-install-output")
                .and_then(Value::as_array)
            {
                crate::outln!(self.output, "");
                crate::outln!(self.output, "{} instructions:", recipe.name);
                for line in lines.iter().filter_map(Value::as_str) {
                    crate::outln!(self.output, "{}", self.options.expand(line));
                }
            }
        }
        Ok(())
    }

    fn install(&self, recipe: &Recipe, lock: &mut FlexLock) -> Result<()> {
        for key in [
            "bundles",
            "copy-from-recipe",
            "copy-from-package",
            "env",
            "dotenv",
            "container",
            "makefile",
            "composer-scripts",
            "composer-commands",
            "gitignore",
            "dockerfile",
            "docker-compose",
        ] {
            let Some(config) = recipe.manifest.get(key) else {
                continue;
            };
            match key {
                "bundles" => self.configure_bundles(config, false)?,
                "copy-from-recipe" => {
                    lock.add_files(&recipe.name, self.copy_from_recipe(recipe, config)?);
                }
                "copy-from-package" => self.copy_from_package(recipe, config, false)?,
                "env" => self.configure_env(recipe, config, "", false)?,
                "dotenv" => {
                    for (suffix, vars) in object(config, "dotenv")? {
                        self.configure_env(recipe, vars, suffix, false)?;
                    }
                }
                "container" => self.configure_container(config, false)?,
                "makefile" => self.configure_marked_file(
                    recipe,
                    &self.root().join("Makefile"),
                    lines(config)?,
                    false,
                )?,
                "composer-scripts" => self.configure_composer_scripts(config, false, true)?,
                "composer-commands" => self.configure_composer_scripts(config, false, false)?,
                "gitignore" => self.configure_marked_file(
                    recipe,
                    &self.root().join(".gitignore"),
                    lines(config)?,
                    false,
                )?,
                "dockerfile" if docker_enabled(self.working_dir)? => {
                    self.configure_dockerfile(recipe, config, false)?
                }
                "docker-compose" if docker_enabled(self.working_dir)? => {
                    self.configure_docker_compose(recipe, config, false)?
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn uninstall(&self, recipe: &Recipe, lock: &FlexLock) -> Result<()> {
        for (key, config) in &recipe.manifest {
            match key.as_str() {
                "bundles" => self.configure_bundles(config, true)?,
                "copy-from-recipe" => self.remove_recipe_files(recipe, lock)?,
                "copy-from-package" => self.copy_from_package(recipe, config, true)?,
                "env" => self.configure_env(recipe, config, "", true)?,
                "dotenv" => {
                    for (suffix, vars) in object(config, "dotenv")? {
                        self.configure_env(recipe, vars, suffix, true)?;
                    }
                }
                "container" => self.configure_container(config, true)?,
                "makefile" => remove_marked_section(&self.root().join("Makefile"), &recipe.name)?,
                "composer-scripts" => self.configure_composer_scripts(config, true, true)?,
                "composer-commands" => self.configure_composer_scripts(config, true, false)?,
                "gitignore" => {
                    remove_marked_section(&self.root().join(".gitignore"), &recipe.name)?
                }
                "dockerfile" => {
                    remove_marked_section(&self.root().join("Dockerfile"), &recipe.name)?
                }
                "docker-compose" => self.configure_docker_compose(recipe, config, true)?,
                "add-lines" => self.configure_add_lines(recipe, config, true)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn root(&self) -> PathBuf {
        if self.options.root_dir.is_absolute() {
            self.options.root_dir.clone()
        } else {
            self.working_dir.join(&self.options.root_dir)
        }
    }

    fn target(&self, target: &str) -> Result<PathBuf> {
        safe_join(&self.root(), &self.options.expand(target))
    }

    fn copy_from_recipe(&self, recipe: &Recipe, config: &Value) -> Result<Vec<String>> {
        let mut copied = Vec::new();
        for (source, target) in object(config, "copy-from-recipe")? {
            let target = self.options.expand(string(target, "copy target")?);
            if source.ends_with('/') {
                for (file, data) in &recipe.files {
                    if let Some(suffix) = file.strip_prefix(source) {
                        let destination = safe_join(&self.root(), &format!("{target}{suffix}"))?;
                        copied.push(relative_path(&self.root(), &destination));
                        write_recipe_file(
                            &destination,
                            &self.options.expand(&data.contents),
                            data.executable,
                            self.force,
                        )?;
                    }
                }
            } else {
                let data = recipe.files.get(source).with_context(|| {
                    format!("Recipe {} does not contain file {source}", recipe.name)
                })?;
                let destination = safe_join(&self.root(), &target)?;
                copied.push(relative_path(&self.root(), &destination));
                write_recipe_file(
                    &destination,
                    &self.options.expand(&data.contents),
                    data.executable,
                    self.force,
                )?;
            }
        }
        Ok(copied)
    }

    fn remove_recipe_files(&self, recipe: &Recipe, lock: &FlexLock) -> Result<()> {
        let Some(files) = lock
            .get(&recipe.name)
            .and_then(|entry| entry.get("files"))
            .and_then(Value::as_array)
        else {
            return Ok(());
        };
        let referenced = lock
            .all()
            .iter()
            .filter(|(name, _)| *name != &recipe.name)
            .filter_map(|(_, entry)| entry.get("files").and_then(Value::as_array))
            .flatten()
            .filter_map(Value::as_str)
            .collect::<HashSet<_>>();
        for file in files.iter().filter_map(Value::as_str) {
            if referenced.contains(file) || file == ".git" {
                continue;
            }
            let target = safe_join(&self.root(), file)?;
            if target.is_file() || target.is_symlink() {
                std::fs::remove_file(&target)
                    .with_context(|| format!("Failed to remove {}", target.display()))?;
            }
            remove_empty_parents(target.parent(), &self.root());
        }
        Ok(())
    }

    fn copy_from_package(&self, recipe: &Recipe, config: &Value, remove: bool) -> Result<()> {
        let package_dir = self.vendor_dir.join(&recipe.name);
        for (source, target) in object(config, "copy-from-package")? {
            let source_path = safe_join(&package_dir, source)?;
            let target = self.options.expand(string(target, "copy target")?);
            if source.ends_with('/') {
                if !source_path.is_dir() {
                    continue;
                }
                for item in WalkDir::new(&source_path)
                    .into_iter()
                    .filter_map(Result::ok)
                {
                    if !item.file_type().is_file() {
                        continue;
                    }
                    let suffix = item.path().strip_prefix(&source_path)?;
                    let destination = safe_join(
                        &self.root(),
                        &Path::new(&target).join(suffix).to_string_lossy(),
                    )?;
                    copy_package_file(
                        item.path(),
                        &destination,
                        remove,
                        &self.options,
                        self.force,
                    )?;
                }
            } else {
                let destination = safe_join(&self.root(), &target)?;
                copy_package_file(
                    &source_path,
                    &destination,
                    remove,
                    &self.options,
                    self.force,
                )?;
            }
        }
        Ok(())
    }

    fn configure_composer_scripts(&self, config: &Value, remove: bool, auto: bool) -> Result<()> {
        let path = self.working_dir.join("composer.json");
        let mut json: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        let original = json.clone();
        let scripts = json
            .as_object_mut()
            .context("composer.json must be an object")?
            .entry("scripts")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .context("composer.json scripts must be an object")?;
        let target = if auto {
            scripts
                .entry("auto-scripts")
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .context("scripts.auto-scripts must be an object")?
        } else {
            scripts
        };
        for (name, command) in object(config, "composer scripts")? {
            if remove {
                target.remove(name);
            } else {
                target.insert(name.clone(), command.clone());
            }
        }
        if json == original {
            return Ok(());
        }
        crate::json::write_json_value(&path, &json, true)?;
        Ok(())
    }

    fn configure_bundles(&self, config: &Value, remove: bool) -> Result<()> {
        let path = self.target("%CONFIG_DIR%/bundles.php")?;
        let mut bundles = if path.is_file() {
            parse_bundles(&std::fs::read_to_string(&path)?)
        } else {
            IndexMap::new()
        };
        for (class, envs) in object(config, "bundles")? {
            let class = class.trim_start_matches('\\').to_owned();
            if remove {
                bundles.shift_remove(&class);
                continue;
            }
            if class != "Symfony\\Bundle\\FrameworkBundle\\FrameworkBundle"
                && bundles.contains_key(&class)
            {
                continue;
            }
            let entry = bundles.entry(class).or_default();
            if let Some(environments) = envs.as_array() {
                for environment in environments.iter().filter_map(Value::as_str) {
                    entry.insert(environment.to_owned(), true);
                }
            } else {
                for (environment, enabled) in object(envs, "bundle environments")? {
                    entry.insert(environment.clone(), enabled.as_bool().unwrap_or(true));
                }
            }
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut contents = String::from("<?php\n\nreturn [\n");
        for (class, envs) in bundles {
            contents.push_str(&format!("    {class}::class => ["));
            for (environment, enabled) in envs {
                contents.push_str(&format!("'{environment}' => {enabled}, "));
            }
            if contents.ends_with(", ") {
                contents.truncate(contents.len() - 2);
            }
            contents.push_str("],\n");
        }
        contents.push_str("];\n");
        std::fs::write(path, contents)?;
        Ok(())
    }

    fn configure_env(
        &self,
        recipe: &Recipe,
        config: &Value,
        suffix: &str,
        remove: bool,
    ) -> Result<()> {
        let dotenv_path = runtime_dotenv_path(self.working_dir)?;
        let files = if suffix.is_empty() {
            vec![format!("{dotenv_path}.dist"), dotenv_path.clone()]
        } else {
            vec![format!("{dotenv_path}.{suffix}")]
        };
        for file in files {
            let path = self.root().join(file);
            if !path.is_file() {
                continue;
            }
            if remove {
                remove_marked_section(&path, &recipe.name)?;
                continue;
            }
            if !self.force && is_marked(&path, &recipe.name) {
                continue;
            }
            let mut data = String::new();
            for (name, value) in object(config, "env")? {
                let value = evaluate_env_value(
                    string(value, "environment value")?,
                    self.force
                        .then(|| find_existing_env_value(&path, &recipe.name, name))
                        .flatten()
                        .as_deref(),
                )?;
                if name.starts_with('#')
                    && name[1..]
                        .chars()
                        .all(|character| character.is_ascii_digit())
                {
                    if value.is_empty() {
                        data.push_str("#\n");
                    } else {
                        data.push_str(&format!("# {value}\n"));
                    }
                } else {
                    data.push_str(&format!(
                        "{name}={}\n",
                        quote_env(&self.options.expand(&value))
                    ));
                }
            }
            append_or_replace_marked(&path, &recipe.name, &data)?;
        }
        if suffix.is_empty() && !self.root().join(format!("{dotenv_path}.test")).exists() {
            self.configure_phpunit_env(recipe, config, remove)?;
        }
        Ok(())
    }

    fn configure_phpunit_env(&self, recipe: &Recipe, config: &Value, remove: bool) -> Result<()> {
        for filename in ["phpunit.xml.dist", "phpunit.dist.xml", "phpunit.xml"] {
            let path = self.root().join(filename);
            if !path.is_file() {
                continue;
            }
            if remove {
                remove_xml_marked_section(&path, &recipe.name)?;
                continue;
            }
            if !self.force && is_xml_marked(&path, &recipe.name) {
                continue;
            }
            let mut data = String::new();
            for (name, value) in object(config, "env")? {
                let value = evaluate_env_value(string(value, "environment value")?, None)?;
                if let Some(comment) = name.strip_prefix('#') {
                    if comment.chars().all(|character| character.is_ascii_digit()) {
                        data.push_str(&format!("        <!-- {} -->\n", escape_xml(&value)));
                    } else {
                        data.push_str(&format!(
                            "        <!-- <env name=\"{}\" value=\"{}\" /> -->\n",
                            escape_xml(comment),
                            escape_xml(&self.options.expand(&value))
                        ));
                    }
                } else {
                    data.push_str(&format!(
                        "        <env name=\"{}\" value=\"{}\" />\n",
                        escape_xml(name),
                        escape_xml(&self.options.expand(&value))
                    ));
                }
            }
            let marked = xml_marked_section(&recipe.name, &data);
            let mut contents = std::fs::read_to_string(&path)?;
            if replace_xml_marked(&mut contents, &recipe.name, &marked)? {
                std::fs::write(path, contents)?;
            } else if let Some(at) = contents.find("</php>") {
                contents.insert_str(at, &marked);
                std::fs::write(path, contents)?;
            }
        }
        Ok(())
    }

    fn configure_container(&self, config: &Value, remove: bool) -> Result<()> {
        let path = self.target("%CONFIG_DIR%/services.yaml")?;
        if !path.is_file() {
            return Ok(());
        }
        let mut contents = std::fs::read_to_string(&path)?;
        for (name, value) in object(config, "container")? {
            let pattern = Regex::new(&format!(
                r"(?m)^    {}:.*(?:\n(?:        .*\n?)*)?",
                regex::escape(name)
            ))?;
            if remove {
                contents = pattern.replace(&contents, "").into_owned();
            } else if !pattern.is_match(&contents) {
                let rendered = render_yaml_value(value, 1)?;
                let insertion = format!("    {name}:{rendered}\n");
                if let Some(index) = contents.find("parameters:\n") {
                    let at = index + "parameters:\n".len();
                    contents.insert_str(at, &insertion);
                } else {
                    contents = format!("parameters:\n{insertion}\n{contents}");
                }
            }
        }
        std::fs::write(path, contents)?;
        Ok(())
    }

    fn configure_marked_file(
        &self,
        recipe: &Recipe,
        path: &Path,
        data: String,
        remove: bool,
    ) -> Result<()> {
        if remove {
            remove_marked_section(path, &recipe.name)
        } else {
            if !self.force && is_marked(path, &recipe.name) {
                return Ok(());
            }
            append_or_replace_marked(path, &recipe.name, &self.options.expand(&data))
        }
    }

    fn configure_dockerfile(&self, recipe: &Recipe, config: &Value, remove: bool) -> Result<()> {
        let path = self.root().join("Dockerfile");
        if remove {
            return remove_marked_section(&path, &recipe.name);
        }
        if !path.is_file() {
            return Ok(());
        }
        if !self.force && is_marked(&path, &recipe.name) {
            return Ok(());
        }
        let data = lines(config)?;
        let marked = marked_section(&recipe.name, &data);
        let mut contents = std::fs::read_to_string(&path)?;
        if replace_marked(&mut contents, &recipe.name, &marked)? {
            std::fs::write(path, contents)?;
        } else if let Some(at) = contents.find("###> recipes ###\n") {
            contents.insert_str(at + "###> recipes ###\n".len(), &marked);
            std::fs::write(path, contents)?;
        }
        Ok(())
    }

    fn configure_docker_compose(
        &self,
        recipe: &Recipe,
        config: &Value,
        remove: bool,
    ) -> Result<()> {
        for (filename, sections) in normalize_docker_compose(config)? {
            let path = find_docker_compose_file(&self.root(), &filename)
                .unwrap_or_else(|| self.root().join(&filename));
            if remove {
                remove_all_marked_sections(&path, &recipe.name)?;
                continue;
            }
            let mut contents = std::fs::read_to_string(&path).unwrap_or_default();
            for (section, definition) in sections {
                let data = definition
                    .as_array()
                    .with_context(|| format!("docker-compose section {section} must be an array"))?
                    .iter()
                    .map(|line| format!("  {}", line.as_str().unwrap_or_default()))
                    .collect::<Vec<_>>()
                    .join("\n");
                configure_docker_compose_section(
                    &mut contents,
                    &section,
                    recipe,
                    &data,
                    self.force,
                )?;
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, contents)?;
        }
        Ok(())
    }

    fn configure_add_lines(&self, _recipe: &Recipe, config: &Value, remove: bool) -> Result<()> {
        for patch in config.as_array().context("add-lines must be an array")? {
            let patch = patch
                .as_object()
                .context("add-lines entry must be an object")?;
            let file = patch
                .get("file")
                .and_then(Value::as_str)
                .context("add-lines.file is required")?;
            if let Some(requires) = patch.get("requires") {
                let requirements = match requires {
                    Value::String(package) => vec![package.as_str()],
                    Value::Array(packages) => packages.iter().filter_map(Value::as_str).collect(),
                    _ => Vec::new(),
                };
                if !remove
                    && requirements.iter().any(|package| {
                        !self
                            .installed_packages
                            .contains(package.split(':').next().unwrap_or(package))
                    })
                {
                    continue;
                }
            }
            let content = patch
                .get("content")
                .and_then(Value::as_str)
                .context("add-lines.content is required")?;
            let path = self.target(file)?;
            if !path.is_file() {
                continue;
            }
            let mut contents = std::fs::read_to_string(&path)?;
            if remove {
                if let Some(at) = contents.find(content) {
                    let start = if at > 0 && contents.as_bytes()[at - 1] == b'\n' {
                        at - 1
                    } else {
                        at
                    };
                    let end = at
                        + content.len()
                        + usize::from(contents[at + content.len()..].starts_with('\n'));
                    contents.replace_range(start..end, "");
                }
            } else if !contents.contains(content) {
                match patch
                    .get("position")
                    .and_then(Value::as_str)
                    .unwrap_or("bottom")
                {
                    "top" => contents = format!("{content}\n{contents}"),
                    "bottom" => contents.push_str(&format!("\n{content}")),
                    "after_target" => {
                        if let Some(target) = patch.get("target").and_then(Value::as_str) {
                            if let Some(at) = contents
                                .find(target)
                                .and_then(|at| contents[at..].find('\n').map(|end| at + end + 1))
                            {
                                contents.insert_str(at, &format!("{content}\n"));
                            }
                        }
                    }
                    _ => {}
                }
            }
            std::fs::write(path, contents)?;
        }
        Ok(())
    }
}

pub(crate) fn render_recipe(
    working_dir: &Path,
    manifest: &crate::json::RiffManifest,
    vendor_dir: PathBuf,
    installed_packages: impl IntoIterator<Item = String>,
    recipe: &Recipe,
) -> Result<RenderedRecipe> {
    let options = FlexOptions::from_manifest(manifest);
    let source_root = if options.root_dir.is_absolute() {
        options.root_dir.clone()
    } else {
        working_dir.join(&options.root_dir)
    };
    let root_relative = source_root.strip_prefix(working_dir).with_context(|| {
        format!(
            "Cannot update recipes when Symfony root-dir {} is outside the project",
            source_root.display()
        )
    })?;
    let paths = recipe_paths(working_dir, &source_root, &options, recipe)?;
    let temporary = tempfile::tempdir().context("Failed to create recipe update workspace")?;
    for relative in &paths {
        let source = working_dir.join(relative);
        if !source.is_file() {
            continue;
        }
        let target = temporary.path().join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&source, &target).with_context(|| {
            format!("Failed to prepare recipe-managed file {}", source.display())
        })?;
    }

    let mut configurator = Configurator::new(
        temporary.path(),
        manifest,
        vendor_dir,
        installed_packages,
        true,
    );
    let preview_root = if root_relative.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        root_relative.to_owned()
    };
    configurator.options.root_dir = preview_root.clone();
    configurator.options.directories.insert(
        "ROOT_DIR".to_owned(),
        preview_root.to_string_lossy().replace('\\', "/"),
    );
    let mut preview_lock = FlexLock::load(temporary.path())?;
    preview_lock.set(recipe.name.clone(), recipe.lock.clone());
    configurator.install(recipe, &mut preview_lock)?;
    if let Some(config) = recipe.manifest.get("add-lines") {
        configurator.configure_add_lines(recipe, config, false)?;
    }

    let files = paths
        .into_iter()
        .map(|relative| {
            let contents = std::fs::read(temporary.path().join(&relative)).ok();
            (relative.to_string_lossy().replace('\\', "/"), contents)
        })
        .collect();
    Ok(RenderedRecipe {
        files,
        lock_entry: preview_lock
            .get(&recipe.name)
            .cloned()
            .unwrap_or_else(|| recipe.lock.clone()),
        copies_from_package: recipe.manifest.contains_key("copy-from-package"),
    })
}

fn recipe_paths(
    working_dir: &Path,
    root: &Path,
    options: &FlexOptions,
    recipe: &Recipe,
) -> Result<BTreeSet<PathBuf>> {
    let mut paths = BTreeSet::from([PathBuf::from("composer.json")]);
    {
        let mut add = |path: PathBuf| -> Result<()> {
            let relative = path.strip_prefix(working_dir).with_context(|| {
                format!(
                    "Recipe-managed path {} is outside the project",
                    path.display()
                )
            })?;
            paths.insert(relative.to_owned());
            Ok(())
        };
        if let Some(config) = recipe.manifest.get("copy-from-recipe") {
            for (source, target) in object(config, "copy-from-recipe")? {
                let target = options.expand(string(target, "copy target")?);
                if source.ends_with('/') {
                    for file in recipe.files.keys() {
                        if let Some(suffix) = file.strip_prefix(source) {
                            add(safe_join(root, &format!("{target}{suffix}"))?)?;
                        }
                    }
                } else {
                    add(safe_join(root, &target)?)?;
                }
            }
        }
        if recipe.manifest.contains_key("bundles") {
            add(safe_join(
                root,
                &options.expand("%CONFIG_DIR%/bundles.php"),
            )?)?;
        }
        if recipe.manifest.contains_key("container") {
            add(safe_join(
                root,
                &options.expand("%CONFIG_DIR%/services.yaml"),
            )?)?;
        }
        if recipe.manifest.contains_key("makefile") {
            add(root.join("Makefile"))?;
        }
        if recipe.manifest.contains_key("gitignore") {
            add(root.join(".gitignore"))?;
        }
        if recipe.manifest.contains_key("dockerfile") {
            add(root.join("Dockerfile"))?;
        }
        if let Some(config) = recipe.manifest.get("docker-compose") {
            for (filename, _) in normalize_docker_compose(config)? {
                let path = find_docker_compose_file(root, &filename)
                    .unwrap_or_else(|| root.join(filename));
                add(path)?;
            }
        }
        if let Some(config) = recipe.manifest.get("add-lines") {
            for patch in config.as_array().context("add-lines must be an array")? {
                if let Some(file) = patch.get("file").and_then(Value::as_str) {
                    add(safe_join(root, &options.expand(file))?)?;
                }
            }
        }
    }
    if recipe.manifest.contains_key("env") {
        add_env_paths(working_dir, root, "", &mut paths)?;
    }
    if let Some(dotenv) = recipe.manifest.get("dotenv") {
        for suffix in object(dotenv, "dotenv")?.keys() {
            add_env_paths(working_dir, root, suffix, &mut paths)?;
        }
    }
    Ok(paths)
}

fn add_env_paths(
    working_dir: &Path,
    root: &Path,
    suffix: &str,
    paths: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let dotenv = runtime_dotenv_path(working_dir)?;
    let mut files = if suffix.is_empty() {
        vec![dotenv.clone(), format!("{dotenv}.dist")]
    } else {
        vec![format!("{dotenv}.{suffix}")]
    };
    if suffix.is_empty() && !root.join(format!("{dotenv}.test")).exists() {
        files.extend(
            ["phpunit.xml.dist", "phpunit.dist.xml", "phpunit.xml"]
                .into_iter()
                .map(str::to_owned),
        );
    }
    for file in files {
        let path = safe_join(root, &file)?;
        let relative = path.strip_prefix(working_dir).with_context(|| {
            format!(
                "Recipe-managed path {} is outside the project",
                path.display()
            )
        })?;
        paths.insert(relative.to_owned());
    }
    Ok(())
}

fn allow_contrib(working_dir: &Path) -> Result<bool> {
    if let Ok(value) = std::env::var("SYMFONY_ALLOW_CONTRIB") {
        return Ok(parse_bool(&value));
    }
    let json: Value = serde_json::from_slice(&std::fs::read(working_dir.join("composer.json"))?)?;
    Ok(json
        .pointer("/extra/symfony/allow-contrib")
        .and_then(Value::as_bool)
        .unwrap_or(false))
}

fn docker_enabled(working_dir: &Path) -> Result<bool> {
    if let Ok(value) = std::env::var("SYMFONY_DOCKER") {
        return Ok(parse_bool(&value));
    }
    let json: Value = serde_json::from_slice(&std::fs::read(working_dir.join("composer.json"))?)?;
    Ok(json
        .pointer("/extra/symfony/docker")
        .and_then(Value::as_bool)
        .unwrap_or(false))
}

fn runtime_dotenv_path(working_dir: &Path) -> Result<String> {
    let json: Value = serde_json::from_slice(&std::fs::read(working_dir.join("composer.json"))?)?;
    Ok(json
        .pointer("/extra/runtime/dotenv_path")
        .and_then(Value::as_str)
        .unwrap_or(".env")
        .to_owned())
}

fn parse_bool(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf> {
    let relative = Path::new(relative);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!(
            "Symfony recipe path escapes the project root: {}",
            relative.display()
        );
    }
    Ok(root.join(relative))
}

fn object<'a>(value: &'a Value, name: &str) -> Result<&'a Map<String, Value>> {
    value
        .as_object()
        .with_context(|| format!("{name} recipe configuration must be an object"))
}

fn string<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value
        .as_str()
        .with_context(|| format!("{name} must be a string"))
}

fn lines(value: &Value) -> Result<String> {
    Ok(value
        .as_array()
        .context("recipe configuration must be an array")?
        .iter()
        .map(|line| line.as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n"))
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn write_recipe_file(path: &Path, contents: &str, executable: bool, force: bool) -> Result<()> {
    if path.exists() && !force {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    #[cfg(not(unix))]
    let _ = executable;
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(permissions.mode() | 0o111);
        std::fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

fn copy_package_file(
    source: &Path,
    target: &Path,
    remove: bool,
    options: &FlexOptions,
    force: bool,
) -> Result<()> {
    if remove {
        if target.is_file() {
            std::fs::remove_file(target)?;
        }
        return Ok(());
    }
    if (target.exists() && !force) || !source.is_file() {
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = std::fs::read_to_string(source)
        .with_context(|| format!("Package recipe source {} is not UTF-8", source.display()))?;
    std::fs::write(target, options.expand(&contents))?;
    Ok(())
}

fn marked_section(name: &str, data: &str) -> String {
    format!(
        "\n###> {name} ###\n{}\n###< {name} ###\n",
        data.trim_end_matches(['\r', '\n'])
    )
}

fn is_marked(path: &Path, name: &str) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .is_some_and(|contents| contents.contains(&format!("###> {name} ###")))
}

fn is_xml_marked(path: &Path, name: &str) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .is_some_and(|contents| contents.contains(&format!("<!-- ###+ {name} ### -->")))
}

fn xml_marked_section(name: &str, data: &str) -> String {
    format!(
        "        <!-- ###+ {name} ### -->\n{}        <!-- ###- {name} ### -->\n",
        data.trim_end_matches(['\r', '\n']).to_owned() + "\n"
    )
}

fn replace_xml_marked(contents: &mut String, name: &str, replacement: &str) -> Result<bool> {
    let pattern = Regex::new(&format!(
        r"(?s)\s*<!-- \#\#\#\+ {} \#\#\# -->.*?<!-- \#\#\#- {} \#\#\# -->\s*",
        regex::escape(name),
        regex::escape(name)
    ))?;
    if !pattern.is_match(contents) {
        return Ok(false);
    }
    *contents = pattern.replace(contents, replacement).into_owned();
    Ok(true)
}

fn remove_xml_marked_section(path: &Path, name: &str) -> Result<()> {
    let mut contents = std::fs::read_to_string(path)?;
    if replace_xml_marked(&mut contents, name, "\n")? {
        std::fs::write(path, contents)?;
    }
    Ok(())
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn find_existing_env_value(path: &Path, recipe: &str, variable: &str) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let start = contents.find(&format!("###> {recipe} ###"))?;
    let section = &contents[start..];
    let end = section.find(&format!("###< {recipe} ###"))?;
    section[..end].lines().find_map(|line| {
        line.strip_prefix(variable)
            .and_then(|line| line.strip_prefix('='))
            .map(|value| value.trim().to_owned())
    })
}

fn evaluate_env_value(value: &str, existing: Option<&str>) -> Result<String> {
    let length = if value == "%generate(secret)%" {
        Some(16)
    } else {
        value
            .strip_prefix("%generate(secret,")
            .and_then(|value| value.strip_suffix(")%"))
            .and_then(|value| value.trim().parse::<usize>().ok())
    };
    let Some(length) = length else {
        return Ok(value.to_owned());
    };
    if let Some(existing) = existing {
        return Ok(existing.to_owned());
    }
    let mut bytes = vec![0_u8; length];
    getrandom::fill(&mut bytes).context("Failed to generate Symfony recipe secret")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn append_or_replace_marked(path: &Path, name: &str, data: &str) -> Result<()> {
    let marked = marked_section(name, data);
    let mut contents = std::fs::read_to_string(path).unwrap_or_default();
    if !replace_marked(&mut contents, name, &marked)? {
        contents.push_str(&marked);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    Ok(())
}

fn replace_marked(contents: &mut String, name: &str, replacement: &str) -> Result<bool> {
    let pattern = Regex::new(&format!(
        r"(?s)\n?###> {} ###.*?###< {} ###\n?",
        regex::escape(name),
        regex::escape(name)
    ))?;
    if !pattern.is_match(contents) {
        return Ok(false);
    }
    *contents = pattern.replace(contents, replacement).into_owned();
    Ok(true)
}

fn remove_marked_section(path: &Path, name: &str) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let mut contents = std::fs::read_to_string(path)?;
    if replace_marked(&mut contents, name, "\n")? {
        if contents.trim().is_empty() {
            std::fs::remove_file(path)?;
        } else {
            let contents = contents.trim_matches(['\r', '\n']);
            std::fs::write(path, format!("{contents}\n"))?;
        }
    }
    Ok(())
}

fn remove_all_marked_sections(path: &Path, name: &str) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let contents = std::fs::read_to_string(path)?;
    let pattern = Regex::new(&format!(
        r"(?s)\n?###> {} ###.*?###< {} ###\n?",
        regex::escape(name),
        regex::escape(name)
    ))?;
    let updated = pattern.replace_all(&contents, "\n");
    if updated != contents {
        std::fs::write(path, updated.as_bytes())?;
    }
    Ok(())
}

fn normalize_docker_compose(config: &Value) -> Result<Vec<(String, Map<String, Value>)>> {
    let config = object(config, "docker-compose")?;
    if config.values().next().is_some_and(Value::is_array) {
        return Ok(vec![("compose.yaml".to_owned(), config.clone())]);
    }
    config
        .iter()
        .map(|(filename, sections)| {
            let mut normalized = filename
                .strip_prefix("docker-")
                .unwrap_or(filename)
                .to_owned();
            if normalized.ends_with(".yml") {
                normalized.truncate(normalized.len() - 4);
                normalized.push_str(".yaml");
            }
            Ok((normalized, object(sections, "docker-compose file")?.clone()))
        })
        .collect()
}

fn find_docker_compose_file(root: &Path, filename: &str) -> Option<PathBuf> {
    let yaml = root.join(filename);
    let yml = filename
        .strip_suffix(".yaml")
        .map(|filename| root.join(format!("{filename}.yml")));
    let docker_yaml = root.join(format!("docker-{filename}"));
    let docker_yml = filename
        .strip_suffix(".yaml")
        .map(|filename| root.join(format!("docker-{filename}.yml")));
    std::iter::once(yaml)
        .chain(yml)
        .chain(std::iter::once(docker_yaml))
        .chain(docker_yml)
        .find(|path| path.is_file())
}

fn configure_docker_compose_section(
    contents: &mut String,
    section: &str,
    recipe: &Recipe,
    data: &str,
    force: bool,
) -> Result<()> {
    let header = Regex::new(&format!(r"(?m)^{}:\s*\r?$", regex::escape(section)))?;
    if !header.is_match(contents) {
        if !contents.is_empty() && !contents.ends_with('\n') {
            contents.push('\n');
        }
        if !contents.is_empty() {
            contents.push('\n');
        }
        contents.push_str(&format!("{section}:\n"));
    }
    let matched = header
        .find(contents)
        .expect("newly inserted Docker Compose section must match");
    let start = contents[matched.end()..]
        .find('\n')
        .map_or(matched.end(), |offset| matched.end() + offset + 1);
    let next_header = Regex::new(r"(?m)^[A-Za-z0-9_-]+:\s*\r?$")?;
    let end = next_header
        .find(&contents[start..])
        .map_or(contents.len(), |next| start + next.start());
    let mut body = contents[start..end].to_owned();
    if !force && body.contains(&format!("###> {} ###", recipe.name)) {
        return Ok(());
    }
    let marked = marked_section(&recipe.name, data);
    if !replace_marked(&mut body, &recipe.name, &marked)? {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(marked.trim_start_matches('\n'));
    }
    contents.replace_range(start..end, &body);
    Ok(())
}

fn remove_empty_parents(mut path: Option<&Path>, root: &Path) {
    while let Some(directory) = path {
        if directory == root || std::fs::remove_dir(directory).is_err() {
            break;
        }
        path = directory.parent();
    }
}

fn parse_bundles(contents: &str) -> IndexMap<String, IndexMap<String, bool>> {
    let mut bundles = IndexMap::new();
    let Ok(line) = Regex::new(r"(?m)^\s*([^\s].*?)::class\s*=>\s*\[(.*?)\],?\s*$") else {
        return bundles;
    };
    let env = Regex::new(r#"['\"]([^'\"]+)['\"]\s*=>\s*(true|false)"#).unwrap();
    for captures in line.captures_iter(contents) {
        let mut environments = IndexMap::new();
        for env_capture in env.captures_iter(&captures[2]) {
            environments.insert(env_capture[1].to_owned(), &env_capture[2] == "true");
        }
        bundles.insert(captures[1].trim().to_owned(), environments);
    }
    bundles
}

fn quote_env(value: &str) -> String {
    if value
        .chars()
        .any(|character| " \t\n&!\"".contains(character))
    {
        format!(
            "\"{}\"",
            value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\t', "\\t")
                .replace('\n', "\\n")
        )
    } else {
        value.to_owned()
    }
}

fn render_yaml_value(value: &Value, level: usize) -> Result<String> {
    if let Some(value) = value.as_str() {
        return Ok(format!(" '{}'", value.replace('\'', "''")));
    }
    let yaml: serde_yaml::Value = serde_json::from_value(value.clone())?;
    let rendered = serde_yaml::to_string(&yaml)?;
    let indent = "    ".repeat(level + 1);
    Ok(format!(
        "\n{}",
        rendered.trim().replace('\n', &format!("\n{indent}"))
    ))
}

fn recipe_origin(recipe: &Recipe) -> String {
    if recipe.origin.is_empty() {
        recipe.name.clone()
    } else {
        recipe.origin.clone()
    }
}

fn package_version(version: &str) -> String {
    let mut parts = version.trim_start_matches(['v', 'V']).split('.');
    format!(
        "{}.{}",
        parts.next().unwrap_or("0"),
        parts.next().unwrap_or("9999999")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_recipe_paths_outside_project() {
        assert!(safe_join(Path::new("/project"), "../secret").is_err());
        assert!(safe_join(Path::new("/project"), "/etc/passwd").is_err());
        assert_eq!(
            safe_join(Path::new("/project"), "config/app.yaml").unwrap(),
            Path::new("/project/config/app.yaml")
        );
    }

    #[test]
    fn marked_sections_replace_and_remove_cleanly() {
        let project = tempfile::tempdir().unwrap();
        let path = project.path().join(".gitignore");
        std::fs::write(&path, "/vendor\n").unwrap();
        append_or_replace_marked(&path, "vendor/package", "/var").unwrap();
        append_or_replace_marked(&path, "vendor/package", "/cache").unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.matches("###> vendor/package ###").count(), 1);
        assert!(contents.contains("/cache"));
        remove_marked_section(&path, "vendor/package").unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "/vendor\n");
    }

    #[test]
    fn generated_secret_is_stable_during_forced_recipe_updates() {
        let project = tempfile::tempdir().unwrap();
        let path = project.path().join(".env");
        std::fs::write(
            &path,
            "###> vendor/package ###\nAPP_SECRET=keep-me\n###< vendor/package ###\n",
        )
        .unwrap();

        let existing = find_existing_env_value(&path, "vendor/package", "APP_SECRET");
        assert_eq!(
            evaluate_env_value("%generate(secret)%", existing.as_deref()).unwrap(),
            "keep-me"
        );
        let generated = evaluate_env_value("%generate(secret, 24)%", None).unwrap();
        assert_eq!(generated.len(), 48);
        assert!(generated
            .chars()
            .all(|character| character.is_ascii_hexdigit()));
    }

    #[test]
    fn xml_marked_sections_remove_cleanly() {
        let project = tempfile::tempdir().unwrap();
        let path = project.path().join("phpunit.xml.dist");
        let mut contents = "<phpunit>\n    <php>\n    </php>\n</phpunit>\n".to_owned();
        let marked = xml_marked_section(
            "vendor/package",
            "        <env name=\"APP_ENV\" value=\"test\" />\n",
        );
        let at = contents.find("</php>").unwrap();
        contents.insert_str(at, &marked);
        std::fs::write(&path, contents).unwrap();

        assert!(is_xml_marked(&path, "vendor/package"));
        remove_xml_marked_section(&path, "vendor/package").unwrap();
        let contents = std::fs::read_to_string(path).unwrap();
        assert!(!contents.contains("APP_ENV"));
        assert!(contents.contains("</php>"));
    }

    #[test]
    fn docker_compose_recipes_are_inserted_inside_their_sections() {
        let recipe = Recipe {
            package: std::sync::Arc::new(crate::package::Package::new("vendor/package", "1.0")),
            name: "vendor/package".to_owned(),
            job: RecipeJob::Install,
            manifest: Map::new(),
            files: IndexMap::new(),
            lock: Value::Null,
            origin: String::new(),
            is_contrib: false,
        };
        let mut contents = "services:\n  existing:\n    image: example\n\nvolumes:\n".to_owned();
        configure_docker_compose_section(
            &mut contents,
            "services",
            &recipe,
            "  database:\n    image: postgres",
            false,
        )
        .unwrap();

        assert!(contents.starts_with("services:\n"));
        assert!(contents.contains("###> vendor/package ###\n  database:"));
        assert_eq!(contents.matches("services:").count(), 1);
        assert!(contents.find("database:").unwrap() < contents.find("volumes:").unwrap());
    }
}
