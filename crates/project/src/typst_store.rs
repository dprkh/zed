use crate::{
    Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource, Hover, HoverBlock,
    HoverBlockKind, Location, PrepareRenameResponse, Project, buffer_store::ProjectTransaction,
    lsp_store::CompletionDocumentation, trusted_worktrees::TrustedWorktrees,
};
use anyhow::{Context as _, Result, ensure};
use collections::HashMap;
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Subscription, Task, WeakEntity};
use language::{
    Buffer, CodeLabel, Diagnostic, DiagnosticEntry, DiagnosticMessage, DiagnosticSet, File as _,
    LocalFile as _, ToOffset, language_settings::FormatOnSave,
};
use parking_lot::Mutex;
use settings::{RegisterSetting, Settings, SettingsLocation};
use std::{
    collections::BTreeMap,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use typst_engine::{Compilation, Configuration, Engine, FileProvider, Input, Snapshot, references};
use util::ResultExt as _;

#[derive(Clone, Debug, Default, RegisterSetting)]
pub struct TypstSettings(pub Configuration);

impl Settings for TypstSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let content = content.typst.clone().unwrap_or_default();
        Self(Configuration {
            root: content.root,
            main_file: content.main_file,
            font_paths: content.font_paths.unwrap_or_default(),
            inputs: content.inputs.unwrap_or_default(),
            system_fonts: content.system_fonts.unwrap_or(true),
            package_downloads: content.package_downloads.unwrap_or(true),
        })
    }
}

struct ProjectFiles(Arc<dyn fs::Fs>);

impl FileProvider for ProjectFiles {
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        // Typst's World reads synchronously. A deterministic FakeFs delay cannot
        // advance while this call occupies its single test-executor thread.
        #[cfg(any(test, feature = "test-support"))]
        if self.0.is_fake() {
            return self
                .0
                .as_fake()
                .read_file_sync(path)
                .map_err(std::io::Error::other);
        }
        smol::block_on(self.0.load_bytes(path)).map_err(std::io::Error::other)
    }
}

#[derive(Clone)]
pub struct Request {
    pub input: Input,
    pub path: PathBuf,
    configuration: Configuration,
    files: Arc<dyn FileProvider>,
    buffers: BTreeMap<PathBuf, language::BufferSnapshot>,
}

struct CachedEngine {
    configuration: Configuration,
    engine: Arc<std::sync::OnceLock<Arc<Engine>>>,
}

#[derive(Default)]
struct Session {
    generation: u64,
    latest: Option<Arc<Compilation>>,
    last_good: Option<Arc<Compilation>>,
    input: Option<Input>,
    configuration: Option<Configuration>,
    last_good_buffers: BTreeMap<PathBuf, language::BufferSnapshot>,
    latest_buffers: BTreeMap<PathBuf, language::BufferSnapshot>,
    error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Updated {
    pub entry: PathBuf,
}

pub struct TypstStore {
    project: WeakEntity<Project>,
    engines: Arc<Mutex<HashMap<PathBuf, CachedEngine>>>,
    sessions: HashMap<PathBuf, Session>,
    generation: u64,
    pending: BTreeMap<PathBuf, Request>,
    focused: Option<PathBuf>,
    compilation_task: Option<Task<()>>,
    debounce_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<Updated> for TypstStore {}

impl TypstStore {
    pub fn new(project: WeakEntity<Project>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe_global::<settings::SettingsStore>(|this, cx| {
            this.engines.lock().clear();
            let project = this.project.clone();
            cx.defer(move |cx| {
                project
                    .update(cx, |project, cx| project.refresh_typst(cx))
                    .log_err();
            });
        });
        let mut subscriptions = vec![subscription];
        if let Some(trust) = TrustedWorktrees::try_get_global(cx) {
            subscriptions.push(cx.subscribe(&trust, |this, _, _, cx| {
                this.engines.lock().clear();
                let project = this.project.clone();
                cx.defer(move |cx| {
                    project
                        .update(cx, |project, cx| project.refresh_typst(cx))
                        .log_err();
                });
            }));
        }
        Self {
            project,
            engines: Arc::default(),
            sessions: HashMap::default(),
            generation: 0,
            pending: BTreeMap::new(),
            focused: None,
            compilation_task: None,
            debounce_task: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn snapshot(&self, request: Request, cx: &App) -> Task<Result<Arc<Snapshot>>> {
        let engines = self.engines.clone();
        cx.background_spawn(async move {
            let engine = {
                let mut engines = engines.lock();
                let cached = engines
                    .entry(request.input.root.clone())
                    .or_insert_with(|| CachedEngine {
                        configuration: request.configuration.clone(),
                        engine: Arc::default(),
                    });
                if cached.configuration != request.configuration {
                    cached.engine = Arc::default();
                    cached.configuration = request.configuration.clone();
                }
                cached.engine.clone()
            };
            let engine = engine.get_or_init(|| {
                Arc::new(Engine::with_files(
                    request.configuration,
                    &request.input.root,
                    request.files,
                ))
            });
            engine.snapshot(request.input)
        })
    }

    pub fn compilation(&self, entry: &Path) -> Option<Arc<Compilation>> {
        self.sessions.get(entry)?.latest.clone()
    }

    pub fn pages(&self, entry: &Path) -> Option<Arc<Compilation>> {
        self.sessions.get(entry)?.last_good.clone()
    }

    pub fn error(&self, entry: &Path) -> Option<&str> {
        self.sessions.get(entry)?.error.as_deref()
    }

    pub fn diagnostic_summaries(
        &self,
        project: &Project,
        include_ignored: bool,
        cx: &App,
    ) -> Vec<(crate::ProjectPath, crate::DiagnosticSummary)> {
        self.diagnostics()
            .into_iter()
            .filter_map(|(path, diagnostics)| {
                let path = project.project_path_for_absolute_path(&path, cx)?;
                let entry = project.entry_for_path(&path, cx)?;
                if !include_ignored && entry.is_ignored {
                    return None;
                }
                let mut summary = crate::DiagnosticSummary::default();
                for diagnostic in diagnostics {
                    if diagnostic.warning {
                        summary.warning_count += 1;
                    } else {
                        summary.error_count += 1;
                    }
                }
                Some((path, summary))
            })
            .collect()
    }

    fn diagnostics(&self) -> BTreeMap<PathBuf, Vec<typst_engine::Diagnostic>> {
        let mut diagnostics: BTreeMap<PathBuf, Vec<typst_engine::Diagnostic>> = BTreeMap::new();
        for session in self.sessions.values() {
            if let Some(compilation) = &session.latest {
                for diagnostic in &compilation.diagnostics {
                    let Some(location) = &diagnostic.location else {
                        continue;
                    };
                    let entries = diagnostics.entry(location.path.clone()).or_default();
                    if !entries.iter().any(|entry| {
                        entry.location == diagnostic.location
                            && entry.message == diagnostic.message
                            && entry.warning == diagnostic.warning
                    }) {
                        entries.push(diagnostic.clone());
                    }
                }
            }
        }
        diagnostics
    }

    pub fn source_offset(&self, entry: &Path, path: &Path, anchor: &text::Anchor) -> Option<usize> {
        let snapshot = self.sessions.get(entry)?.last_good_buffers.get(path)?;
        snapshot
            .can_resolve(anchor)
            .then(|| anchor.to_offset(snapshot))
    }

    pub fn source_range(
        &self,
        entry: &Path,
        location: &typst_engine::Location,
        buffer: &Buffer,
    ) -> Range<usize> {
        self.sessions
            .get(entry)
            .and_then(|session| session.last_good_buffers.get(&location.path))
            .filter(|snapshot| snapshot.remote_id() == buffer.remote_id())
            .map(|snapshot| {
                let start = snapshot
                    .anchor_after(location.range.start)
                    .to_offset(buffer);
                let end = snapshot.anchor_before(location.range.end).to_offset(buffer);
                start..end.max(start)
            })
            .unwrap_or_else(|| {
                let start = buffer.clip_offset(location.range.start, text::Bias::Right);
                let end = buffer.clip_offset(location.range.end, text::Bias::Left);
                start..end.max(start)
            })
    }

    pub fn refresh(&mut self, mut request: Request, cx: &mut Context<Self>) {
        if self
            .sessions
            .get(&request.input.entry)
            .is_some_and(|session| {
                session.configuration.as_ref() == Some(&request.configuration)
                    && session.input.as_ref().is_some_and(|input| {
                        input.root == request.input.root
                            && input.files == request.input.files
                            && input.known_files == request.input.known_files
                    })
            })
        {
            return;
        }
        self.generation += 1;
        request.input.generation = self.generation;
        let entry = request.input.entry.clone();
        let session = self.sessions.entry(entry.clone()).or_default();
        session.generation = self.generation;
        session.input = Some(request.input.clone());
        session.configuration = Some(request.configuration.clone());
        self.pending.insert(entry, request);
        if self.debounce_task.is_none() {
            let delay = cx.background_executor().timer(Duration::from_millis(20));
            self.debounce_task = Some(cx.spawn(async move |this, cx| {
                delay.await;
                this.update(cx, |this, cx| {
                    this.debounce_task = None;
                    this.start_next(cx);
                })
                .log_err();
            }));
        }
    }

    pub fn focus(&mut self, entry: PathBuf) {
        self.focused = Some(entry);
    }

    pub fn invalidate_files(&mut self, changed_fonts: bool) {
        if changed_fonts {
            self.engines.lock().clear();
        }
        for session in self.sessions.values_mut() {
            session.input = None;
        }
    }

    fn start_next(&mut self, cx: &mut Context<Self>) {
        if self.compilation_task.is_some() {
            return;
        }
        let entry = self
            .focused
            .as_ref()
            .filter(|entry| self.pending.contains_key(*entry))
            .cloned()
            .or_else(|| self.pending.keys().next().cloned());
        let Some(entry) = entry else { return };
        let Some(request) = self.pending.remove(&entry) else {
            return;
        };
        let generation = request.input.generation;
        let buffers = request.buffers.clone();
        let snapshot = self.snapshot(request, cx);
        self.compilation_task = Some(cx.spawn(async move |this, cx| {
            let result = async {
                let snapshot = snapshot.await?;
                Ok::<_, anyhow::Error>(
                    cx.background_spawn(async move { Arc::new(snapshot.compile()) })
                        .await,
                )
            }
            .await;
            this.update(cx, |this, cx| {
                this.compilation_task = None;
                let current = this
                    .sessions
                    .get(&entry)
                    .is_some_and(|session| session.generation == generation);
                if !current {
                    this.start_next(cx);
                    return;
                }
                let Some(session) = this.sessions.get_mut(&entry) else {
                    return;
                };
                match result {
                    Ok(compilation) => {
                        if compilation.document.is_some() {
                            session.last_good = Some(compilation.clone());
                            session.last_good_buffers = buffers.clone();
                        }
                        session.error = None;
                        session.latest_buffers = buffers.clone();
                        session.latest = Some(compilation);
                    }
                    Err(error) => {
                        session.latest = None;
                        session.error = Some(format!("{error:#}"));
                    }
                }
                let project = this.project.clone();
                cx.defer(move |cx| {
                    project
                        .update(cx, |project, cx| project.publish_typst_diagnostics(cx))
                        .log_err();
                });
                cx.emit(Updated { entry });
                cx.notify();
                this.start_next(cx);
            })
            .log_err();
        }));
    }
}

impl Project {
    pub fn signature_help<T: language::ToPointUtf16>(
        &self,
        buffer: &Entity<Buffer>,
        position: T,
        cx: &mut Context<Self>,
    ) -> Task<Option<Vec<crate::lsp_command::SignatureHelp>>> {
        let position = position.to_point_utf16(buffer.read(cx));
        if !self.is_native_typst(buffer, cx) {
            return self
                .lsp_store
                .update(cx, |store, cx| store.signature_help(buffer, position, cx));
        }
        let Some(request) = self.typst_request(buffer, cx).log_err() else {
            return Task::ready(None);
        };
        let offset = position.to_offset(buffer.read(cx));
        let path = request.path.clone();
        let snapshot = self.typst_store(cx).read(cx).snapshot(request, cx);
        cx.spawn(async move |_, cx| {
            let snapshot = snapshot.await.log_err()?;
            let signature = cx
                .background_spawn(async move { snapshot.signature(&path, offset) })
                .await
                .log_err()??;
            Some(vec![cx.update(|cx| {
                crate::lsp_command::SignatureHelp::native(signature, cx)
            })])
        })
    }

    pub(crate) fn typst_format(
        &self,
        buffers: collections::HashSet<Entity<Buffer>>,
        target: &crate::lsp_store::LspFormatTarget,
        push_to_history: bool,
        trigger: crate::lsp_store::FormatTrigger,
        cx: &mut Context<Self>,
    ) -> Task<Result<ProjectTransaction>> {
        let inputs = buffers
            .into_iter()
            .filter_map(|buffer| {
                let snapshot = buffer.read(cx).snapshot();
                let settings = snapshot.settings_at(0, cx);
                if trigger == crate::lsp_store::FormatTrigger::Save
                    && settings.format_on_save == FormatOnSave::Off
                {
                    return None;
                }
                let ranges = match target {
                    crate::lsp_store::LspFormatTarget::Buffers => vec![None],
                    crate::lsp_store::LspFormatTarget::Ranges(ranges) => ranges
                        .get(&snapshot.remote_id())
                        .into_iter()
                        .flatten()
                        .map(|range| {
                            Some(range.start.to_offset(&snapshot)..range.end.to_offset(&snapshot))
                        })
                        .collect(),
                };
                let width = settings.preferred_line_length as usize;
                Some((buffer, snapshot.text(), ranges, width))
            })
            .collect::<Vec<_>>();
        let task = cx.background_spawn(async move {
            inputs
                .into_iter()
                .map(|(buffer, text, ranges, width)| {
                    let mut edits = ranges
                        .into_iter()
                        .map(|range| typst_engine::format(&text, width, range))
                        .collect::<Result<Vec<_>>>()?;
                    edits.sort_by_key(|(range, _)| range.start);
                    edits.dedup_by(|right, left| right.0 == left.0);
                    ensure!(
                        edits
                            .windows(2)
                            .all(|pair| pair[0].0.end <= pair[1].0.start),
                        "Typst formatting selections overlap"
                    );
                    let edits = edits
                        .into_iter()
                        .map(|(range, formatted)| {
                            let original = text
                                .get(range.clone())
                                .context("Invalid Typst formatting edit range")?;
                            Ok(language::text_diff(original, &formatted)
                                .into_iter()
                                .map(|(edit, replacement)| {
                                    (
                                        edit.start + range.start..edit.end + range.start,
                                        replacement,
                                    )
                                })
                                .collect::<Vec<_>>())
                        })
                        .collect::<Result<Vec<_>>>()?
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>();
                    Ok((buffer, text, edits))
                })
                .collect::<Result<Vec<_>>>()
        });
        cx.spawn(async move |_, cx| {
            let edits = task.await?;
            cx.update(|cx| {
                for (buffer, original, _) in &edits {
                    ensure!(
                        buffer.read(cx).text() == *original,
                        "Typst file changed during formatting; try again"
                    );
                }
                let mut transaction = ProjectTransaction::default();
                for (buffer, _, edits) in edits {
                    buffer.update(cx, |buffer_value, cx| {
                        buffer_value.finalize_last_transaction();
                        buffer_value.start_transaction();
                        buffer_value.edit(edits, None, cx);
                        if let Some(transaction_id) = buffer_value.end_transaction(cx) {
                            if let Some(edit) = buffer_value.get_transaction(transaction_id) {
                                transaction.0.insert(buffer.clone(), edit.clone());
                            }
                            if !push_to_history {
                                buffer_value.forget_transaction(transaction_id);
                            }
                        }
                        buffer_value.finalize_last_transaction();
                    });
                }
                Ok(transaction)
            })
        })
    }

    pub fn is_native_typst(&self, buffer: &Entity<Buffer>, cx: &App) -> bool {
        self.is_local()
            && buffer
                .read(cx)
                .language()
                .is_some_and(|language| language.name().as_ref() == "Typst")
    }

    pub fn typst_store(&self, cx: &mut Context<Self>) -> Entity<TypstStore> {
        self.typst_store
            .get_or_init(|| {
                let project = cx.weak_entity();
                cx.new(|cx| TypstStore::new(project, cx))
            })
            .clone()
    }

    pub fn typst_request(
        &self,
        buffer: &Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> Result<Request> {
        ensure!(
            self.is_native_typst(buffer, cx),
            "Native Typst requires a local Typst file"
        );
        let file = crate::File::from_dyn(buffer.read(cx).file())
            .context("Save this Typst file before compiling it")?;
        let path = file.abs_path(cx);
        let worktree_id = file.worktree_id(cx);
        let worktree = self
            .worktree_for_id(file.worktree_id(cx), cx)
            .context("Missing Typst worktree")?;
        let worktree_root = worktree.read(cx).abs_path();
        let worktree_root = if worktree.read(cx).is_single_file() {
            worktree_root
                .parent()
                .context("Missing Typst parent directory")?
                .to_owned()
        } else {
            worktree_root.to_path_buf()
        };
        let mut configuration = TypstSettings::get(
            Some(SettingsLocation {
                worktree_id: file.worktree_id(cx),
                path: file.path(),
            }),
            cx,
        )
        .0
        .clone();
        if let Some(trusted) = TrustedWorktrees::try_get_global(cx) {
            let can_trust = trusted.update(cx, |trusted, cx| {
                trusted.can_trust(&self.worktree_store, worktree_id, cx)
            });
            if !can_trust {
                configuration = Configuration {
                    system_fonts: false,
                    package_downloads: false,
                    ..Default::default()
                };
            }
        }
        let root = configuration
            .root
            .as_ref()
            .map(|root| util::paths::normalize_lexically(&worktree_root.join(root)))
            .transpose()?
            .unwrap_or(worktree_root);
        ensure!(
            path.starts_with(&root),
            "The Typst file is outside the configured compiler root"
        );
        let entry = configuration
            .main_file
            .as_ref()
            .map(|entry| util::paths::normalize_lexically(&root.join(entry)))
            .transpose()?
            .unwrap_or_else(|| path.clone());
        let buffers = self
            .opened_buffers(cx)
            .into_iter()
            .filter_map(|buffer| {
                let buffer = buffer.read(cx);
                let file = crate::File::from_dyn(buffer.file())?;
                let path = file.abs_path(cx);
                path.starts_with(&root).then(|| (path, buffer.snapshot()))
            })
            .collect::<BTreeMap<_, _>>();
        let files = buffers
            .iter()
            .map(|(path, snapshot)| (path.clone(), Arc::from(snapshot.text())))
            .collect();
        let known_files = worktree
            .read(cx)
            .snapshot()
            .entries(false, 0)
            .filter(|entry| entry.is_file())
            .map(|entry| worktree.read(cx).abs_path().join(entry.path.as_std_path()))
            .filter(|path| path.starts_with(&root))
            .collect();
        Ok(Request {
            input: Input {
                root,
                entry,
                generation: 0,
                files,
                known_files,
            },
            path,
            configuration,
            files: Arc::new(ProjectFiles(self.fs.clone())),
            buffers,
        })
    }

    pub fn refresh_typst(&self, cx: &mut Context<Self>) {
        let mut requests = BTreeMap::new();
        for buffer in self.opened_buffers(cx) {
            if self.is_native_typst(&buffer, cx) {
                match self.typst_request(&buffer, cx) {
                    Ok(request) => {
                        requests.insert(request.input.entry.clone(), request);
                    }
                    Err(error) => log::debug!("Native Typst: {error:#}"),
                }
            }
        }
        if requests.is_empty() && self.typst_store.get().is_none() {
            return;
        }
        self.typst_store(cx).update(cx, |store, cx| {
            let open_paths = requests
                .values()
                .flat_map(|request| request.input.files.keys())
                .collect::<std::collections::BTreeSet<_>>();
            store
                .sessions
                .retain(|entry, _| requests.contains_key(entry) || open_paths.contains(entry));
            store
                .engines
                .lock()
                .retain(|root, _| requests.values().any(|request| &request.input.root == root));
            store
                .pending
                .retain(|entry, _| store.sessions.contains_key(entry));
            for request in requests.into_values() {
                store.refresh(request, cx);
            }
        });
        self.publish_typst_diagnostics(cx);
    }

    fn publish_typst_diagnostics(&self, cx: &mut Context<Self>) {
        let Some(store) = self.typst_store.get() else {
            return;
        };
        let diagnostics = store.read(cx).diagnostics();
        let mut changed_paths = Vec::new();
        for buffer in self.opened_buffers(cx) {
            let snapshot = buffer.read(cx).snapshot();
            let Some(file) = crate::File::from_dyn(snapshot.file()) else {
                continue;
            };
            let path = file.abs_path(cx);
            let mut entries = diagnostics
                .get(&path)
                .into_iter()
                .flatten()
                .enumerate()
                .filter_map(|(index, diagnostic)| {
                    let location = diagnostic.location.as_ref()?;
                    let source = store
                        .read(cx)
                        .sessions
                        .values()
                        .find_map(|session| {
                            let compilation = session.latest.as_ref()?;
                            if !compilation.diagnostics.iter().any(|entry| {
                                entry.location == diagnostic.location
                                    && entry.message == diagnostic.message
                            }) {
                                return None;
                            }
                            session
                                .latest_buffers
                                .get(&path)
                                .filter(|source| source.remote_id() == snapshot.remote_id())
                        })
                        .unwrap_or(&snapshot);
                    let message = if diagnostic.hints.is_empty() {
                        diagnostic.message.clone()
                    } else {
                        format!("{}\n{}", diagnostic.message, diagnostic.hints.join("\n"))
                    };
                    Some(DiagnosticEntry::new(
                        source.anchor_before(location.range.start)
                            ..source.anchor_after(location.range.end),
                        Diagnostic {
                            source: Some("Typst".into()),
                            message: DiagnosticMessage::plain(message),
                            severity: if diagnostic.warning {
                                lsp::DiagnosticSeverity::WARNING
                            } else {
                                lsp::DiagnosticSeverity::ERROR
                            },
                            group_id: usize::MAX - index,
                            is_primary: true,
                            ..Default::default()
                        },
                    ))
                })
                .collect::<Vec<_>>();
            entries.sort_by(|left, right| left.range.start.cmp(&right.range.start, &snapshot));
            let diagnostics = (!entries.is_empty())
                .then(|| DiagnosticSet::from_sorted_entries(entries, &snapshot));
            buffer.update(cx, |buffer, cx| {
                buffer.update_native_diagnostics(diagnostics, cx)
            });
            if let Some(path) = self.project_path_for_absolute_path(&path, cx) {
                changed_paths.push(path);
            }
        }
        changed_paths.extend(
            diagnostics
                .keys()
                .filter_map(|path| self.project_path_for_absolute_path(path, cx)),
        );
        changed_paths.sort();
        changed_paths.dedup();
        cx.emit(crate::Event::NativeDiagnosticsUpdated {
            paths: changed_paths,
        });
    }

    pub fn native_diagnostic_summaries(
        &self,
        include_ignored: bool,
        cx: &App,
    ) -> Vec<(crate::ProjectPath, crate::DiagnosticSummary)> {
        self.typst_store
            .get()
            .map(|store| {
                store
                    .read(cx)
                    .diagnostic_summaries(self, include_ignored, cx)
            })
            .unwrap_or_default()
    }

    pub(crate) fn typst_completions(
        &self,
        buffer: &Entity<Buffer>,
        position: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<CompletionResponse>>> {
        let request = match self.typst_request(buffer, cx) {
            Ok(request) => request,
            Err(error) => return Task::ready(Err(error)),
        };
        let path = request.path.clone();
        let text_snapshot = buffer.read(cx).snapshot();
        let store = self.typst_store(cx);
        let compilation = store.read(cx).pages(&request.input.entry);
        let snapshot = store.read(cx).snapshot(request, cx);
        cx.background_spawn(async move {
            let snapshot = snapshot.await?;
            let Some((start, completions)) = snapshot.completions(
                compilation
                    .as_ref()
                    .and_then(|compilation| compilation.document.as_ref()),
                &path,
                position,
                true,
            )?
            else {
                return Ok(Vec::new());
            };
            let completions = completions
                .into_iter()
                .map(|completion| {
                    let label = completion.label.to_string();
                    let (new_text, snippet) = completion
                        .apply
                        .as_ref()
                        .map(|apply| completion_snippet(apply.as_str()))
                        .unwrap_or_else(|| (label.clone(), false));
                    Completion {
                        replace_range: text_snapshot.anchor_before(start)
                            ..text_snapshot.anchor_after(position),
                        new_text,
                        label: CodeLabel::plain(label, None),
                        documentation: completion.detail.map(|detail| {
                            CompletionDocumentation::SingleLine(detail.to_string().into())
                        }),
                        source: CompletionSource::Native { snippet },
                        icon_path: None,
                        icon_color: None,
                        match_start: Some(text_snapshot.anchor_before(start)),
                        snippet_deduplication_key: None,
                        insert_text_mode: None,
                        confirm: None,
                        group: None,
                    }
                })
                .collect();
            Ok(vec![CompletionResponse {
                completions,
                display_options: CompletionDisplayOptions::default(),
                is_incomplete: true,
            }])
        })
    }

    pub(crate) fn typst_hover(
        &self,
        buffer: &Entity<Buffer>,
        position: usize,
        cx: &mut Context<Self>,
    ) -> Task<Option<Vec<Hover>>> {
        let Some(request) = self.typst_request(buffer, cx).log_err() else {
            return Task::ready(None);
        };
        let Some(file) = crate::File::from_dyn(buffer.read(cx).file()) else {
            return Task::ready(None);
        };
        let path = file.abs_path(cx);
        let language = buffer.read(cx).language().cloned();
        let store = self.typst_store(cx);
        let compilation = store.read(cx).pages(&request.input.entry);
        let snapshot = store.read(cx).snapshot(request, cx);
        cx.background_spawn(async move {
            let snapshot = snapshot.await.log_err()?;
            let tooltip = snapshot
                .hover(
                    compilation
                        .as_ref()
                        .and_then(|compilation| compilation.document.as_ref()),
                    &path,
                    position,
                )
                .log_err()??;
            let (text, kind) = match tooltip {
                typst_engine::Tooltip::Text(text) => (text.to_string(), HoverBlockKind::PlainText),
                typst_engine::Tooltip::Code(text) => (
                    text.to_string(),
                    HoverBlockKind::Code {
                        language: "Typst".into(),
                    },
                ),
            };
            Some(vec![Hover {
                contents: vec![HoverBlock { text, kind }],
                range: None,
                language,
            }])
        })
    }

    pub(crate) fn typst_locations(
        &self,
        buffer: &Entity<Buffer>,
        position: usize,
        references: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<Vec<Location>>>> {
        let request = match self.typst_request(buffer, cx) {
            Ok(request) => request,
            Err(error) => return Task::ready(Err(error)),
        };
        let path = request.path.clone();
        let buffers = request.buffers.clone();
        let store = self.typst_store(cx);
        let compilation = store.read(cx).pages(&request.input.entry);
        let snapshot = store.read(cx).snapshot(request, cx);
        cx.spawn(async move |project, cx| {
            let snapshot = snapshot.await?;
            let locations = cx
                .background_spawn(async move {
                    if references {
                        let index = references::Index::new(&snapshot)?;
                        if index.target(&path, position).is_err() {
                            return Ok(Vec::new());
                        }
                        index.references(&path, position)
                    } else {
                        Ok(snapshot
                            .definition(
                                compilation
                                    .as_ref()
                                    .and_then(|compilation| compilation.document.as_ref()),
                                &path,
                                position,
                            )?
                            .into_iter()
                            .collect())
                    }
                })
                .await?;
            let mut result = Vec::new();
            for location in locations {
                let buffer = project
                    .update(cx, |project, cx| {
                        project.open_local_buffer(&location.path, cx)
                    })?
                    .await?;
                let range = buffer.read_with(cx, |buffer, _| {
                    let current = buffer.snapshot();
                    let source = buffers
                        .get(&location.path)
                        .filter(|source| source.remote_id() == current.remote_id())
                        .unwrap_or(&current);
                    let start = source.clip_offset(location.range.start, text::Bias::Left);
                    let end = source.clip_offset(location.range.end, text::Bias::Right);
                    source.anchor_before(start)..source.anchor_after(end.max(start))
                });
                result.push(Location { buffer, range });
            }
            Ok(Some(result))
        })
    }

    pub(crate) fn typst_prepare_rename(
        &self,
        buffer: Entity<Buffer>,
        position: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<PrepareRenameResponse>> {
        let request = match self.typst_request(&buffer, cx) {
            Ok(request) => request,
            Err(error) => return Task::ready(Err(error)),
        };
        let path = request.path.clone();
        let buffer_snapshot = buffer.read(cx).snapshot();
        let snapshot = self.typst_store(cx).read(cx).snapshot(request, cx);
        cx.background_spawn(async move {
            let snapshot = snapshot.await?;
            let index = references::Index::new(&snapshot)?;
            let Ok(range) = index.range(&path, position) else {
                return Ok(PrepareRenameResponse::InvalidPosition);
            };
            let rename = index.rename(
                &snapshot,
                &path,
                position,
                &snapshot.source_at(&path)?.text()[range.clone()],
            );
            if rename.is_err() {
                return Ok(PrepareRenameResponse::InvalidPosition);
            }
            Ok(PrepareRenameResponse::Success {
                range: buffer_snapshot.anchor_before(range.start)
                    ..buffer_snapshot.anchor_after(range.end),
                language_server_id: None,
            })
        })
    }

    pub(crate) fn typst_rename(
        &self,
        buffer: Entity<Buffer>,
        position: usize,
        name: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<ProjectTransaction>> {
        let request = match self.typst_request(&buffer, cx) {
            Ok(request) => request,
            Err(error) => return Task::ready(Err(error)),
        };
        let path = request.path.clone();
        let snapshot = self.typst_store(cx).read(cx).snapshot(request, cx);
        cx.spawn(async move |project, cx| {
            let snapshot = snapshot.await?;
            let (snapshot, edits) = cx
                .background_spawn(async move {
                    let edits = references::Index::new(&snapshot)?
                        .rename(&snapshot, &path, position, &name)?;
                    Ok::<_, anyhow::Error>((snapshot, edits))
                })
                .await?;
            let mut grouped = BTreeMap::<PathBuf, Vec<(Range<usize>, String)>>::new();
            for edit in edits {
                grouped
                    .entry(edit.location.path)
                    .or_default()
                    .push((edit.location.range, edit.text));
            }
            let mut buffers = Vec::new();
            for (path, edits) in grouped {
                let buffer = project
                    .update(cx, |project, cx| project.open_local_buffer(&path, cx))?
                    .await?;
                buffers.push((buffer, snapshot.source_at(&path)?.text().to_owned(), edits));
            }
            project.update(cx, |_, cx| {
                for (buffer, original, _) in &buffers {
                    ensure!(
                        buffer.read(cx).text() == *original,
                        "Typst files changed during rename; try again"
                    );
                }
                let mut transaction = ProjectTransaction::default();
                for (buffer, _, edits) in buffers {
                    buffer.update(cx, |buffer_value, cx| {
                        buffer_value.finalize_last_transaction();
                        buffer_value.start_transaction();
                        buffer_value.edit(edits, None, cx);
                        if let Some(transaction_id) = buffer_value.end_transaction(cx)
                            && let Some(edit) = buffer_value.get_transaction(transaction_id)
                        {
                            transaction.0.insert(buffer.clone(), edit.clone());
                        }
                        buffer_value.finalize_last_transaction();
                    });
                }
                Ok(transaction)
            })?
        })
    }
}

fn completion_snippet(source: &str) -> (String, bool) {
    let mut output = String::new();
    let mut remaining = source;
    let mut index = 1;
    while let Some(start) = remaining.find("${") {
        let Some(end) = remaining[start + 2..].find('}') else {
            break;
        };
        output.push_str(&remaining[..start].replace('$', "\\$"));
        let label = &remaining[start + 2..start + 2 + end];
        output.push_str(&format!("${{{index}:{label}}}"));
        remaining = &remaining[start + 3 + end..];
        index += 1;
    }
    if index == 1 {
        return (source.to_owned(), false);
    }
    output.push_str(&remaining.replace('$', "\\$"));
    (output, true)
}
