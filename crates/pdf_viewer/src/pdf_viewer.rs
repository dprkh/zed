mod pdf_document;
#[cfg(test)]
mod tests;

use anyhow::{Context as _, Result};
use collections::HashMap;
use editor::Editor;
use gpui::{
    App, AppContext, Bounds, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, KeyBinding, ListAlignment, ListOffset, ListState, MouseButton, Render,
    RenderImage, ScrollHandle, Subscription, Task, WeakEntity, Window, actions, div, img, list,
    point, px, size,
};
use language::File as _;
use pdf_document::{DocumentChanged, LinkTarget, PdfDocument, TextPage};
use project::{Project, ProjectPath, search::SearchQuery};
use serde::{Deserialize, Serialize};
use settings::SeedQuerySetting;
use std::{ops::Range, path::PathBuf, sync::Arc};
use ui::{ContextMenu, TintColor, Tooltip, prelude::*, right_click_menu};
use util::ResultExt as _;
use workspace::{
    ItemId, Workspace, WorkspaceId, WorkspaceItemBuilder,
    item::{Item, ItemBufferKind, ItemEvent, SerializableItem},
    searchable::{
        Direction, SearchEvent, SearchOptions, SearchToken, SearchableItem, SearchableItemHandle,
    },
};

const PAGE_SPACING: f32 = 24.0;
const CACHE_BYTES: usize = 64 * 1024 * 1024;

actions!(
    pdf_viewer,
    [
        /// Zoom in the PDF.
        ZoomIn,
        /// Zoom out the PDF.
        ZoomOut,
        /// Show the PDF at 100%.
        ActualSize,
        /// Fit PDF pages to the pane width.
        FitWidth,
        /// Fit the current PDF page in the pane.
        FitPage,
        /// Go to the next PDF page.
        NextPage,
        /// Go to the previous PDF page.
        PreviousPage,
        /// Copy selected PDF text.
        Copy,
        /// Select all text in the PDF.
        SelectAll,
        /// Clear the PDF text selection.
        ClearSelection,
        /// Scroll down in the PDF.
        ScrollDown,
        /// Scroll up in the PDF.
        ScrollUp,
        /// Go to the first PDF page.
        FirstPage,
        /// Go to the last PDF page.
        LastPage,
    ]
);

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
enum ZoomMode {
    Width,
    Page,
    Custom,
}
#[derive(Clone, Serialize, Deserialize)]
struct ViewState {
    path: PathBuf,
    zoom: f32,
    mode: ZoomMode,
    page: usize,
    offset: f32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TextPosition {
    page: usize,
    offset: usize,
}
#[derive(Clone, Debug)]
pub struct PdfMatch {
    page: usize,
    range: Range<usize>,
}
struct CachedPage {
    generation: u64,
    scale: u32,
    image: Arc<RenderImage>,
    bytes: usize,
    used: u64,
}

pub struct PdfView {
    document: Entity<PdfDocument>,
    project: Entity<Project>,
    focus: FocusHandle,
    state: ViewState,
    list: ListState,
    horizontal_scroll: ScrollHandle,
    viewport: Option<gpui::Size<gpui::Pixels>>,
    scale: f32,
    generation: u64,
    pages: HashMap<usize, CachedPage>,
    tasks: HashMap<usize, Task<()>>,
    text: HashMap<usize, Arc<TextPage>>,
    selection: Option<(TextPosition, TextPosition)>,
    dragging: bool,
    link_down: Option<(gpui::Point<gpui::Pixels>, LinkTarget)>,
    hovered_link: Option<(usize, LinkTarget)>,
    matches: Vec<PdfMatch>,
    active_match: Option<usize>,
    page_input: Entity<Editor>,
    password_input: Entity<Editor>,
    raster_error: Option<SharedString>,
    clock: u64,
    _subscription: Subscription,
}

#[derive(Clone)]
pub struct StateChanged;
impl EventEmitter<StateChanged> for PdfView {}
impl EventEmitter<SearchEvent> for PdfView {}

pub fn init(cx: &mut App) {
    workspace::register_serializable_item::<PdfView>(cx);
    workspace::register_project_path_opener(
        |project, path, window, cx| {
            if !project.read(cx).is_local()
                || !path
                    .path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
            {
                return None;
            }
            let project = project.clone();
            let document = PdfDocument::open(project.clone(), path.clone(), cx);
            Some(window.spawn(cx, async move |cx| {
                let document = document.await?;
                cx.update(|window, cx| {
                    let entry = document.read(cx).file.entry_id;
                    let view = cx.new(|cx| PdfView::new(document, project, None, window, cx));
                    anyhow::Ok((entry, WorkspaceItemBuilder::new(move |_, _, _| view)))
                })?
            }))
        },
        cx,
    );
    let modifier = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    cx.bind_keys([
        KeyBinding::new(&format!("{modifier}-+"), ZoomIn, Some("PdfViewer")),
        KeyBinding::new(&format!("{modifier}-="), ZoomIn, Some("PdfViewer")),
        KeyBinding::new(&format!("{modifier}--"), ZoomOut, Some("PdfViewer")),
        KeyBinding::new(&format!("{modifier}-0"), ActualSize, Some("PdfViewer")),
        KeyBinding::new(&format!("{modifier}-c"), Copy, Some("PdfViewer")),
        KeyBinding::new(&format!("{modifier}-a"), SelectAll, Some("PdfViewer")),
        KeyBinding::new("escape", ClearSelection, Some("PdfViewer")),
        KeyBinding::new("pagedown", NextPage, Some("PdfViewer")),
        KeyBinding::new("pageup", PreviousPage, Some("PdfViewer")),
        KeyBinding::new("down", ScrollDown, Some("PdfViewer")),
        KeyBinding::new("up", ScrollUp, Some("PdfViewer")),
        KeyBinding::new("home", FirstPage, Some("PdfViewer")),
        KeyBinding::new("end", LastPage, Some("PdfViewer")),
    ]);
}

impl PdfView {
    fn new(
        document: Entity<PdfDocument>,
        project: Entity<Project>,
        state: Option<ViewState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = state.unwrap_or_else(|| ViewState {
            path: document.read(cx).path(cx),
            zoom: 1.0,
            mode: ZoomMode::Width,
            page: 0,
            offset: 0.0,
        });
        let count = document
            .read(cx)
            .parsed
            .as_ref()
            .map_or(0, |parsed| parsed.sizes.len());
        let list = ListState::new(count, ListAlignment::Top, px(256.0));
        list.scroll_to(ListOffset {
            item_ix: state.page.min(count.saturating_sub(1)),
            offset_in_item: px(state.offset),
        });
        let view = cx.weak_entity();
        list.set_scroll_handler(move |event, window, cx| {
            let page = event.visible_range.start.min(event.count.saturating_sub(1));
            let view = view.clone();
            window.defer(cx, move |window, cx| {
                view.update(cx, |view, cx| {
                    view.state.page = page;
                    let offset = view.list.logical_scroll_top();
                    view.state.offset = f32::from(offset.offset_in_item);
                    if !view.page_input.focus_handle(cx).is_focused(window) {
                        let value = (view.state.page + 1).to_string();
                        view.page_input
                            .update(cx, |editor, cx| editor.set_text(value, window, cx));
                    }
                    cx.emit(StateChanged);
                    cx.notify();
                })
                .log_err();
            });
        });
        let subscription = cx.subscribe_in(
            &document,
            window,
            |view, _, _: &DocumentChanged, window, cx| view.document_changed(window, cx),
        );
        let page_input = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(
                if count == 0 {
                    "0".to_string()
                } else {
                    (state.page + 1).to_string()
                },
                window,
                cx,
            );
            editor
        });
        let password_input = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_masked(true, cx);
            editor.set_placeholder_text("PDF password", window, cx);
            editor
        });
        let generation = document.read(cx).generation;
        let mut view = Self {
            document,
            project,
            focus: cx.focus_handle(),
            state,
            list,
            horizontal_scroll: ScrollHandle::default(),
            viewport: None,
            scale: 1.0,
            generation,
            pages: HashMap::default(),
            tasks: HashMap::default(),
            text: HashMap::default(),
            selection: None,
            dragging: false,
            link_down: None,
            hovered_link: None,
            matches: Vec::new(),
            active_match: None,
            page_input,
            password_input,
            raster_error: None,
            clock: 0,
            _subscription: subscription,
        };
        view.state.page = view.state.page.min(count.saturating_sub(1));
        cx.on_release_in(window, |view, window, _| {
            for (_, page) in view.pages.drain() {
                window.drop_image(page.image).log_err();
            }
        })
        .detach();
        view
    }

    fn document_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.state.path = self.document.read(cx).path(cx);
        let generation = self.document.read(cx).generation;
        if generation != self.generation {
            self.generation = generation;
            self.tasks.clear();
            self.text.clear();
            self.selection = None;
            self.dragging = false;
            self.link_down = None;
            self.hovered_link = None;
            self.matches.clear();
            self.active_match = None;
            self.raster_error = None;
            let count = self
                .document
                .read(cx)
                .parsed
                .as_ref()
                .map_or(0, |parsed| parsed.sizes.len());
            self.list.splice(0..self.list.item_count(), count);
            self.state.page = self.state.page.min(count.saturating_sub(1));
            self.list.scroll_to(ListOffset {
                item_ix: self.state.page,
                offset_in_item: px(self.state.offset),
            });
            self.pages.retain(|page, image| {
                if *page < count {
                    true
                } else {
                    window.drop_image(image.image.clone()).log_err();
                    false
                }
            });
            cx.emit(SearchEvent::MatchesInvalidated);
        }
        cx.emit(StateChanged);
        cx.notify();
    }

    fn change_page(&mut self, page: usize, y: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.state.page = page.min(self.list.item_count().saturating_sub(1));
        self.state.offset = y;
        self.list.scroll_to(ListOffset {
            item_ix: self.state.page,
            offset_in_item: px(y),
        });
        self.page_input.update(cx, |editor, cx| {
            editor.set_text(
                if self.list.item_count() == 0 {
                    "0".to_string()
                } else {
                    (self.state.page + 1).to_string()
                },
                window,
                cx,
            )
        });
        cx.emit(StateChanged);
        cx.notify();
    }

    fn zoom(&mut self, factor: f32, cx: &mut Context<Self>) {
        self.state.zoom = (self.scale * factor).clamp(0.1, 8.0);
        self.state.mode = ZoomMode::Custom;
        cx.emit(StateChanged);
        cx.notify();
    }

    fn raster_page(&mut self, page: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.clock += 1;
        let scale = (self.scale * window.scale_factor()).to_bits();
        if let Some(cached) = self.pages.get_mut(&page) {
            cached.used = self.clock;
            if cached.generation == self.generation && cached.scale == scale {
                return;
            }
        }
        if self.tasks.contains_key(&page) || self.tasks.len() >= 2 {
            return;
        }
        let Some(parsed) = self.document.read(cx).parsed.clone() else {
            return;
        };
        let generation = self.generation;
        let rendering = cx.background_spawn(async move {
            let _permit = parsed.raster_limit.acquire().await;
            let text = parsed.text_page(page)?;
            let mut raster = parsed.rasterize(page, f32::from_bits(scale))?;
            let bytes = raster.as_raw().len();
            for pixel in raster.chunks_exact_mut(4) {
                gpui::swap_rgba_pa_to_bgra(pixel);
            }
            anyhow::Ok((
                text,
                bytes,
                Arc::new(RenderImage::new(vec![image::Frame::new(raster)])),
            ))
        });
        let task = cx.spawn_in(window, async move |view, cx| {
            let result = rendering.await;
            view.update_in(cx, |view, window, cx| {
                view.tasks.remove(&page);
                if view.generation != generation
                    || (view.scale * window.scale_factor()).to_bits() != scale
                {
                    cx.notify();
                    return;
                }
                match result {
                    Ok((text, bytes, image)) => {
                        view.text.insert(page, text);
                        if let Some(previous) = view.pages.insert(
                            page,
                            CachedPage {
                                generation,
                                scale,
                                image,
                                bytes,
                                used: view.clock,
                            },
                        ) {
                            window.drop_image(previous.image).log_err();
                        }
                        let visible = view.list.viewport_bounds();
                        while view.pages.values().map(|page| page.bytes).sum::<usize>()
                            > CACHE_BYTES
                        {
                            let oldest = view
                                .pages
                                .iter()
                                .filter(|(page, _)| {
                                    view.list
                                        .bounds_for_item(**page)
                                        .is_none_or(|bounds| !bounds.intersects(&visible))
                                })
                                .min_by_key(|(_, image)| image.used)
                                .map(|(page, _)| *page);
                            let Some(oldest) = oldest else {
                                break;
                            };
                            if let Some(image) = view.pages.remove(&oldest) {
                                window.drop_image(image.image).log_err();
                            }
                        }
                    }
                    Err(error) => {
                        view.raster_error =
                            Some(format!("Unable to render PDF page {}: {error}", page + 1).into())
                    }
                }
                cx.notify();
            })
            .log_err();
        });
        self.tasks.insert(page, task);
    }

    fn selection_range(&self, page: usize) -> Option<Range<usize>> {
        let (start, end) = self.selection?;
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        if page < start.page || page > end.page {
            return None;
        }
        Some(
            (if page == start.page { start.offset } else { 0 })..(if page == end.page {
                end.offset
            } else {
                usize::MAX
            }),
        )
    }

    fn selected_text(&self) -> String {
        let mut selected = Vec::new();
        for page in 0..self.list.item_count() {
            if let Some(range) = self.selection_range(page)
                && let Some(text) = self.text.get(&page)
            {
                selected.push(
                    text.text
                        .get(range.start.min(text.text.len())..range.end.min(text.text.len()))
                        .unwrap_or_default(),
                );
            }
        }
        selected.join("\n")
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let Some(parsed) = self.document.read(cx).parsed.clone() else {
            return;
        };
        let ranges = (0..self.list.item_count())
            .filter_map(|page| {
                self.selection_range(page)
                    .filter(|range| !range.is_empty())
                    .map(|range| (page, range))
            })
            .collect::<Vec<_>>();
        if ranges.is_empty() {
            return;
        }
        let copying = cx.background_spawn(async move {
            let mut text = Vec::new();
            for (page, range) in ranges {
                let page = parsed.text_page(page)?;
                text.push(
                    page.text
                        .get(range.start.min(page.text.len())..range.end.min(page.text.len()))
                        .unwrap_or_default()
                        .to_string(),
                );
            }
            anyhow::Ok(text.join("\n"))
        });
        cx.spawn(async move |view, cx| {
            match copying.await {
                Ok(text) => cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string(text))),
                Err(error) => view.update(cx, |view, cx| {
                    view.raster_error = Some(format!("Unable to copy PDF text: {error}").into());
                    cx.notify();
                })?,
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn unlock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let password = self.password_input.read(cx).text(cx);
        self.password_input
            .update(cx, |editor, cx| editor.set_text("", window, cx));
        self.document
            .update(cx, |document, cx| document.unlock(password, cx));
        self.focus.focus(window, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selection = Some((
            TextPosition { page: 0, offset: 0 },
            TextPosition {
                page: self.list.item_count().saturating_sub(1),
                offset: usize::MAX,
            },
        ));
        cx.notify();
    }

    fn pointer_position(
        &self,
        page: usize,
        position: gpui::Point<gpui::Pixels>,
        cx: &App,
    ) -> Option<kurbo::Point> {
        let bounds = self.list.bounds_for_item(page)?;
        let (width, _) = *self.document.read(cx).parsed.as_ref()?.sizes.get(page)?;
        Some(kurbo::Point::new(
            f32::from(
                position.x - bounds.origin.x - (bounds.size.width - px(width * self.scale)) / 2.0,
            ) as f64
                / self.scale as f64,
            f32::from(position.y - bounds.origin.y - px(PAGE_SPACING)) as f64 / self.scale as f64,
        ))
    }

    fn pointer_down(
        &mut self,
        page: usize,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus.focus(window, cx);
        let Some(position) = self.pointer_position(page, event.position, cx) else {
            return;
        };
        let Some(text) = self.text.get(&page) else {
            return;
        };
        if !event.modifiers.shift
            && let Some(link) = text
                .links
                .iter()
                .find(|link| link.bounds.contains(position))
        {
            self.link_down = Some((event.position, link.target.clone()));
            return;
        }
        self.link_down = None;
        let Some(offset) = text.hit(position) else {
            self.selection = None;
            cx.notify();
            return;
        };
        let position = TextPosition { page, offset };
        if event.click_count >= 2 {
            let mut start = offset.min(text.text.len());
            let mut end = start;
            for (index, character) in text.text[..start].char_indices().rev() {
                if !character.is_alphanumeric() {
                    break;
                }
                start = index;
            }
            for character in text.text[end..].chars() {
                if !character.is_alphanumeric() {
                    break;
                }
                end += character.len_utf8();
            }
            self.selection = Some((
                TextPosition {
                    page,
                    offset: start,
                },
                TextPosition { page, offset: end },
            ));
            self.dragging = false;
        } else {
            let anchor = if event.modifiers.shift {
                self.selection.map_or(position, |selection| selection.0)
            } else {
                position
            };
            self.selection = Some((anchor, position));
            self.dragging = true;
        }
        cx.notify();
    }

    fn pointer_move(
        &mut self,
        event: &gpui::MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hovered = self.text.iter().find_map(|(page, text)| {
            let position = self.pointer_position(*page, event.position, cx)?;
            text.links
                .iter()
                .find(|link| link.bounds.contains(position))
                .map(|link| (*page, link.target.clone()))
        });
        if hovered != self.hovered_link {
            self.hovered_link = hovered;
            cx.notify();
        }
        if event.pressed_button != Some(MouseButton::Left) {
            self.dragging = false;
            return;
        }
        if !self.dragging {
            return;
        }
        let viewport = self.list.viewport_bounds();
        if event.position.y < viewport.top() {
            self.list.scroll_by(px(-15.0));
        }
        if event.position.y > viewport.bottom() {
            self.list.scroll_by(px(15.0));
        }
        let page = (0..self.list.item_count())
            .filter_map(|page| {
                self.list.bounds_for_item(page).map(|bounds| {
                    (
                        page,
                        (f32::from(bounds.center().y - event.position.y)).abs(),
                    )
                })
            })
            .min_by(|left, right| left.1.total_cmp(&right.1))
            .map(|(page, _)| page);
        if let Some(page) = page
            && let Some(position) = self.pointer_position(page, event.position, cx)
            && let Some(text) = self.text.get(&page)
            && let Some(offset) = text.hit(position)
            && let Some((anchor, _)) = self.selection
        {
            self.selection = Some((anchor, TextPosition { page, offset }));
            cx.notify();
        }
    }

    fn pointer_up(
        &mut self,
        event: &gpui::MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dragging = false;
        if let Some((position, target)) = self.link_down.take()
            && (event.position - position).magnitude() < 4.0
        {
            match target {
                LinkTarget::Url(url) => cx.open_url(&url),
                LinkTarget::Page { page, y } => self.change_page(
                    page,
                    PAGE_SPACING + y as f32 * self.scale
                        - self
                            .viewport
                            .map_or(0.0, |size| f32::from(size.height) / 2.0),
                    window,
                    cx,
                ),
            }
        }
    }

    fn render_page(
        &mut self,
        page: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some((width, height)) = self
            .document
            .read(cx)
            .parsed
            .as_ref()
            .and_then(|parsed| parsed.sizes.get(page))
            .copied()
        else {
            return div().into_any_element();
        };
        self.raster_page(page, window, cx);
        let scale = self.scale;
        let mut highlights = Vec::new();
        if let Some(text) = self.text.get(&page) {
            for (index, found) in self
                .matches
                .iter()
                .enumerate()
                .filter(|(_, found)| found.page == page)
            {
                highlights.extend(text.highlights(&found.range).map(|bounds| {
                    (
                        bounds,
                        if self.active_match == Some(index) {
                            gpui::rgb(0xf59e0b).alpha(0.5)
                        } else {
                            gpui::rgb(0xfacc15).alpha(0.35)
                        },
                    )
                }));
            }
            if let Some(range) = self.selection_range(page) {
                highlights.extend(
                    text.highlights(&range)
                        .map(|bounds| (bounds, gpui::rgb(0x3b82f6).alpha(0.35))),
                );
            }
        }
        h_flex()
            .w_full()
            .justify_center()
            .px(px(PAGE_SPACING))
            .pt(px(PAGE_SPACING))
            .when(
                page + 1
                    == self
                        .document
                        .read(cx)
                        .parsed
                        .as_ref()
                        .map_or(0, |parsed| parsed.sizes.len()),
                |row| row.pb(px(PAGE_SPACING)),
            )
            .child(
                div()
                    .id(("pdf-page", page))
                    .relative()
                    .w(px(width * scale))
                    .h(px(height * scale))
                    .bg(gpui::white())
                    .border_1()
                    .border_color(gpui::black().alpha(0.18))
                    .shadow_sm()
                    .cursor(
                        if self
                            .hovered_link
                            .as_ref()
                            .is_some_and(|(hovered, _)| *hovered == page)
                        {
                            gpui::CursorStyle::PointingHand
                        } else {
                            gpui::CursorStyle::IBeam
                        },
                    )
                    .when_some(self.pages.get(&page), |element, image| {
                        element.child(img(image.image.clone()).size_full())
                    })
                    .child(
                        gpui::canvas(
                            |_, _, _| {},
                            move |bounds, _, window, _| {
                                for (rectangle, color) in &highlights {
                                    let rectangle = Bounds::new(
                                        bounds.origin
                                            + point(
                                                px(rectangle.x0 as f32 * scale),
                                                px(rectangle.y0 as f32 * scale),
                                            ),
                                        size(
                                            px(rectangle.width() as f32 * scale),
                                            px(rectangle.height() as f32 * scale),
                                        ),
                                    );
                                    window.paint_quad(gpui::fill(rectangle, *color));
                                }
                            },
                        )
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full(),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, event, window, cx| {
                            view.pointer_down(page, event, window, cx)
                        }),
                    ),
            )
            .into_any_element()
    }
}

impl Render for PdfView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.list.item_count();
        let parsed = self.document.read(cx).parsed.clone();
        let scale = parsed
            .as_ref()
            .and_then(|parsed| {
                let viewport = self.viewport?;
                match self.state.mode {
                    ZoomMode::Width => Some(
                        (f32::from(viewport.width) - PAGE_SPACING * 2.0)
                            / parsed
                                .sizes
                                .iter()
                                .map(|size| size.0)
                                .max_by(f32::total_cmp)?,
                    ),
                    ZoomMode::Page => {
                        let (width, height) = *parsed.sizes.get(self.state.page)?;
                        Some(
                            ((f32::from(viewport.width) - PAGE_SPACING * 2.0) / width)
                                .min((f32::from(viewport.height) - PAGE_SPACING * 2.0) / height),
                        )
                    }
                    ZoomMode::Custom => Some(self.state.zoom),
                }
            })
            .unwrap_or(self.state.zoom)
            .clamp(0.1, 8.0);
        if (scale - self.scale).abs() > 0.0001 {
            let offset = self.list.logical_scroll_top();
            self.list.remeasure_items(0..count);
            self.list.scroll_to(ListOffset {
                item_ix: offset.item_ix,
                offset_in_item: px(f32::from(offset.offset_in_item) * scale / self.scale),
            });
            self.scale = scale;
        }
        let width = parsed
            .as_ref()
            .and_then(|parsed| {
                parsed
                    .sizes
                    .iter()
                    .map(|size| size.0 * scale + PAGE_SPACING * 2.0)
                    .max_by(f32::total_cmp)
            })
            .unwrap_or(0.0)
            .max(self.viewport.map_or(0.0, |size| f32::from(size.width)));
        let error = self
            .document
            .read(cx)
            .error
            .clone()
            .map(SharedString::from)
            .or_else(|| self.raster_error.clone());
        let password_required = self.document.read(cx).password_required;
        let view = cx.weak_entity();
        let menu_view = cx.entity();
        let content = div()
            .id("pdf-pages")
            .size_full()
            .h(self.viewport.map_or(px(0.0), |viewport| viewport.height))
            .overflow_x_scroll()
            .track_scroll(&self.horizontal_scroll)
            .child(
                list(self.list.clone(), cx.processor(Self::render_page))
                    .h_full()
                    .w(px(width)),
            );
        v_flex()
            .id("PdfViewer")
            .key_context("PdfViewer")
            .track_focus(&self.focus)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(|view, _: &ZoomIn, _, cx| view.zoom(1.2, cx)))
            .on_action(cx.listener(|view, _: &ZoomOut, _, cx| view.zoom(1.0 / 1.2, cx)))
            .on_action(cx.listener(|view, _: &ActualSize, _, cx| {
                view.state.zoom = 1.0;
                view.state.mode = ZoomMode::Custom;
                cx.emit(StateChanged);
                cx.notify();
            }))
            .on_action(cx.listener(|view, _: &FitWidth, _, cx| {
                view.state.mode = ZoomMode::Width;
                cx.emit(StateChanged);
                cx.notify();
            }))
            .on_action(cx.listener(|view, _: &FitPage, _, cx| {
                view.state.mode = ZoomMode::Page;
                cx.emit(StateChanged);
                cx.notify();
            }))
            .on_action(cx.listener(|view, _: &NextPage, window, cx| {
                view.change_page(view.state.page.saturating_add(1), 0.0, window, cx)
            }))
            .on_action(cx.listener(|view, _: &PreviousPage, window, cx| {
                view.change_page(view.state.page.saturating_sub(1), 0.0, window, cx)
            }))
            .on_action(
                cx.listener(|view, _: &FirstPage, window, cx| view.change_page(0, 0.0, window, cx)),
            )
            .on_action(cx.listener(|view, _: &LastPage, window, cx| {
                view.change_page(view.list.item_count().saturating_sub(1), 0.0, window, cx)
            }))
            .on_action(cx.listener(|view, _: &ScrollDown, _, cx| {
                view.list.scroll_by(px(40.0));
                cx.notify();
            }))
            .on_action(cx.listener(|view, _: &ScrollUp, _, cx| {
                view.list.scroll_by(px(-40.0));
                cx.notify();
            }))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(|view, _: &ClearSelection, _, cx| {
                view.selection = None;
                cx.notify();
            }))
            .child(
                h_flex()
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        IconButton::new("pdf-previous-page", IconName::ChevronLeft)
                            .disabled(count == 0 || self.state.page == 0)
                            .tooltip(|_, cx| {
                                Tooltip::for_action("Previous page", &PreviousPage, cx)
                            })
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.change_page(view.state.page.saturating_sub(1), 0.0, window, cx)
                            })),
                    )
                    .child(
                        div()
                            .w(px(42.0))
                            .rounded_sm()
                            .border_1()
                            .border_color(cx.theme().colors().border)
                            .px_1()
                            .child(self.page_input.clone())
                            .on_key_down(cx.listener(
                                |view, event: &gpui::KeyDownEvent, window, cx| {
                                    if event.keystroke.key == "enter" {
                                        if let Ok(page) =
                                            view.page_input.read(cx).text(cx).parse::<usize>()
                                        {
                                            view.change_page(
                                                page.saturating_sub(1),
                                                0.0,
                                                window,
                                                cx,
                                            );
                                        }
                                        view.focus.focus(window, cx);
                                        cx.stop_propagation();
                                    }
                                },
                            )),
                    )
                    .child(Label::new(format!("/ {count}")).size(LabelSize::Small))
                    .child(
                        IconButton::new("pdf-next-page", IconName::ChevronRight)
                            .disabled(count == 0 || self.state.page + 1 >= count)
                            .tooltip(|_, cx| Tooltip::for_action("Next page", &NextPage, cx))
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.change_page(view.state.page.saturating_add(1), 0.0, window, cx)
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        IconButton::new("pdf-zoom-out", IconName::Dash)
                            .tooltip(|_, cx| Tooltip::for_action("Zoom out", &ZoomOut, cx))
                            .on_click(cx.listener(|view, _, _, cx| view.zoom(1.0 / 1.2, cx))),
                    )
                    .child(Label::new(format!("{:.0}%", scale * 100.0)).size(LabelSize::Small))
                    .child(
                        IconButton::new("pdf-zoom-in", IconName::Plus)
                            .tooltip(|_, cx| Tooltip::for_action("Zoom in", &ZoomIn, cx))
                            .on_click(cx.listener(|view, _, _, cx| view.zoom(1.2, cx))),
                    )
                    .child(
                        Button::new("pdf-fit-width", "Fit Width")
                            .style(if self.state.mode == ZoomMode::Width {
                                ButtonStyle::Tinted(TintColor::Accent)
                            } else {
                                ButtonStyle::Subtle
                            })
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.state.mode = ZoomMode::Width;
                                cx.emit(StateChanged);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("pdf-fit-page", "Fit Page")
                            .style(if self.state.mode == ZoomMode::Page {
                                ButtonStyle::Tinted(TintColor::Accent)
                            } else {
                                ButtonStyle::Subtle
                            })
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.state.mode = ZoomMode::Page;
                                cx.emit(StateChanged);
                                cx.notify();
                            })),
                    ),
            )
            .when_some(error, |view, error| {
                view.child(
                    div()
                        .px_2()
                        .py_1()
                        .child(Label::new(error).color(Color::Error).size(LabelSize::Small)),
                )
            })
            .when(password_required, |element| {
                element.child(
                    h_flex()
                        .gap_2()
                        .p_2()
                        .child(
                            div()
                                .w(px(220.0))
                                .border_1()
                                .border_color(cx.theme().colors().border)
                                .px_2()
                                .child(self.password_input.clone())
                                .on_key_down(cx.listener(
                                    |view, event: &gpui::KeyDownEvent, window, cx| {
                                        if event.keystroke.key == "enter" {
                                            view.unlock(window, cx);
                                            cx.stop_propagation();
                                        }
                                    },
                                )),
                        )
                        .child(Button::new("pdf-unlock", "Unlock").on_click(cx.listener(
                            |view, _, window, cx| {
                                view.unlock(window, cx);
                            },
                        ))),
                )
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .w_full()
                    .min_h_0()
                    .bg(cx
                        .theme()
                        .colors()
                        .editor_background
                        .blend(gpui::black().alpha(0.08)))
                    .on_mouse_move(cx.listener(Self::pointer_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::pointer_up))
                    .on_pinch(cx.listener(|view, event: &gpui::PinchEvent, _, cx| {
                        view.zoom(1.0 + event.delta, cx)
                    }))
                    .child(
                        gpui::canvas(
                            move |bounds, _, cx| {
                                view.update(cx, |view, cx| {
                                    if view.viewport != Some(bounds.size) {
                                        view.viewport = Some(bounds.size);
                                        cx.notify();
                                    }
                                })
                                .log_err();
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full(),
                    )
                    .child(
                        right_click_menu("pdf-context-menu")
                            .trigger(move |_, _, _| content)
                            .menu(move |window, cx| {
                                let focus = menu_view.read(cx).focus.clone();
                                ContextMenu::build(window, cx, |menu, _, _| {
                                    menu.context(focus)
                                        .action("Copy", Box::new(Copy))
                                        .action("Select All", Box::new(SelectAll))
                                        .separator()
                                        .action("Fit Width", Box::new(FitWidth))
                                        .action("Fit Page", Box::new(FitPage))
                                        .action("Actual Size", Box::new(ActualSize))
                                })
                            }),
                    ),
            )
    }
}

impl Focusable for PdfView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl Item for PdfView {
    type Event = StateChanged;
    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        self.document.read(cx).file.file_name(cx).to_string().into()
    }
    fn tab_icon(&self, _: &Window, cx: &App) -> Option<Icon> {
        file_icons::FileIcons::get_icon(&self.document.read(cx).path(cx), cx).map(Icon::from_path)
    }
    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(
            self.document
                .read(cx)
                .path(cx)
                .to_string_lossy()
                .to_string()
                .into(),
        )
    }
    fn to_item_events(_: &StateChanged, notify: &mut dyn FnMut(ItemEvent)) {
        notify(ItemEvent::UpdateTab);
    }
    fn for_each_project_item(
        &self,
        cx: &App,
        callback: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        callback(self.document.entity_id(), self.document.read(cx));
    }
    fn has_deleted_file(&self, cx: &App) -> bool {
        self.document.read(cx).file.disk_state.is_deleted()
    }
    fn reload(
        &mut self,
        _: Entity<Project>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.document.update(cx, |document, cx| document.reload(cx));
        Task::ready(Ok(()))
    }
    fn buffer_kind(&self, _: &App) -> ItemBufferKind {
        ItemBufferKind::Singleton
    }
    fn can_split(&self) -> bool {
        true
    }
    fn clone_on_split(
        &self,
        _: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>> {
        Task::ready(Some(cx.new(|cx| {
            Self::new(
                self.document.clone(),
                self.project.clone(),
                Some(self.state.clone()),
                window,
                cx,
            )
        })))
    }
    fn as_searchable(&self, view: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(view.clone()))
    }
}

impl SearchableItem for PdfView {
    type Match = PdfMatch;
    fn supported_options(&self) -> SearchOptions {
        SearchOptions {
            case: true,
            word: true,
            regex: true,
            ..Default::default()
        }
    }
    fn clear_matches(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.matches.clear();
        self.active_match = None;
        cx.notify();
    }
    fn get_matches(&self, _: &mut Window, _: &mut App) -> (Vec<PdfMatch>, SearchToken) {
        (self.matches.clone(), SearchToken::new(self.generation))
    }
    fn update_matches(
        &mut self,
        matches: &[PdfMatch],
        active: Option<usize>,
        token: SearchToken,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if token.value() != self.generation {
            return;
        }
        self.matches = matches.to_vec();
        self.active_match = active;
        cx.notify();
    }
    fn query_suggestion(
        &mut self,
        _: Option<SeedQuerySetting>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> String {
        self.selected_text()
    }
    fn activate_match(
        &mut self,
        index: usize,
        matches: &[PdfMatch],
        token: SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if token.value() != self.generation {
            return;
        }
        let Some(found) = matches.get(index) else {
            return;
        };
        self.active_match = Some(index);
        let Some(parsed) = self.document.read(cx).parsed.clone() else {
            return;
        };
        let found = found.clone();
        let text =
            cx.background_spawn(
                async move { parsed.text_page(found.page).map(|text| (found, text)) },
            );
        cx.spawn_in(window, async move |view, cx| {
            let (found, text) = text.await?;
            view.update_in(cx, |view, window, cx| {
                if view.generation != token.value() {
                    return;
                }
                let y = text
                    .highlights(&found.range)
                    .next()
                    .map_or(0.0, |rectangle| rectangle.center().y as f32);
                view.text.insert(found.page, text);
                let offset = PAGE_SPACING + y * view.scale
                    - view
                        .viewport
                        .map_or(0.0, |size| f32::from(size.height) / 2.0);
                view.change_page(found.page, offset, window, cx);
                cx.emit(SearchEvent::ActiveMatchChanged);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }
    fn select_matches(
        &mut self,
        _: &[PdfMatch],
        _: SearchToken,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }
    fn replace(
        &mut self,
        _: &PdfMatch,
        _: &SearchQuery,
        _: SearchToken,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }
    fn find_matches(
        &mut self,
        query: Arc<SearchQuery>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Vec<PdfMatch>> {
        let Some(parsed) = self.document.read(cx).parsed.clone() else {
            return Task::ready(Vec::new());
        };
        cx.background_spawn(async move {
            let mut matches = Vec::new();
            for page in 0..parsed.sizes.len() {
                match parsed.text_page(page) {
                    Ok(text) => matches.extend(
                        query
                            .search_str(&text.text)
                            .into_iter()
                            .map(|range| PdfMatch { page, range }),
                    ),
                    Err(error) => log::error!("Unable to search PDF page {}: {error}", page + 1),
                }
            }
            matches
        })
    }
    fn find_matches_with_token(
        &mut self,
        query: Arc<SearchQuery>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<(Vec<PdfMatch>, SearchToken)> {
        let token = SearchToken::new(self.generation);
        let matches = self.find_matches(query, window, cx);
        cx.spawn(async move |_, _| (matches.await, token))
    }
    fn active_match_index(
        &mut self,
        direction: Direction,
        matches: &[PdfMatch],
        token: SearchToken,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        if token.value() != self.generation || matches.is_empty() {
            return None;
        }
        self.active_match.or_else(|| match direction {
            Direction::Next => matches
                .iter()
                .position(|found| found.page >= self.state.page)
                .or(Some(0)),
            Direction::Prev => matches
                .iter()
                .rposition(|found| found.page <= self.state.page)
                .or(Some(matches.len() - 1)),
        })
    }
}

impl SerializableItem for PdfView {
    fn serialized_item_kind() -> &'static str {
        "PdfView"
    }
    fn cleanup(
        workspace_id: WorkspaceId,
        alive: Vec<ItemId>,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        workspace::delete_unloaded_items(
            alive,
            workspace_id,
            "pdf_views",
            &persistence::PdfViewDb::global(cx),
            cx,
        )
    }
    fn deserialize(
        project: Entity<Project>,
        _: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let database = persistence::PdfViewDb::global(cx);
        window.spawn(cx, async move |cx| {
            let state: ViewState = serde_json::from_str(
                &database
                    .get_view(item_id, workspace_id)?
                    .context("Missing PDF view state")?,
            )?;
            let (worktree, path) = project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(state.path.clone(), false, cx)
                })
                .await?;
            let path = ProjectPath {
                worktree_id: worktree.read_with(cx, |worktree, _| worktree.id()),
                path,
            };
            let document = cx
                .update(|_, cx| PdfDocument::open(project.clone(), path, cx))?
                .await?;
            cx.update(|window, cx| {
                Ok(cx.new(|cx| Self::new(document, project, Some(state), window, cx)))
            })?
        })
    }
    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let offset = self.list.logical_scroll_top();
        self.state.path = self.document.read(cx).path(cx);
        self.state.page = offset.item_ix;
        self.state.offset = f32::from(offset.offset_in_item);
        let state = match serde_json::to_string(&self.state) {
            Ok(state) => state,
            Err(error) => return Some(Task::ready(Err(error.into()))),
        };
        let database = persistence::PdfViewDb::global(cx);
        Some(cx.background_spawn(
            async move { database.save_view(item_id, workspace_id, state).await },
        ))
    }
    fn should_serialize(&self, _: &StateChanged) -> bool {
        true
    }
}

mod persistence {
    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use workspace::{ItemId, WorkspaceDb, WorkspaceId};
    pub struct PdfViewDb(ThreadSafeConnection);
    impl Domain for PdfViewDb {
        const NAME: &str = stringify!(PdfViewDb);
        const MIGRATIONS: &[&str] = &[
            sql!(CREATE TABLE pdf_views (workspace_id INTEGER NOT NULL, item_id INTEGER NOT NULL, state TEXT NOT NULL, PRIMARY KEY(workspace_id, item_id), FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id) ON DELETE CASCADE) STRICT;),
        ];
    }
    db::static_connection!(PdfViewDb, [WorkspaceDb]);
    impl PdfViewDb {
        query! { pub async fn save_view(item_id: ItemId, workspace_id: WorkspaceId, state: String) -> Result<()> { INSERT OR REPLACE INTO pdf_views(item_id, workspace_id, state) VALUES (?, ?, ?) } }
        query! { pub fn get_view(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<String>> { SELECT state FROM pdf_views WHERE item_id = ? AND workspace_id = ? } }
    }
}
