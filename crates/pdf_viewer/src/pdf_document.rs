use anyhow::{Context as _, Result, bail};
use collections::HashMap;
use gpui::{
    App, AppContext, Context, Entity, EntityId, EventEmitter, Global, Subscription, Task,
    WeakEntity,
};
use hayro::{
    RenderCache, RenderSettings,
    hayro_interpret::{
        self as interpret, Device,
        font::Glyph,
        hayro_cmap::BfString,
        hayro_syntax::{
            LoadPdfError, Pdf,
            object::{Array, Dict, Name, Object, String as PdfString},
        },
        util::TransformExt,
    },
};
use kurbo::{Affine, BezPath, Point, Rect, Shape};
use parking_lot::Mutex;
use project::{Project, ProjectEntryId, ProjectPath};
use std::{ops::Range, path::PathBuf, sync::Arc, time::Duration};
use util::ResultExt as _;
use worktree::PathChange;

pub(crate) struct ParsedPdf {
    pub pdf: Pdf,
    pub sizes: Vec<(f32, f32)>,
    pub raster_limit: async_lock::Semaphore,
    text: Mutex<HashMap<usize, Arc<TextPage>>>,
}

impl ParsedPdf {
    pub fn parse(bytes: Arc<Vec<u8>>, password: &str) -> std::result::Result<Self, LoadPdfError> {
        let pdf = Pdf::new_with_password(bytes, password)?;
        let sizes = pdf
            .pages()
            .iter()
            .map(|page| page.render_dimensions())
            .collect();
        Ok(Self {
            pdf,
            sizes,
            raster_limit: async_lock::Semaphore::new(2),
            text: Mutex::new(HashMap::default()),
        })
    }

    pub fn text_page(&self, page: usize) -> Result<Arc<TextPage>> {
        if let Some(text) = self.text.lock().get(&page).cloned() {
            return Ok(text);
        }
        let source = self
            .pdf
            .pages()
            .get(page)
            .context("PDF page no longer exists")?;
        let cache = interpret::InterpreterCache::new();
        let settings = interpret::InterpreterSettings {
            render_annotations: false,
            ..Default::default()
        };
        let transform = source.initial_transform(true).to_kurbo();
        let (width, height) = source.render_dimensions();
        let mut context = interpret::Context::new(
            transform,
            Rect::new(0.0, 0.0, width as f64, height as f64),
            &cache,
            source.xref(),
            settings,
        );
        let mut device = TextDevice::default();
        interpret::interpret_page(source, &mut context, &mut device);
        let crop = Rect::new(0.0, 0.0, width as f64, height as f64);
        device.glyphs.retain_mut(|glyph| {
            glyph.bounds = glyph.bounds.intersect(crop);
            glyph.bounds.area() > 0.0
        });
        let mut text = TextPage::from_glyphs(device.glyphs);
        text.links = self.links(page);
        let text = Arc::new(text);
        self.text.lock().insert(page, text.clone());
        Ok(text)
    }

    pub fn rasterize(&self, page: usize, scale: f32) -> Result<image::RgbaImage> {
        let source = self
            .pdf
            .pages()
            .get(page)
            .context("PDF page no longer exists")?;
        let (width, height) = source.render_dimensions();
        // Bound raster allocations independently of the zoom control: PDFs may have enormous media boxes.
        let scale = scale
            .min(8192.0 / width.max(height))
            .min((8_000_000.0 / (width * height)).sqrt());
        if !scale.is_finite() || scale <= 0.0 {
            bail!("Invalid PDF page dimensions");
        }
        let settings = RenderSettings {
            x_scale: scale,
            y_scale: scale,
            width: Some((width * scale).ceil().max(1.0) as u16),
            height: Some((height * scale).ceil().max(1.0) as u16),
            bg_color: hayro::vello_cpu::color::palette::css::WHITE,
        };
        let raster = hayro::render(source, &RenderCache::new(), &Default::default(), &settings);
        image::RgbaImage::from_raw(
            raster.width().into(),
            raster.height().into(),
            raster.data_as_u8_slice().to_vec(),
        )
        .context("Invalid PDF raster")
    }

    fn links(&self, page: usize) -> Vec<PdfLink> {
        let Some(source) = self.pdf.pages().get(page) else {
            return Vec::new();
        };
        source
            .raw()
            .get::<Array<'_>>(b"Annots")
            .into_iter()
            .flat_map(|annotations| annotations.iter::<Dict<'_>>())
            .filter_map(|annotation| {
                if annotation.get::<Name<'_>>(b"Subtype")?.as_ref() != b"Link" {
                    return None;
                }
                let bounds = annotation.get::<hayro::hayro_syntax::object::Rect>(b"Rect")?;
                let bounds = source
                    .initial_transform(true)
                    .to_kurbo()
                    .transform_rect_bbox(Rect::new(bounds.x0, bounds.y0, bounds.x1, bounds.y1));
                let target = if let Some(action) = annotation.get::<Dict<'_>>(b"A") {
                    match action.get::<Name<'_>>(b"S")?.as_ref() {
                        b"URI" => {
                            let url = pdf_string(&action.get::<PdfString<'_>>(b"URI")?);
                            let scheme = url.split(':').next()?.to_ascii_lowercase();
                            if !matches!(scheme.as_str(), "http" | "https" | "mailto") {
                                return None;
                            }
                            LinkTarget::Url(url)
                        }
                        b"GoTo" => self.destination(action.get::<Object<'_>>(b"D")?, 0)?,
                        _ => return None,
                    }
                } else {
                    self.destination(annotation.get::<Object<'_>>(b"Dest")?, 0)?
                };
                Some(PdfLink { bounds, target })
            })
            .collect()
    }

    fn destination(&self, destination: Object<'_>, depth: usize) -> Option<LinkTarget> {
        if depth > 16 {
            return None;
        }
        match destination {
            Object::Array(array) => {
                let mut entries = array.iter::<Object<'_>>();
                let first = entries.next()?;
                let page = match first {
                    Object::Dict(dict) => self
                        .pdf
                        .pages()
                        .iter()
                        .position(|page| page.raw().obj_id() == dict.obj_id())?,
                    Object::Number(number) => usize::try_from(number.as_i64()).ok()?,
                    _ => return None,
                };
                let source = self.pdf.pages().get(page)?;
                let kind = entries.next()?.into_name()?;
                let coordinates = entries.collect::<Vec<_>>();
                let y = if kind.as_ref() == b"XYZ" {
                    let left = coordinates
                        .first()
                        .and_then(|value| value.clone().into_f32())
                        .unwrap_or(source.crop_box().x0 as f32);
                    let top = coordinates
                        .get(1)
                        .and_then(|value| value.clone().into_f32())
                        .unwrap_or(source.crop_box().y1 as f32);
                    (source.initial_transform(true).to_kurbo()
                        * Point::new(left as f64, top as f64))
                    .y
                } else if kind.as_ref() == b"FitH" || kind.as_ref() == b"FitBH" {
                    let top = coordinates
                        .first()
                        .and_then(|value| value.clone().into_f32())
                        .unwrap_or(source.crop_box().y1 as f32);
                    (source.initial_transform(true).to_kurbo()
                        * Point::new(source.crop_box().x0, top as f64))
                    .y
                } else {
                    0.0
                };
                Some(LinkTarget::Page {
                    page,
                    y: y.max(0.0),
                })
            }
            Object::Dict(dict) => self.destination(dict.get::<Object<'_>>(b"D")?, depth + 1),
            name @ (Object::Name(_) | Object::String(_)) => {
                let name = match name {
                    Object::Name(name) => name.as_ref().to_vec(),
                    Object::String(string) => string.as_bytes().to_vec(),
                    _ => return None,
                };
                let root = self.pdf.xref().get::<Dict<'_>>(self.pdf.xref().root_id())?;
                if let Some(destination) = root
                    .get::<Dict<'_>>(b"Dests")
                    .and_then(|destinations| destinations.get::<Object<'_>>(&name))
                {
                    return self.destination(destination, depth + 1);
                }
                let names = root.get::<Dict<'_>>(b"Names")?.get::<Dict<'_>>(b"Dests")?;
                self.destination(named_destination(names, &name, 0)?, depth + 1)
            }
            _ => None,
        }
    }
}

fn named_destination<'a>(tree: Dict<'a>, name: &[u8], depth: usize) -> Option<Object<'a>> {
    if depth > 16 {
        return None;
    }
    if let Some(names) = tree.get::<Array<'a>>(b"Names") {
        let mut entries = names.iter::<Object<'a>>();
        while let (Some(key), Some(value)) = (entries.next(), entries.next()) {
            if key.into_string().is_some_and(|key| key.as_bytes() == name) {
                return Some(value);
            }
        }
    }
    tree.get::<Array<'a>>(b"Kids")?
        .iter::<Dict<'a>>()
        .find_map(|child| named_destination(child, name, depth + 1))
}

fn pdf_string(string: &PdfString<'_>) -> String {
    let bytes = string.as_bytes();
    if let Some(bytes) = bytes.strip_prefix(&[0xfe, 0xff]) {
        String::from_utf16_lossy(
            &bytes
                .chunks_exact(2)
                .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>(),
        )
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum LinkTarget {
    Page { page: usize, y: f64 },
    Url(String),
}
#[derive(Clone)]
pub(crate) struct PdfLink {
    pub bounds: Rect,
    pub target: LinkTarget,
}
#[derive(Clone)]
pub(crate) struct TextGlyph {
    pub text: String,
    pub bounds: Rect,
    pub origin: Point,
    pub advance: Point,
    pub range: Range<usize>,
}
#[derive(Default)]
pub(crate) struct TextPage {
    pub text: String,
    pub glyphs: Vec<TextGlyph>,
    pub links: Vec<PdfLink>,
}

impl TextPage {
    pub(crate) fn from_glyphs(mut glyphs: Vec<TextGlyph>) -> Self {
        let direction = glyphs.first().map(|glyph| glyph.advance - glyph.origin);
        let flow = direction.map_or(Affine::IDENTITY, |direction| {
            Affine::rotate(-direction.y.atan2(direction.x))
        });
        order_blocks(&mut glyphs, 0, flow);
        let mut page = Self::default();
        for mut glyph in glyphs {
            if let Some(previous) = page.glyphs.last() {
                if previous.text == glyph.text && (previous.origin - glyph.origin).hypot() < 0.01 {
                    continue;
                }
                let direction = previous.advance - previous.origin;
                let distance = glyph.origin - previous.origin;
                let length = direction.hypot().max(0.1);
                let across = (distance.x * direction.y - distance.y * direction.x).abs() / length;
                let along = (distance.x * direction.x + distance.y * direction.y) / length;
                let em = previous
                    .bounds
                    .height()
                    .max(previous.bounds.width().min(length * 2.0))
                    .max(1.0);
                if across > em * 0.6 || along < -em || along > em * 8.0 {
                    if !page.text.ends_with('\n') {
                        page.text.push('\n');
                    }
                } else if along - length > em * 0.15
                    && !page.text.ends_with(char::is_whitespace)
                    && !glyph.text.starts_with(char::is_whitespace)
                {
                    page.text.push(' ');
                }
            }
            glyph.range = page.text.len()..page.text.len() + glyph.text.len();
            page.text.push_str(&glyph.text);
            page.glyphs.push(glyph);
        }
        page
    }

    pub fn hit(&self, position: Point) -> Option<usize> {
        self.glyphs
            .iter()
            .min_by(|left, right| {
                let distance = |glyph: &TextGlyph| {
                    let x = (glyph.bounds.x0 - position.x)
                        .max(position.x - glyph.bounds.x1)
                        .max(0.0);
                    let y = (glyph.bounds.y0 - position.y)
                        .max(position.y - glyph.bounds.y1)
                        .max(0.0);
                    x * x + y * y
                };
                distance(left).total_cmp(&distance(right))
            })
            .map(|glyph| {
                let direction = glyph.advance - glyph.origin;
                let distance = position - glyph.bounds.center();
                if distance.x * direction.x + distance.y * direction.y > 0.0 {
                    glyph.range.end
                } else {
                    glyph.range.start
                }
            })
    }

    pub fn highlights(&self, range: &Range<usize>) -> impl Iterator<Item = Rect> + '_ {
        let range = range.clone();
        self.glyphs
            .iter()
            .filter(move |glyph| glyph.range.start < range.end && glyph.range.end > range.start)
            .map(|glyph| glyph.bounds)
    }
}

fn order_blocks(glyphs: &mut [TextGlyph], depth: usize, flow: Affine) {
    if glyphs.len() < 2 || depth > 32 {
        return;
    }
    let mut heights = glyphs
        .iter()
        .map(|glyph| flow.transform_rect_bbox(glyph.bounds).height())
        .filter(|height| *height > 0.0)
        .collect::<Vec<_>>();
    heights.sort_by(f64::total_cmp);
    let em = heights
        .get(heights.len() / 2)
        .copied()
        .unwrap_or(10.0)
        .max(1.0);
    let mut best = None;
    for horizontal in [true, false] {
        let mut intervals = glyphs
            .iter()
            .map(|glyph| {
                let bounds = flow.transform_rect_bbox(glyph.bounds);
                if horizontal {
                    (bounds.x0, bounds.x1)
                } else {
                    (bounds.y0, bounds.y1)
                }
            })
            .collect::<Vec<_>>();
        intervals.sort_by(|left, right| left.0.total_cmp(&right.0));
        let mut end = intervals.first().map_or(0.0, |interval| interval.1);
        for (start, next_end) in intervals.into_iter().skip(1) {
            let threshold = if horizontal { em * 2.0 } else { em * 0.8 };
            let score = (start - end) / threshold;
            if score > 1.0 && best.is_none_or(|(_, _, previous)| score > previous) {
                best = Some((horizontal, (start + end) / 2.0, score));
            }
            end = end.max(next_end);
        }
    }
    let Some((horizontal, cut, _)) = best else {
        return;
    };
    // Stable partition preserves the PDF's character order inside each spatial block.
    glyphs.sort_by_key(|glyph| {
        let bounds = flow.transform_rect_bbox(glyph.bounds);
        if horizontal {
            bounds.x0 >= cut
        } else {
            bounds.y0 >= cut
        }
    });
    let middle = glyphs.partition_point(|glyph| {
        let bounds = flow.transform_rect_bbox(glyph.bounds);
        if horizontal {
            bounds.x0 < cut
        } else {
            bounds.y0 < cut
        }
    });
    let (first, second) = glyphs.split_at_mut(middle);
    order_blocks(first, depth + 1, flow);
    order_blocks(second, depth + 1, flow);
}

#[derive(Default)]
struct TextDevice {
    glyphs: Vec<TextGlyph>,
}
impl Device<'_> for TextDevice {
    fn draw_glyph(
        &mut self,
        glyph: &Glyph<'_>,
        transform: Affine,
        glyph_transform: Affine,
        _: &interpret::Paint<'_>,
        _: &interpret::GlyphDrawMode,
    ) {
        let Some(unicode) = glyph.as_unicode() else {
            return;
        };
        let text = match unicode {
            BfString::Char(character) => character.to_string(),
            BfString::String(text) => text,
        };
        let transform = transform * glyph_transform;
        let origin = transform * Point::ZERO;
        let (bounds, width) = match glyph {
            Glyph::Outline(glyph) => {
                let width = glyph.advance_width().unwrap_or(500.0) as f64;
                let outline = glyph.outline();
                let mut bounds = outline.bounding_box();
                if bounds.width() < 0.1 || bounds.height() < 0.1 {
                    bounds = Rect::new(0.0, -200.0, width.max(1.0), 800.0);
                }
                (bounds, width)
            }
            Glyph::Type3(_) => (Rect::new(0.0, -200.0, 500.0, 800.0), 500.0),
        };
        let bounds = transform.transform_rect_bbox(bounds);
        if !bounds.is_finite() || text.is_empty() {
            return;
        }
        self.glyphs.push(TextGlyph {
            text,
            bounds,
            origin,
            advance: transform * Point::new(width, 0.0),
            range: 0..0,
        });
    }
    fn set_soft_mask(&mut self, _: Option<interpret::SoftMask<'_>>) {}
    fn set_blend_mode(&mut self, _: interpret::BlendMode) {}
    fn draw_path(
        &mut self,
        _: &BezPath,
        _: Affine,
        _: &interpret::Paint<'_>,
        _: &interpret::PathDrawMode,
    ) {
    }
    fn push_clip_path(&mut self, _: &interpret::ClipPath) {}
    fn push_transparency_group(
        &mut self,
        _: f32,
        _: Option<interpret::SoftMask<'_>>,
        _: interpret::BlendMode,
    ) {
    }
    fn draw_image(&mut self, _: interpret::Image<'_, '_>, _: Affine) {}
    fn pop_clip_path(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

pub(crate) struct PdfDocument {
    pub file: Arc<worktree::File>,
    pub parsed: Option<Arc<ParsedPdf>>,
    pub generation: u64,
    pub error: Option<String>,
    pub password_required: bool,
    password: String,
    bytes: Arc<Vec<u8>>,
    _subscription: Subscription,
    _reload: Option<Task<()>>,
}

#[derive(Clone)]
pub(crate) struct DocumentChanged;
impl EventEmitter<DocumentChanged> for PdfDocument {}

#[derive(Default)]
struct Documents(HashMap<(EntityId, ProjectPath), WeakEntity<PdfDocument>>);
impl Global for Documents {}

impl PdfDocument {
    pub fn open(
        project: Entity<Project>,
        path: ProjectPath,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let key = (project.entity_id(), path.clone());
        if let Some(document) = find_open_document(project.entity_id(), &path, cx) {
            return Task::ready(Ok(document));
        }
        cx.spawn(async move |cx| {
            let (worktree, loading) = project.update(cx, |project, cx| {
                let worktree = project
                    .worktree_for_id(path.worktree_id, cx)
                    .context("PDF worktree is unavailable")?;
                let loading =
                    worktree.update(cx, |worktree, cx| worktree.load_binary_file(&path.path, cx));
                anyhow::Ok((worktree, loading))
            })?;
            let loaded = loading.await?;
            let bytes = Arc::new(loaded.content);
            let result = cx
                .background_spawn({
                    let bytes = bytes.clone();
                    async move { ParsedPdf::parse(bytes, "") }
                })
                .await;
            cx.update(|cx| {
                if let Some(document) = find_open_document(project.entity_id(), &path, cx) {
                    return Ok(document);
                }
                let document = cx.new(|cx| {
                    let subscription =
                        cx.subscribe(&worktree, |document: &mut Self, worktree, event, cx| {
                            if let worktree::Event::UpdatedEntries(entries) = event {
                                let snapshot = worktree.read(cx).snapshot();
                                let previous = document.file.clone();
                                if let Some(entry) = document
                                    .file
                                    .entry_id
                                    .and_then(|id| snapshot.entry_for_id(id))
                                    .or_else(|| snapshot.entry_for_path(&document.file.path))
                                {
                                    document.file =
                                        worktree::File::for_entry(entry.clone(), worktree);
                                }
                                if entries.iter().any(|(path, id, _)| {
                                    document.file.entry_id == Some(*id)
                                        || path == &document.file.path
                                }) {
                                    if entries.iter().any(|(_, id, change)| {
                                        document.file.entry_id == Some(*id)
                                            && *change == PathChange::Removed
                                    }) && document
                                        .file
                                        .entry_id
                                        .and_then(|id| snapshot.entry_for_id(id))
                                        .is_none()
                                    {
                                        Arc::make_mut(&mut document.file).disk_state =
                                            language::DiskState::Deleted;
                                        document.error = Some("PDF file was deleted".into());
                                        cx.emit(DocumentChanged);
                                        cx.notify();
                                    } else if previous != document.file {
                                        document.reload(cx);
                                    }
                                }
                            }
                        });
                    let mut document = Self {
                        file: loaded.file,
                        parsed: None,
                        generation: 0,
                        error: None,
                        password_required: false,
                        password: String::new(),
                        bytes,
                        _subscription: subscription,
                        _reload: None,
                    };
                    document.install(result, cx);
                    document
                });
                cx.default_global::<Documents>()
                    .0
                    .retain(|_, document| document.is_upgradable());
                cx.default_global::<Documents>()
                    .0
                    .insert(key, document.downgrade());
                Ok(document)
            })
        })
    }

    fn install(
        &mut self,
        result: std::result::Result<ParsedPdf, LoadPdfError>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(parsed) => {
                self.parsed = Some(Arc::new(parsed));
                self.generation += 1;
                self.error = None;
                self.password_required = false;
            }
            Err(LoadPdfError::Decryption(_)) => {
                self.password_required = true;
                self.error =
                    Some("This PDF needs a password, or the password was incorrect.".into());
            }
            Err(LoadPdfError::Invalid) => {
                self.error =
                    Some("Unable to read this PDF. The file is invalid or incomplete.".into());
            }
        }
        cx.emit(DocumentChanged);
        cx.notify();
    }

    pub fn unlock(&mut self, password: String, cx: &mut Context<Self>) {
        self.password = password;
        let bytes = self.bytes.clone();
        let password = self.password.clone();
        let task = cx.background_spawn(async move { ParsedPdf::parse(bytes, &password) });
        self._reload = Some(cx.spawn(async move |document, cx| {
            let result = task.await;
            document
                .update(cx, |document, cx| document.install(result, cx))
                .log_err();
        }));
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let worktree = self.file.worktree.clone();
        let path = self.file.path.clone();
        let password = self.password.clone();
        self._reload = Some(cx.spawn(async move |document, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let result = async {
                let loading =
                    worktree.update(cx, |worktree, cx| worktree.load_binary_file(&path, cx));
                let loaded = loading.await?;
                let bytes = Arc::new(loaded.content);
                let parsed = cx
                    .background_spawn({
                        let bytes = bytes.clone();
                        async move { ParsedPdf::parse(bytes, &password) }
                    })
                    .await;
                anyhow::Ok((loaded.file, bytes, parsed))
            }
            .await;
            document
                .update(cx, |document, cx| match result {
                    Ok((file, bytes, parsed)) => {
                        document.file = file;
                        document.bytes = bytes;
                        document.install(parsed, cx);
                    }
                    Err(error) => {
                        document.error = Some(format!("Unable to reload PDF: {error}"));
                        cx.emit(DocumentChanged);
                        cx.notify();
                    }
                })
                .log_err();
        }));
    }

    pub fn path(&self, cx: &App) -> PathBuf {
        self.file.worktree.read(cx).absolutize(&self.file.path)
    }
}

impl project::ProjectItem for PdfDocument {
    fn try_open(
        _: &Entity<Project>,
        _: &ProjectPath,
        _: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        None
    }
    fn entry_id(&self, _: &App) -> Option<ProjectEntryId> {
        self.file.entry_id
    }
    fn project_path(&self, cx: &App) -> Option<ProjectPath> {
        Some(ProjectPath {
            worktree_id: self.file.worktree_id(cx),
            path: self.file.path.clone(),
        })
    }
    fn is_dirty(&self) -> bool {
        false
    }
}

fn find_open_document(
    project_id: EntityId,
    path: &ProjectPath,
    cx: &mut App,
) -> Option<Entity<PdfDocument>> {
    let existing = cx
        .default_global::<Documents>()
        .0
        .iter()
        .filter(|((candidate, _), _)| *candidate == project_id)
        .filter_map(|(_, document)| document.upgrade())
        .collect::<Vec<_>>();
    existing.into_iter().find(|document| {
        let file = &document.read(cx).file;
        file.worktree_id(cx) == path.worktree_id && file.path == path.path
    })
}
