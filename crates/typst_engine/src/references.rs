use crate::{Location, Snapshot};
use anyhow::{Result, anyhow, ensure};
use std::{collections::BTreeMap, ops::Range, path::Path, sync::Arc};
use typst::{
    foundations::Value,
    syntax::{LinkedNode, Side, Source, SyntaxKind, ast, ast::AstNode, is_ident},
};
use typst_ide::{Definition, NamedItem};

#[derive(Clone, Debug)]
pub struct Edit {
    pub location: Location,
    pub text: String,
}

#[derive(Clone, Debug)]
struct Occurrence {
    location: Location,
    name: String,
    target: Option<Location>,
    label: bool,
}

struct Alias {
    name: String,
    location: Location,
    scope: Range<usize>,
    visible_from: usize,
}

pub struct Index {
    occurrences: Vec<Occurrence>,
    aliases: BTreeMap<std::path::PathBuf, Vec<Alias>>,
    labels: BTreeMap<String, Vec<Location>>,
    sources: BTreeMap<std::path::PathBuf, Source>,
    malformed: bool,
}

impl Index {
    pub fn new(snapshot: &Snapshot) -> Result<Self> {
        let mut index = Self {
            occurrences: Vec::new(),
            aliases: BTreeMap::new(),
            labels: BTreeMap::new(),
            sources: BTreeMap::new(),
            malformed: false,
        };
        for path in snapshot
            .input
            .known_files
            .iter()
            .chain(snapshot.input.files.keys())
        {
            if !matches!(
                path.extension().and_then(|value| value.to_str()),
                Some("typ" | "typst")
            ) {
                continue;
            }
            if index.sources.contains_key(path) {
                continue;
            }
            let source = snapshot.source_at(path)?;
            index.malformed |= source.root().diagnosis().errors;
            index.sources.insert(path.clone(), source);
        }
        for (path, source) in &index.sources {
            visit(LinkedNode::new(source.root()), &mut |node| {
                if let Some(label) = node.cast::<ast::Label>() {
                    let range = node.range();
                    index
                        .labels
                        .entry(label.get().to_string())
                        .or_default()
                        .push(Location {
                            path: path.clone(),
                            range: range.start + 1..range.end - 1,
                        });
                }
                if let Some(import) = node.cast::<ast::ModuleImport>() {
                    let mut declarations = Vec::new();
                    if let Some(name) = import.new_name() {
                        declarations.push(name);
                    }
                    if let Some(ast::Imports::Items(items)) = import.imports() {
                        for item in items.iter() {
                            if matches!(item, ast::ImportItem::Renamed(_)) {
                                declarations.push(item.bound_name());
                            }
                        }
                    }
                    for name in declarations {
                        if let Some(location) = snapshot.location(name.span()) {
                            let scope = containing_scope(&node);
                            index.aliases.entry(path.clone()).or_default().push(Alias {
                                name: name.get().to_string(),
                                location,
                                scope,
                                visible_from: node.range().end,
                            });
                        }
                    }
                }
            });
        }
        let sources = index.sources.clone();
        for (path, source) in sources {
            visit(LinkedNode::new(source.root()), &mut |node| {
                let kind = node.kind();
                let label = matches!(kind, SyntaxKind::Label | SyntaxKind::Ref);
                let (name, range) = if let Some(label) = node.cast::<ast::Label>() {
                    (
                        label.get().to_string(),
                        node.range().start + 1..node.range().end - 1,
                    )
                } else if let Some(reference) = node.cast::<ast::Ref>() {
                    let name = reference.target();
                    (
                        name.to_string(),
                        node.range().start + 1..node.range().start + 1 + name.len(),
                    )
                } else if matches!(kind, SyntaxKind::Ident | SyntaxKind::MathIdent) {
                    (node.leaf_text().to_string(), node.range())
                } else {
                    return;
                };
                let target = if label {
                    index
                        .labels
                        .get(&name)
                        .filter(|locations| locations.len() == 1)
                        .and_then(|locations| locations.first())
                        .cloned()
                } else {
                    index.resolve(snapshot, &path, &source, &node, &name)
                };
                index.occurrences.push(Occurrence {
                    location: Location {
                        path: path.clone(),
                        range,
                    },
                    name,
                    target,
                    label,
                });
            });
        }
        Ok(index)
    }

    fn resolve(
        &self,
        snapshot: &Snapshot,
        path: &Path,
        source: &Source,
        node: &LinkedNode,
        name: &str,
    ) -> Option<Location> {
        if let Some(location) = snapshot.location(node.span())
            && self.is_declaration(node)
        {
            return Some(location);
        }
        if let Some(import_item) = node.parent().filter(|parent| {
            matches!(
                parent.kind(),
                SyntaxKind::ImportItemPath | SyntaxKind::RenamedImportItem
            )
        }) {
            let import = ancestor(import_item, SyntaxKind::ModuleImport)?;
            let expression = import.cast::<ast::ModuleImport>()?.source();
            let expression = import.find(expression.span())?;
            let value = typst_ide::analyze_import(snapshot, &expression)?;
            return value
                .scope()?
                .get(name)
                .and_then(|binding| snapshot.location(binding.span()));
        }
        if let Some(parent) = node.parent()
            && let Some(access) = parent.cast::<ast::FieldAccess>()
            && access.field().span() == node.span()
        {
            let target = parent.find(access.target().span())?;
            let values = typst_ide::analyze_expr(snapshot, &target);
            let (Value::Module(module), _) = values.first()? else {
                return None;
            };
            return module
                .scope()
                .get(name)
                .and_then(|binding| snapshot.location(binding.span()));
        }
        if let Some(parent) = node.parent()
            && let Some(named) = parent.cast::<ast::Named>()
            && named.name().span() == node.span()
        {
            let call = ancestor(parent, SyntaxKind::FuncCall)?;
            let callee = call.find(call.cast::<ast::FuncCall>()?.callee().span())?;
            let values = typst_ide::analyze_expr(snapshot, &callee);
            let (Value::Func(function), _) = values.first()? else {
                return None;
            };
            let parameter = function.param(name)?;
            if let typst::foundations::ParamInfo::Closure(parameter) = parameter {
                let source = snapshot
                    .source_at(&snapshot.location(parameter.span)?.path)
                    .ok()?;
                let node = source.find(parameter.span)?;
                if let Some(named) = node.cast::<ast::Named>() {
                    return snapshot.location(named.name().span());
                }
                return snapshot.location(parameter.span);
            }
            return None;
        }
        if let Some(location) = typst_ide::named_items(snapshot, node.clone(), |item| match item {
            NamedItem::Var(ident) | NamedItem::Fn(ident) if ident.get().as_str() == name => {
                snapshot.location(ident.span())
            }
            NamedItem::Module(bound, span, _) if bound.as_str() == name => snapshot.location(span),
            NamedItem::Import(bound, span, _) if bound.as_str() == name => self
                .aliases
                .get(path)
                .and_then(|aliases| {
                    aliases.iter().rev().find(|alias| {
                        alias.name == name
                            && alias.scope.contains(&node.range().start)
                            && alias.visible_from <= node.range().start
                    })
                })
                .map(|alias| alias.location.clone())
                .or_else(|| snapshot.location(span)),
            _ => None,
        }) {
            return Some(location);
        }
        match typst_ide::definition(
            snapshot,
            None::<&typst_layout::PagedDocument>,
            source,
            node.range().start,
            Side::After,
        ) {
            Some(Definition::Span(span)) => snapshot.location(span),
            _ => None,
        }
    }

    fn is_declaration(&self, node: &LinkedNode) -> bool {
        let span = node.span();
        let mut current = node.parent();
        while let Some(parent) = current {
            if let Some(binding) = parent.cast::<ast::LetBinding>() {
                return binding
                    .kind()
                    .bindings()
                    .iter()
                    .any(|ident| ident.span() == span);
            }
            if let Some(parameters) = parent.cast::<ast::Params>() {
                return parameters.children().any(|parameter| match parameter {
                    ast::Param::Pos(pattern) => {
                        pattern.bindings().iter().any(|ident| ident.span() == span)
                    }
                    ast::Param::Named(named) => named.name().span() == span,
                    ast::Param::Spread(spread) => spread
                        .sink_ident()
                        .is_some_and(|ident| ident.span() == span),
                });
            }
            if let Some(loop_node) = parent.cast::<ast::ForLoop>() {
                return loop_node
                    .pattern()
                    .bindings()
                    .iter()
                    .any(|ident| ident.span() == span);
            }
            if let Some(import) = parent.cast::<ast::ModuleImport>() {
                return import.new_name().is_some_and(|ident| ident.span() == span)
                    || import.imports().is_some_and(|items| match items {
                        ast::Imports::Items(items) => items.iter().any(|item| {
                            matches!(item, ast::ImportItem::Renamed(_))
                                && item.bound_name().span() == span
                        }),
                        _ => false,
                    });
            }
            if matches!(
                parent.kind(),
                SyntaxKind::CodeBlock | SyntaxKind::ContentBlock
            ) {
                break;
            }
            current = parent.parent();
        }
        false
    }

    fn occurrence(&self, path: &Path, cursor: usize) -> Result<&Occurrence> {
        self.occurrences
            .iter()
            .find(|occurrence| {
                occurrence.location.path == path
                    && occurrence.location.range.start <= cursor
                    && cursor <= occurrence.location.range.end
            })
            .context("There is no renamable Typst symbol here")
    }

    pub fn range(&self, path: &Path, cursor: usize) -> Result<Range<usize>> {
        Ok(self.occurrence(path, cursor)?.location.range.clone())
    }

    pub fn target(&self, path: &Path, cursor: usize) -> Result<Location> {
        self.occurrence(path, cursor)?
            .target
            .clone()
            .ok_or_else(|| anyhow!("This Typst symbol cannot be resolved safely"))
    }

    pub fn references(&self, path: &Path, cursor: usize) -> Result<Vec<Location>> {
        let target = self.target(path, cursor)?;
        Ok(self
            .occurrences
            .iter()
            .filter(|occurrence| occurrence.target.as_ref() == Some(&target))
            .map(|occurrence| occurrence.location.clone())
            .collect())
    }

    pub fn rename(
        &self,
        snapshot: &Snapshot,
        path: &Path,
        cursor: usize,
        name: &str,
    ) -> Result<Vec<Edit>> {
        ensure!(
            !snapshot.is_package_path(path),
            "Typst packages cannot be renamed"
        );
        ensure!(
            !self.malformed,
            "Fix syntax errors before renaming Typst symbols"
        );
        let occurrence = self.occurrence(path, cursor)?;
        let target = self.target(path, cursor)?;
        ensure!(
            target.path.starts_with(&snapshot.input.root)
                && !snapshot.is_package_path(&target.path),
            "Package and standard-library symbols are read-only"
        );
        ensure!(
            is_ident(name),
            "The new name must be a valid Typst identifier"
        );
        ensure!(
            self.occurrences
                .iter()
                .any(|candidate| candidate.location == target
                    && candidate.target.as_ref() == Some(&target)),
            "This Typst binding has no explicit declaration to rename"
        );
        ensure!(
            self.occurrences.iter().all(|candidate| {
                candidate.name != occurrence.name
                    || candidate.label != occurrence.label
                    || candidate.target.is_some()
            }),
            "An unresolved occurrence makes this rename ambiguous"
        );
        ensure!(
            !self.occurrences.iter().any(|candidate| {
                candidate.name == name
                    && candidate.label == occurrence.label
                    && candidate.target.as_ref() != Some(&target)
            }),
            "The new name conflicts with another Typst binding"
        );
        let locations = self.references(path, cursor)?;
        let edits = locations
            .into_iter()
            .map(|location| Edit {
                location,
                text: name.to_owned(),
            })
            .collect::<Vec<_>>();
        let mut files = BTreeMap::new();
        for (path, source) in &self.sources {
            let mut text = source.text().to_owned();
            let mut changes = edits
                .iter()
                .filter(|edit| &edit.location.path == path)
                .collect::<Vec<_>>();
            changes.sort_by_key(|edit| std::cmp::Reverse(edit.location.range.start));
            for change in changes {
                text.replace_range(change.location.range.clone(), &change.text);
            }
            files.insert(path.clone(), Arc::from(text));
        }
        let updated = snapshot.with_files(files);
        let new_index = Index::new(&updated)?;
        ensure!(
            !new_index.malformed,
            "The new name would make the Typst source invalid"
        );
        // Re-resolve all bindings after the edit, including occurrences outside the
        // rename, so shadowing and accidental capture cannot silently change meaning.
        for previous in &self.occurrences {
            let location = moved_location(&previous.location, &edits);
            let expected_target = previous
                .target
                .as_ref()
                .map(|target| moved_location(target, &edits));
            let current = new_index
                .occurrences
                .iter()
                .find(|candidate| candidate.location == location);
            ensure!(
                current.is_some_and(|current| current.target == expected_target),
                "The rename would change another Typst binding or cannot be verified safely"
            );
        }
        Ok(edits)
    }
}

use anyhow::Context as _;

fn moved_location(location: &Location, edits: &[Edit]) -> Location {
    let mut start = location.range.start as isize;
    let mut end = location.range.end as isize;
    for edit in edits
        .iter()
        .filter(|edit| edit.location.path == location.path)
    {
        let delta = edit.text.len() as isize - edit.location.range.len() as isize;
        if edit.location.range.end <= location.range.start {
            start += delta;
            end += delta;
        } else if edit.location.range == location.range {
            end += delta;
        }
    }
    Location {
        path: location.path.clone(),
        range: start as usize..end as usize,
    }
}

fn visit<'a>(node: LinkedNode<'a>, callback: &mut impl FnMut(&LinkedNode<'a>)) {
    callback(&node);
    for child in node.children() {
        visit(child, callback);
    }
}

fn containing_scope(node: &LinkedNode) -> Range<usize> {
    let mut parent = node.parent();
    while let Some(node) = parent {
        if matches!(
            node.kind(),
            SyntaxKind::CodeBlock | SyntaxKind::ContentBlock
        ) {
            return node.range();
        }
        if node.parent().is_none() {
            return node.range();
        }
        parent = node.parent();
    }
    0..usize::MAX
}

fn ancestor<'a>(node: &'a LinkedNode<'a>, kind: SyntaxKind) -> Option<LinkedNode<'a>> {
    let mut current = Some(node.clone());
    while let Some(node) = current {
        if node.kind() == kind {
            return Some(node);
        }
        current = node.parent().cloned();
    }
    None
}
