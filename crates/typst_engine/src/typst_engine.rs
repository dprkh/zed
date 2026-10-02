use anyhow::{Context as _, Result, anyhow, ensure};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    any::Any,
    collections::{BTreeMap, HashMap},
    io,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use typst::{
    Library, LibraryExt, World, WorldExt,
    diag::{FileError, FileResult, Severity, SourceDiagnostic},
    foundations::{Bytes, Datetime, Duration, Value},
    introspection::PagedPosition,
    layout::{Abs, Point},
    syntax::{
        FileId, LinkedNode, RootedPath, Side, Source, VirtualPath, VirtualRoot, ast, ast::AstNode,
    },
    text::{Font, FontBook},
    utils::LazyHash,
};
pub use typst_ide::Tooltip;
use typst_ide::{Completion, Definition, IdeWorld, Jump};
use typst_kit::{
    downloader::{Downloader, SystemDownloader},
    files::FsRoot,
    fonts::{self, FontStore},
    packages::SystemPackages,
};
use typst_layout::PagedDocument;

pub mod references;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Configuration {
    pub root: Option<PathBuf>,
    pub main_file: Option<PathBuf>,
    pub font_paths: Vec<PathBuf>,
    pub inputs: BTreeMap<String, String>,
    pub system_fonts: bool,
    pub package_downloads: bool,
}

impl Default for Configuration {
    fn default() -> Self {
        Self {
            root: None,
            main_file: None,
            font_paths: Vec::new(),
            inputs: BTreeMap::new(),
            system_fonts: true,
            package_downloads: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Input {
    pub root: PathBuf,
    pub entry: PathBuf,
    pub generation: u64,
    pub files: BTreeMap<PathBuf, Arc<str>>,
    pub known_files: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub path: PathBuf,
    pub range: Range<usize>,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub location: Option<Location>,
    pub message: String,
    pub warning: bool,
    pub hints: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Signature {
    pub label: String,
    pub parameters: Vec<Range<usize>>,
    pub active_parameter: usize,
    pub documentation: Option<String>,
}

pub trait FileProvider: Send + Sync {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
}

pub struct DiskFiles;

impl FileProvider for DiskFiles {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }
}

struct PackageDownloader {
    enabled: bool,
    downloader: SystemDownloader,
}

impl Downloader for PackageDownloader {
    fn stream(&self, key: &dyn Any, url: &str) -> io::Result<(Option<usize>, Box<dyn io::Read>)> {
        if !self.enabled {
            return Err(io::Error::other("Typst package downloads are disabled"));
        }
        self.downloader.stream(key, url)
    }
}

pub struct Engine {
    configuration: Configuration,
    fonts: Arc<FontStore>,
    packages: Arc<SystemPackages>,
    library: LazyHash<Library>,
    sources: Arc<Mutex<HashMap<FileId, Source>>>,
    files: Arc<dyn FileProvider>,
}

impl Engine {
    pub fn new(configuration: Configuration, root: &Path) -> Self {
        Self::with_files(configuration, root, Arc::new(DiskFiles))
    }

    pub fn with_files(
        configuration: Configuration,
        root: &Path,
        files: Arc<dyn FileProvider>,
    ) -> Self {
        let mut fonts = FontStore::new();
        // Project fonts win over fonts with the same family installed on the machine.
        for directory in &configuration.font_paths {
            fonts.extend(fonts::scan(&root.join(directory)));
        }
        if configuration.system_fonts {
            fonts.extend(fonts::system());
        }
        fonts.extend(fonts::embedded());
        let inputs = configuration
            .inputs
            .iter()
            .map(|(key, value)| (key.as_str().into(), Value::Str(value.as_str().into())))
            .collect();
        let library = LazyHash::new(Library::builder().with_inputs(inputs).build());
        let packages = SystemPackages::new(PackageDownloader {
            enabled: configuration.package_downloads,
            downloader: SystemDownloader::new("Zed native Typst"),
        });
        Self {
            configuration,
            fonts: Arc::new(fonts),
            packages: Arc::new(packages),
            library,
            sources: Arc::default(),
            files,
        }
    }

    pub fn configuration(&self) -> &Configuration {
        &self.configuration
    }

    pub fn snapshot(&self, input: Input) -> Result<Arc<Snapshot>> {
        let main = file_id(&input.root, &input.entry)?;
        Ok(Arc::new(Snapshot {
            input,
            main,
            fonts: self.fonts.clone(),
            packages: self.packages.clone(),
            library: self.library.clone(),
            source_cache: self.sources.clone(),
            sources: Mutex::default(),
            bytes: Mutex::default(),
            files: self.files.clone(),
            time: Arc::new(typst_kit::datetime::Time::system()),
        }))
    }
}

pub struct Snapshot {
    pub input: Input,
    main: FileId,
    fonts: Arc<FontStore>,
    packages: Arc<SystemPackages>,
    library: LazyHash<Library>,
    source_cache: Arc<Mutex<HashMap<FileId, Source>>>,
    sources: Mutex<HashMap<FileId, Source>>,
    bytes: Mutex<HashMap<FileId, FileResult<Bytes>>>,
    files: Arc<dyn FileProvider>,
    time: Arc<typst_kit::datetime::Time>,
}

pub struct Compilation {
    pub snapshot: Arc<Snapshot>,
    pub document: Option<PagedDocument>,
    pub diagnostics: Vec<Diagnostic>,
    page_sizes: Vec<(f64, f64)>,
    page_fingerprints: Vec<u128>,
}

pub struct RasterizedPage {
    pub width: u32,
    pub height: u32,
    /// Premultiplied RGBA pixels, as returned by Typst's renderer.
    pub pixels: Vec<u8>,
}

impl Snapshot {
    pub fn with_files(&self, files: BTreeMap<PathBuf, Arc<str>>) -> Self {
        let mut input = self.input.clone();
        input.files.extend(files);
        Self {
            input,
            main: self.main,
            fonts: self.fonts.clone(),
            packages: self.packages.clone(),
            library: self.library.clone(),
            source_cache: self.source_cache.clone(),
            sources: Mutex::default(),
            bytes: Mutex::default(),
            files: self.files.clone(),
            time: self.time.clone(),
        }
    }

    pub fn compile(self: &Arc<Self>) -> Compilation {
        comemo::evict(10);
        let result = typst::compile::<PagedDocument>(self.as_ref());
        let mut diagnostics = result
            .warnings
            .iter()
            .map(|d| self.diagnostic(d))
            .collect::<Vec<_>>();
        let document = match result.output {
            Ok(document) => Some(document),
            Err(errors) => {
                diagnostics.extend(errors.iter().map(|d| self.diagnostic(d)));
                None
            }
        };
        let (page_sizes, page_fingerprints) = document
            .as_ref()
            .into_iter()
            .flat_map(|document| document.pages())
            .map(|page| {
                let size = page.frame.size();
                (
                    (size.x.to_pt(), size.y.to_pt()),
                    typst::utils::hash128(page),
                )
            })
            .unzip();
        Compilation {
            snapshot: self.clone(),
            document,
            diagnostics,
            page_sizes,
            page_fingerprints,
        }
    }

    pub fn path(&self, id: FileId) -> FileResult<PathBuf> {
        let root = match id.root() {
            VirtualRoot::Project => FsRoot::new(self.input.root.clone()),
            VirtualRoot::Package(specification) => self.packages.obtain(specification)?,
        };
        root.resolve(id.vpath())
    }

    pub fn is_package_path(&self, path: &Path) -> bool {
        self.packages
            .data()
            .into_iter()
            .chain(self.packages.cache())
            .any(|packages| path.starts_with(packages.path()))
    }

    pub fn source_at(&self, path: &Path) -> Result<Source> {
        self.source(file_id(&self.input.root, path)?)
            .map_err(|error| anyhow!(error.to_string()))
    }

    pub fn location(&self, span: typst::syntax::Span) -> Option<Location> {
        Some(Location {
            path: self.path(span.id()?).ok()?,
            range: self.range(span)?,
        })
    }

    pub fn dependencies(&self) -> Vec<PathBuf> {
        let ids = self.bytes.lock().keys().copied().collect::<Vec<_>>();
        ids.into_iter()
            .filter_map(|id| self.path(id).ok())
            .collect()
    }

    fn diagnostic(&self, diagnostic: &SourceDiagnostic) -> Diagnostic {
        Diagnostic {
            location: diagnostic.span.id().and_then(|id| {
                Some(Location {
                    path: self.path(id).ok()?,
                    range: self.range(diagnostic.span)?,
                })
            }),
            message: diagnostic.message.to_string(),
            warning: diagnostic.severity == Severity::Warning,
            hints: diagnostic
                .hints
                .iter()
                .map(|hint| hint.v.to_string())
                .collect(),
        }
    }

    pub fn completions(
        &self,
        document: Option<&PagedDocument>,
        path: &Path,
        cursor: usize,
        explicit: bool,
    ) -> Result<Option<(usize, Vec<Completion>)>> {
        let source = self.source_at(path)?;
        ensure!(
            source.text().is_char_boundary(cursor),
            "Invalid Typst cursor position"
        );
        Ok(typst_ide::autocomplete(
            self, document, &source, cursor, explicit,
        ))
    }

    pub fn hover(
        &self,
        document: Option<&PagedDocument>,
        path: &Path,
        cursor: usize,
    ) -> Result<Option<Tooltip>> {
        let source = self.source_at(path)?;
        Ok(typst_ide::tooltip(
            self,
            document,
            &source,
            cursor,
            Side::After,
        ))
    }

    pub fn definition(
        &self,
        document: Option<&PagedDocument>,
        path: &Path,
        cursor: usize,
    ) -> Result<Option<Location>> {
        let source = self.source_at(path)?;
        Ok(
            match typst_ide::definition(self, document, &source, cursor, Side::After) {
                Some(Definition::Span(span)) => self.location(span),
                Some(Definition::File(id)) => Some(Location {
                    path: self.path(id)?,
                    range: 0..0,
                }),
                _ => None,
            },
        )
    }

    pub fn signature(&self, path: &Path, cursor: usize) -> Result<Option<Signature>> {
        let source = self.source_at(path)?;
        let root = LinkedNode::new(source.root());
        let Some(mut node) = root.leaf_at(cursor, Side::Before) else {
            return Ok(None);
        };
        loop {
            if let Some(call) = node.cast::<ast::FuncCall>() {
                let callee_span = call.callee().span();
                let Some(callee) = node.children().find(|child| child.span() == callee_span) else {
                    return Ok(None);
                };
                let values = typst_ide::analyze_expr(self, &callee);
                let Some((Value::Func(function), _)) = values.first() else {
                    return Ok(None);
                };
                let mut label = format!("{}(", function.name().unwrap_or("function"));
                let mut parameters = Vec::new();
                let infos = function.params().collect::<Vec<_>>();
                for (index, parameter) in infos.iter().enumerate() {
                    if index > 0 {
                        label.push_str(", ");
                    }
                    let start = label.len();
                    if parameter.variadic() {
                        label.push_str("..");
                    }
                    label.push_str(parameter.name().unwrap_or("arguments"));
                    if parameter.named() {
                        label.push_str(": …");
                    }
                    parameters.push(start..label.len());
                }
                label.push(')');
                let arguments = node
                    .children()
                    .find(|child| child.cast::<ast::Args>().is_some());
                let active_parameter = arguments
                    .as_ref()
                    .map(|arguments| {
                        let preceding = arguments
                            .children()
                            .filter(|child| {
                                child.kind() == typst::syntax::SyntaxKind::Comma
                                    && child.range().end <= cursor
                            })
                            .count();
                        let named = arguments
                            .children()
                            .find(|child| child.range().contains(&cursor))
                            .and_then(|child| {
                                child
                                    .cast::<ast::Named>()
                                    .map(|named| named.name().get().to_string())
                            });
                        named
                            .and_then(|name| {
                                infos.iter().position(|info| info.name() == Some(&name))
                            })
                            .unwrap_or(preceding)
                            .min(parameters.len().saturating_sub(1))
                    })
                    .unwrap_or(0);
                return Ok(Some(Signature {
                    label,
                    parameters,
                    active_parameter,
                    documentation: function.docs().map(str::to_owned),
                }));
            }
            let Some(parent) = node.parent() else {
                return Ok(None);
            };
            node = parent.clone();
        }
    }
}

impl Compilation {
    pub fn page_sizes(&self) -> &[(f64, f64)] {
        &self.page_sizes
    }

    pub fn page_fingerprint(&self, page: usize) -> Option<u128> {
        self.page_fingerprints.get(page).copied()
    }

    pub fn rasterize(&self, page: usize, scale: f32) -> Result<RasterizedPage> {
        ensure!(
            scale.is_finite() && scale > 0.0,
            "Invalid Typst raster scale"
        );
        let page = self
            .document
            .as_ref()
            .and_then(|document| document.pages().get(page))
            .context("The requested Typst page is unavailable")?;
        let size = page.frame.size();
        let width = size.x.to_pt().max(1.0);
        let height = size.y.to_pt().max(1.0);
        // A zoomed page can be much larger than the viewport. Bound the backing
        // image while retaining the full page's layout and navigation coordinates.
        let scale = f64::from(scale)
            .min((16_777_216.0 / (width * height)).sqrt())
            .min(16_384.0 / width.max(height));
        let pixmap = typst_render::render(
            page,
            &typst_render::RenderOptions {
                pixel_per_pt: typst::utils::Scalar::new(scale),
                render_bleed: false,
            },
        );
        Ok(RasterizedPage {
            width: pixmap.width(),
            height: pixmap.height(),
            pixels: pixmap.take(),
        })
    }

    pub fn svg(&self, page: usize) -> Result<String> {
        let page = self
            .document
            .as_ref()
            .and_then(|document| document.pages().get(page))
            .context("The requested Typst page is unavailable")?;
        Ok(typst_svg::svg(page, &typst_svg::SvgOptions::default()))
    }

    pub fn jump_from_page(&self, page: usize, x: f64, y: f64) -> Option<Navigation> {
        let document = self.document.as_ref()?;
        let position = PagedPosition {
            page: std::num::NonZeroUsize::new(page + 1)?,
            point: Point::new(Abs::pt(x), Abs::pt(y)),
        };
        match typst_ide::jump_from_click(self.snapshot.as_ref(), document, &position)? {
            Jump::File(id, offset) => Some(Navigation::Source(Location {
                path: self.snapshot.path(id).ok()?,
                range: offset..offset,
            })),
            Jump::Position(position) => Some(Navigation::Page {
                page: position.page.get() - 1,
                x: position.point.x.to_pt(),
                y: position.point.y.to_pt(),
            }),
            Jump::Url(url) => Some(Navigation::Url(url.to_string())),
        }
    }

    pub fn jump_from_source(&self, path: &Path, cursor: usize) -> Option<(usize, f64, f64)> {
        let source = self.snapshot.source_at(path).ok()?;
        let position = typst_ide::jump_from_cursor(self.document.as_ref()?, &source, cursor)
            .into_iter()
            .next()?;
        Some((
            position.page.get() - 1,
            position.point.x.to_pt(),
            position.point.y.to_pt(),
        ))
    }
}

#[derive(Clone, Debug)]
pub enum Navigation {
    Source(Location),
    Page { page: usize, x: f64, y: f64 },
    Url(String),
}

impl IdeWorld for Snapshot {
    fn upcast(&self) -> &dyn World {
        self
    }

    fn files(&self) -> Vec<FileId> {
        self.input
            .known_files
            .iter()
            .chain(self.input.files.keys())
            .filter_map(|path| file_id(&self.input.root, path).ok())
            .collect()
    }
}

impl World for Snapshot {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }
    fn book(&self) -> &LazyHash<FontBook> {
        self.fonts.book()
    }
    fn main(&self) -> FileId {
        self.main
    }
    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.font(index)
    }
    fn today(&self, offset: Option<Duration>) -> Option<Datetime> {
        self.time.today(offset)
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if let Some(source) = self.sources.lock().get(&id) {
            return Ok(source.clone());
        }
        let bytes = self.file(id)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| FileError::InvalidUtf8)?;
        let source = {
            let mut cache = self.source_cache.lock();
            let source = cache
                .entry(id)
                .or_insert_with(|| Source::new(id, text.to_owned()));
            if source.text() != text {
                source.replace(text);
            }
            source.clone()
        };
        self.sources.lock().insert(id, source.clone());
        Ok(source)
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        let mut bytes = self.bytes.lock();
        if let Some(bytes) = bytes.get(&id) {
            return bytes.clone();
        }
        let path = self.path(id)?;
        let value = match self.input.files.get(&path) {
            Some(text) => Ok(Bytes::new(text.as_bytes().to_vec())),
            None => self
                .files
                .read(&path)
                .map(Bytes::new)
                .map_err(|error| FileError::from_io(error, &path)),
        };
        bytes.insert(id, value.clone());
        value
    }
}

pub fn file_id(root: &Path, path: &Path) -> Result<FileId> {
    let path = path
        .strip_prefix(root)
        .context("Typst file is outside the project root")?;
    let path = VirtualPath::new(path.to_string_lossy().as_ref())
        .map_err(|error| anyhow!(error.to_string()))?;
    Ok(FileId::new(RootedPath::new(VirtualRoot::Project, path)))
}

pub fn format(
    text: &str,
    width: usize,
    range: Option<Range<usize>>,
) -> Result<(Range<usize>, String)> {
    let configuration = typstyle_core::Config {
        max_width: width,
        ..Default::default()
    };
    let formatter = typstyle_core::Typstyle::new(configuration);
    ensure!(
        range.as_ref().is_none_or(|range| range.start <= range.end
            && range.end <= text.len()
            && text.is_char_boundary(range.start)
            && text.is_char_boundary(range.end)),
        "Invalid Typst formatting range"
    );
    let source = Source::detached(text);
    if let Some(range) = range {
        let formatted = formatter
            .format_source_range(source, range)
            .map_err(|error| anyhow!(error.to_string()))?;
        Ok((formatted.source_range, formatted.content))
    } else {
        let formatted = formatter
            .format_source(source)
            .render()
            .map_err(|error| anyhow!(error.to_string()))?;
        Ok((0..text.len(), formatted))
    }
}
