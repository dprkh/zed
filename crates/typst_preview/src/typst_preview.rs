use anyhow::{Context as _, Result};
use collections::HashMap;
use editor::{Editor, EditorEvent};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ListAlignment, ListOffset, ListState, MouseButton, Render, RenderImage, ScrollHandle,
    Subscription, Task, WeakEntity, Window, div, img, list, px,
};
use multi_buffer::MultiBufferOffset;
use project::{Project, typst_store::TypstStore};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};
use typst_engine::{Compilation, Navigation};
use ui::{TintColor, prelude::*};
use util::ResultExt as _;
use workspace::{
    ItemId, Pane, Workspace, WorkspaceId,
    item::{Item, ItemEvent, SerializableItem},
};
pub use zed_actions::preview::typst::*;

const PAGE_GAP: f32 = 24.0;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ViewState {
    path: PathBuf,
    following: bool,
    pinned_entry: Option<PathBuf>,
    zoom: f32,
    fit_width: bool,
    cursor_follow: bool,
    page: usize,
}

pub struct TypstPreview {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    store: Entity<TypstStore>,
    editor: Entity<Editor>,
    entry: PathBuf,
    state: ViewState,
    compilation: Option<Arc<Compilation>>,
    images: HashMap<usize, CachedPage>,
    raster_tasks: HashMap<usize, RasterTask>,
    list: ListState,
    error: Option<SharedString>,
    image_scale: f32,
    viewport_width: Option<f32>,
    horizontal_scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
    _editor_subscription: Option<Subscription>,
    _refresh: Task<()>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct RasterKey {
    fingerprint: u128,
    scale_bits: u32,
}

struct CachedPage {
    key: RasterKey,
    image: Arc<RenderImage>,
}

struct RasterTask {
    key: RasterKey,
    _task: Task<()>,
}

#[derive(Clone)]
pub struct StateChanged;

impl EventEmitter<StateChanged> for TypstPreview {}

pub fn init(cx: &mut App) {
    workspace::register_serializable_item::<TypstPreview>(cx);
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &OpenPreview, window, cx| {
            TypstPreview::open(workspace, false, false, window, cx);
        });
        workspace.register_action(|workspace, _: &OpenPreviewToTheSide, window, cx| {
            TypstPreview::open(workspace, true, false, window, cx);
        });
        workspace.register_action(|workspace, _: &OpenFollowingPreview, window, cx| {
            TypstPreview::open(workspace, true, true, window, cx);
        });
    })
    .detach();
}

impl TypstPreview {
    pub fn is_typst_file(editor: &Entity<Editor>, cx: &App) -> bool {
        let editor = editor.read(cx);
        let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
            return false;
        };
        editor
            .project()
            .is_some_and(|project| project.read(cx).is_native_typst(&buffer, cx))
    }

    fn open(
        workspace: &mut Workspace,
        split: bool,
        following: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(editor) = workspace.active_item_as::<Editor>(cx) else {
            return;
        };
        if !Self::is_typst_file(&editor, cx) {
            return;
        }
        let pane = workspace.active_pane().clone();
        if following {
            let pane = workspace.adjacent_pane_of(&pane, window, cx);
            Self::activate_or_add_preview(workspace, editor, pane, true, window, cx);
        } else if split {
            Self::open_preview_to_the_side_of_pane(workspace, editor, pane, window, cx);
        } else {
            Self::open_preview_in_pane(workspace, editor, pane, window, cx);
        }
    }

    pub fn open_preview_in_pane(
        workspace: &mut Workspace,
        editor: Entity<Editor>,
        pane: Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        Self::activate_or_add_preview(workspace, editor, pane, false, window, cx);
    }

    pub fn open_preview_to_the_side_of_pane(
        workspace: &mut Workspace,
        editor: Entity<Editor>,
        origin_pane: Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if !Self::is_typst_file(&editor, cx) {
            return;
        }
        let pane = workspace.adjacent_pane_of(&origin_pane, window, cx);
        Self::activate_or_add_preview(workspace, editor.clone(), pane, false, window, cx);
        editor.focus_handle(cx).focus(window, cx);
    }

    fn activate_or_add_preview(
        workspace: &mut Workspace,
        editor: Entity<Editor>,
        pane: Entity<Pane>,
        following: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if !Self::is_typst_file(&editor, cx) {
            return;
        }
        let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() else {
            return;
        };
        let project = workspace.project().clone();
        let existing = pane.read(cx).items_of_type::<Self>().find(|view| {
            view.read(cx)
                .editor
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                == Some(buffer.clone())
                && view.read(cx).state.following == following
        });
        if let Some(existing) = existing {
            pane.update(cx, |pane, cx| {
                if let Some(index) = pane.index_for_item(&existing) {
                    pane.activate_item(index, true, true, window, cx);
                }
            });
            return;
        }
        let workspace_handle = workspace.weak_handle();
        match Self::new(
            editor,
            project,
            workspace_handle,
            following,
            None,
            window,
            cx,
        ) {
            Ok(view) => {
                pane.update(cx, |pane, cx| {
                    pane.add_item(Box::new(view), true, true, None, window, cx)
                });
            }
            Err(error) => log::error!("Opening native Typst preview: {error:#}"),
        }
    }

    fn new(
        editor: Entity<Editor>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        following: bool,
        restored: Option<ViewState>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<Entity<Self>> {
        let buffer = editor
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()
            .context("Typst preview requires a single source file")?;
        let request = project.update(cx, |project, cx| project.typst_request(&buffer, cx))?;
        let store = project.update(cx, |project, cx| project.typst_store(cx));
        let state = restored.unwrap_or(ViewState {
            path: request.path.clone(),
            following,
            pinned_entry: None,
            zoom: 1.0,
            fit_width: true,
            cursor_follow: true,
            page: 0,
        });
        let entry = state.pinned_entry.clone().unwrap_or(request.input.entry);
        let view = cx.new(|cx: &mut Context<Self>| {
            let mut view = Self {
                focus_handle: cx.focus_handle(),
                workspace: workspace.clone(),
                project: project.clone(),
                store: store.clone(),
                editor,
                entry,
                state,
                compilation: None,
                images: HashMap::default(),
                raster_tasks: HashMap::default(),
                list: ListState::new(0, ListAlignment::Top, px(500.0)),
                error: None,
                image_scale: 1.0,
                viewport_width: None,
                horizontal_scroll: ScrollHandle::new(),
                _subscriptions: Vec::new(),
                _editor_subscription: None,
                _refresh: Task::ready(()),
            };
            view._subscriptions.push(cx.subscribe_in(
                &store,
                window,
                |view, _, updated: &project::typst_store::Updated, window, cx| {
                    if updated.entry == view.entry {
                        view.update_pages(window, cx);
                    }
                },
            ));
            if let Some(workspace) = workspace.upgrade() {
                view._subscriptions.push(cx.subscribe_in(
                    &workspace,
                    window,
                    |view, workspace, event: &workspace::Event, window, cx| {
                        if matches!(event, workspace::Event::ActiveItemChanged)
                            && let Some(editor) = workspace
                                .read(cx)
                                .active_item(cx)
                                .and_then(|item| item.downcast::<Editor>())
                            && let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton()
                            && view.project.read(cx).is_native_typst(&buffer, cx)
                            && (view.state.following
                                || view.editor.read(cx).buffer().read(cx).as_singleton()
                                    == Some(buffer))
                            && view.editor != editor
                        {
                            view.editor = editor;
                            view.bind_editor(window, cx);
                            view.request_compile(window, cx);
                        }
                    },
                ));
            }
            view._subscriptions.push(cx.subscribe_in(
                &project,
                window,
                |view, _, event: &project::Event, window, cx| {
                    if matches!(event, project::Event::BufferEdited { .. }) {
                        view.defer_compile(window, cx);
                    }
                },
            ));
            let preview = cx.weak_entity();
            view.list.set_scroll_handler(move |event, _, cx| {
                preview
                    .update(cx, |view, cx| {
                        if view.state.page != event.visible_range.start {
                            view.state.page = event.visible_range.start;
                            cx.emit(StateChanged);
                            cx.notify();
                        }
                    })
                    .log_err();
            });
            view._subscriptions
                .push(cx.on_release_in(window, |view, window, _| view.clear_images(window)));
            view.bind_editor(window, cx);
            view.request_compile(window, cx);
            view
        });
        Ok(view)
    }

    fn bind_editor(&mut self, window: &Window, cx: &mut Context<Self>) {
        self._editor_subscription = Some(cx.subscribe_in(
            &self.editor,
            window,
            |view, editor, event: &EditorEvent, _, cx| {
                if matches!(
                    event,
                    EditorEvent::SelectionsChanged { local: true } | EditorEvent::Edited { .. }
                ) && view.state.cursor_follow
                    && let Some(compilation) = &view.compilation
                    && let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton()
                {
                    let position = editor.read(cx).selections.newest_anchor().head();
                    let multibuffer = editor.read(cx).buffer().read(cx);
                    if let Some((source, anchor)) =
                        multibuffer.text_anchor_for_position(position, cx)
                        && source == buffer
                        && let Some(offset) = view.store.read(cx).source_offset(
                            &view.entry,
                            &view.state.path,
                            &anchor,
                        )
                        && let Some((page, _, y)) =
                            compilation.jump_from_source(&view.state.path, offset)
                    {
                        if view.state.page != page {
                            view.state.page = page;
                            cx.emit(StateChanged);
                        }
                        view.list.scroll_to(ListOffset {
                            item_ix: page,
                            offset_in_item: px((y as f32 * view.image_scale - 48.0).max(0.0)),
                        });
                        cx.notify();
                    }
                }
            },
        ));
    }

    fn defer_compile(&mut self, window: &Window, cx: &mut Context<Self>) {
        self._refresh = cx.spawn_in(window, async move |view, cx| {
            view.update_in(cx, |view, window, cx| view.request_compile(window, cx))
                .log_err();
        });
    }

    fn request_compile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(buffer) = self.editor.read(cx).buffer().read(cx).as_singleton() else {
            return;
        };
        match self
            .project
            .update(cx, |project, cx| project.typst_request(&buffer, cx))
        {
            Ok(mut request) => {
                self.state.path = request.path.clone();
                if let Some(entry) = &self.state.pinned_entry {
                    request.input.entry = entry.clone();
                }
                if self.entry != request.input.entry {
                    self.compilation = None;
                    self.clear_images(window);
                    self.list.reset(0);
                    self.state.page = 0;
                }
                self.entry = request.input.entry.clone();
                self.store.update(cx, |store, cx| {
                    if self.focus_handle.is_focused(window)
                        || self.editor.focus_handle(cx).is_focused(window)
                    {
                        store.focus(self.entry.clone());
                    }
                    store.refresh(request, cx);
                });
                self.update_pages(window, cx);
                cx.emit(StateChanged);
            }
            Err(error) => {
                self.error = Some(error.to_string().into());
                cx.notify();
            }
        }
    }

    fn update_pages(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.error = self
            .store
            .read(cx)
            .compilation(&self.entry)
            .and_then(|compilation| {
                compilation
                    .diagnostics
                    .iter()
                    .find(|diagnostic| !diagnostic.warning)
                    .map(|diagnostic| diagnostic.message.clone().into())
            });
        if let Some(error) = self.store.read(cx).error(&self.entry) {
            self.error = Some(error.to_owned().into());
        }
        if let Some(compilation) = self.store.read(cx).pages(&self.entry)
            && self
                .compilation
                .as_ref()
                .is_none_or(|current| !Arc::ptr_eq(current, &compilation))
        {
            let count = compilation.page_sizes().len();
            let old_count = self.list.item_count();
            if count > old_count {
                self.list.splice(old_count..old_count, count - old_count);
            } else if count < old_count {
                self.list.splice(count..old_count, 0);
            }
            if let Some(previous) = &self.compilation {
                for (page, (old, new)) in previous
                    .page_sizes()
                    .iter()
                    .zip(compilation.page_sizes())
                    .enumerate()
                {
                    if old != new {
                        self.list.remeasure_items(page..page + 1);
                    }
                }
            } else {
                self.list.scroll_to(ListOffset {
                    item_ix: self.state.page.min(count.saturating_sub(1)),
                    offset_in_item: px(0.0),
                });
            }
            let removed = self
                .images
                .keys()
                .filter(|page| **page >= count)
                .copied()
                .collect::<Vec<_>>();
            for page in removed {
                if let Some(image) = self.images.remove(&page) {
                    window.drop_image(image.image).log_err();
                }
            }
            self.raster_tasks.retain(|page, _| *page < count);
            self.state.page = self.state.page.min(count.saturating_sub(1));
            let scroll = self.list.logical_scroll_top();
            if scroll.item_ix >= count && count > 0 {
                self.list.scroll_to(ListOffset {
                    item_ix: count - 1,
                    offset_in_item: px(0.0),
                });
            }
            self.compilation = Some(compilation);
        }
        cx.notify();
    }

    fn clear_images(&mut self, window: &mut Window) {
        self.raster_tasks.clear();
        for (_, image) in self.images.drain() {
            window.drop_image(image.image).log_err();
        }
    }

    fn raster_key(&self, page: usize, window: &Window) -> Option<RasterKey> {
        Some(RasterKey {
            fingerprint: self.compilation.as_ref()?.page_fingerprint(page)?,
            scale_bits: (self.image_scale * window.scale_factor()).to_bits(),
        })
    }

    fn raster_page(&mut self, page: usize, window: &Window, cx: &mut Context<Self>) {
        let Some(key) = self.raster_key(page, window) else {
            return;
        };
        if self.images.get(&page).is_some_and(|image| image.key == key)
            || self.raster_tasks.contains_key(&page)
        {
            return;
        }
        let Some(compilation) = self.compilation.clone() else {
            return;
        };
        let background = cx.background_spawn(async move {
            let raster = compilation.rasterize(page, f32::from_bits(key.scale_bits))?;
            let mut pixels = image::RgbaImage::from_raw(raster.width, raster.height, raster.pixels)
                .context("Invalid Typst raster image")?;
            for pixel in pixels.chunks_exact_mut(4) {
                gpui::swap_rgba_pa_to_bgra(pixel);
            }
            Ok::<_, anyhow::Error>(Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])))
        });
        let task = cx.spawn_in(window, async move |view, cx| {
            let result = background.await;
            view.update_in(cx, |view, window, cx| {
                if !view
                    .raster_tasks
                    .get(&page)
                    .is_some_and(|task| task.key == key)
                {
                    return;
                }
                view.raster_tasks.remove(&page);
                if view.raster_key(page, window) != Some(key) {
                    cx.notify();
                    return;
                }
                match result {
                    Ok(image) => {
                        if !view.images.contains_key(&page)
                            && view.images.len() >= 8
                            // A fixed page count evicts visible pages at low zoom,
                            // causing an endless cycle of rendering and eviction.
                            && view
                                .images
                                .values()
                                .filter_map(|cached| cached.image.as_bytes(0))
                                .map(<[u8]>::len)
                                .sum::<usize>()
                                >= 64 * 1024 * 1024
                            && let Some(oldest) = view
                                .images
                                .keys()
                                .max_by_key(|index| index.abs_diff(page))
                                .copied()
                            && let Some(image) = view.images.remove(&oldest)
                        {
                            window.drop_image(image.image).log_err();
                        }
                        if let Some(previous) = view.images.insert(page, CachedPage { key, image })
                        {
                            window.drop_image(previous.image).log_err();
                        }
                    }
                    Err(error) => view.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .log_err();
        });
        self.raster_tasks
            .insert(page, RasterTask { key, _task: task });
    }

    fn render_page(
        &mut self,
        page: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some((width, height)) = self
            .compilation
            .as_ref()
            .and_then(|compilation| compilation.page_sizes().get(page).copied())
        else {
            return div().into_any_element();
        };
        self.raster_page(page, window, cx);
        let width = width as f32 * self.image_scale;
        let height = height as f32 * self.image_scale;
        let scale = self.image_scale;
        let list = self.list.clone();
        h_flex()
            .w_full()
            .justify_center()
            .py(px(PAGE_GAP / 2.0))
            .child(
                div()
                    .id(("typst-page", page))
                    .relative()
                    .w(px(width))
                    .h(px(height))
                    .bg(gpui::white())
                    .border_1()
                    .border_color(gpui::black().alpha(0.18))
                    .shadow_sm()
                    .when_some(self.images.get(&page), |page, image| {
                        page.child(img(image.image.clone()).size_full())
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, event: &gpui::MouseDownEvent, window, cx| {
                            let Some(bounds) = list.bounds_for_item(page) else {
                                return;
                            };
                            let x = f32::from(
                                event.position.x
                                    - bounds.origin.x
                                    - (bounds.size.width - px(width)) / 2.0,
                            ) / scale;
                            let y =
                                f32::from(event.position.y - bounds.origin.y - px(PAGE_GAP / 2.0))
                                    / scale;
                            if !view.images.get(&page).is_some_and(|image| {
                                Some(image.key) == view.raster_key(page, window)
                            }) {
                                return;
                            }
                            if let Some(navigation) =
                                view.compilation.as_ref().and_then(|compilation| {
                                    compilation.jump_from_page(page, x as f64, y as f64)
                                })
                            {
                                view.navigate(navigation, window, cx);
                            }
                        }),
                    ),
            )
            .into_any_element()
    }

    fn navigate(&mut self, navigation: Navigation, window: &mut Window, cx: &mut Context<Self>) {
        match navigation {
            Navigation::Page { page, y, .. } => {
                self.state.page = page;
                self.list.scroll_to(ListOffset {
                    item_ix: page,
                    offset_in_item: px(y as f32 * self.image_scale),
                });
                cx.emit(StateChanged);
                cx.notify();
            }
            Navigation::Url(url) => cx.open_url(&url),
            Navigation::Source(location) => {
                let project = self.project.clone();
                let workspace = self.workspace.clone();
                let source = self.editor.clone();
                let store = self.store.clone();
                let entry = self.entry.clone();
                cx.spawn_in(window, async move |_, cx| {
                    let buffer = project
                        .update(cx, |project, cx| {
                            project.open_local_buffer(&location.path, cx)
                        })
                        .await?;
                    workspace.update_in(cx, |workspace, window, cx| {
                        let range = store
                            .read(cx)
                            .source_range(&entry, &location, buffer.read(cx));
                        let editor = if source.read(cx).buffer().read(cx).as_singleton()
                            == Some(buffer.clone())
                        {
                            source
                        } else {
                            cx.new(|cx| {
                                Editor::for_buffer(buffer, Some(project.clone()), window, cx)
                            })
                        };
                        let pane = workspace
                            .pane_for(&editor)
                            .unwrap_or_else(|| workspace.active_pane().clone());
                        pane.update(cx, |pane, cx| {
                            if let Some(index) = pane.index_for_item(&editor) {
                                pane.activate_item(index, true, true, window, cx);
                            } else {
                                pane.add_item(
                                    Box::new(editor.clone()),
                                    true,
                                    true,
                                    None,
                                    window,
                                    cx,
                                );
                            }
                        });
                        editor.update(cx, |editor, cx| {
                            editor.change_selections(Default::default(), window, cx, |selections| {
                                selections
                                    .select_ranges([MultiBufferOffset(range.start)
                                        ..MultiBufferOffset(range.end)])
                            })
                        });
                    })?;
                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
            }
        }
    }

    fn zoom(&mut self, factor: f32, _window: &mut Window, cx: &mut Context<Self>) {
        self.state.zoom = (self.image_scale * factor).clamp(0.1, 8.0);
        self.state.fit_width = false;
        cx.emit(StateChanged);
        cx.notify();
    }

    fn change_page(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self
            .compilation
            .as_ref()
            .map_or(0, |compilation| compilation.page_sizes().len());
        self.state.page = self
            .state
            .page
            .saturating_add_signed(delta)
            .min(count.saturating_sub(1));
        self.list.scroll_to(ListOffset {
            item_ix: self.state.page,
            offset_in_item: px(0.0),
        });
        cx.emit(StateChanged);
        cx.notify();
    }
}

impl Render for TypstPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page_count = self
            .compilation
            .as_ref()
            .map_or(0, |compilation| compilation.page_sizes().len());
        let width = self.viewport_width;
        let scale = if self.state.fit_width {
            let page_width = self.compilation.as_ref().and_then(|compilation| {
                compilation
                    .page_sizes()
                    .iter()
                    .map(|size| size.0 as f32)
                    .max_by(f32::total_cmp)
            });
            width
                .zip(page_width)
                .map(|(width, page_width)| ((width - 32.0) / page_width).clamp(0.1, 8.0))
                .unwrap_or(self.state.zoom)
        } else {
            self.state.zoom
        };
        if (self.image_scale - scale).abs() > 0.01 {
            self.image_scale = scale;
            self.list.remeasure_items(0..page_count);
        }
        let content_width = self
            .compilation
            .as_ref()
            .and_then(|compilation| {
                compilation
                    .page_sizes()
                    .iter()
                    .map(|size| size.0 as f32 * self.image_scale + 32.0)
                    .max_by(f32::total_cmp)
            })
            .unwrap_or(0.0)
            .max(width.unwrap_or(0.0));
        let preview = cx.weak_entity();
        v_flex()
            .id("TypstPreview")
            .key_context("TypstPreview")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(|view, _: &ZoomIn, window, cx| view.zoom(1.2, window, cx)))
            .on_action(
                cx.listener(|view, _: &ZoomOut, window, cx| view.zoom(1.0 / 1.2, window, cx)),
            )
            .on_action(cx.listener(|view, _: &NextPage, _, cx| view.change_page(1, cx)))
            .on_action(cx.listener(|view, _: &PreviousPage, _, cx| view.change_page(-1, cx)))
            .child(
                h_flex()
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        IconButton::new("previous-page", IconName::ChevronLeft)
                            .on_click(cx.listener(|view, _, _, cx| view.change_page(-1, cx))),
                    )
                    .child(
                        Label::new(format!(
                            "{} / {}",
                            if page_count == 0 {
                                0
                            } else {
                                self.state.page + 1
                            },
                            page_count
                        ))
                        .size(LabelSize::Small),
                    )
                    .child(
                        IconButton::new("next-page", IconName::ChevronRight)
                            .on_click(cx.listener(|view, _, _, cx| view.change_page(1, cx))),
                    )
                    .child(div().flex_1())
                    .child(IconButton::new("zoom-out", IconName::Dash).on_click(
                        cx.listener(|view, _, window, cx| view.zoom(1.0 / 1.2, window, cx)),
                    ))
                    .child(
                        Label::new(format!("{:.0}%", self.image_scale * 100.0))
                            .size(LabelSize::Small),
                    )
                    .child(
                        IconButton::new("zoom-in", IconName::Plus).on_click(
                            cx.listener(|view, _, window, cx| view.zoom(1.2, window, cx)),
                        ),
                    )
                    .child(
                        Button::new("fit-width", "Fit")
                            .style(if self.state.fit_width {
                                ButtonStyle::Tinted(TintColor::Accent)
                            } else {
                                ButtonStyle::Subtle
                            })
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.state.fit_width = true;
                                cx.emit(StateChanged);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("cursor-follow", "Follow cursor")
                            .style(if self.state.cursor_follow {
                                ButtonStyle::Tinted(TintColor::Accent)
                            } else {
                                ButtonStyle::Subtle
                            })
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.state.cursor_follow = !view.state.cursor_follow;
                                cx.emit(StateChanged);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("pin-entry", "Pin main")
                            .style(if self.state.pinned_entry.is_some() {
                                ButtonStyle::Tinted(TintColor::Accent)
                            } else {
                                ButtonStyle::Subtle
                            })
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.state.pinned_entry = if view.state.pinned_entry.is_some() {
                                    None
                                } else {
                                    Some(view.entry.clone())
                                };
                                view.request_compile(window, cx);
                            })),
                    ),
            )
            .when_some(self.error.clone(), |view, error| {
                view.child(
                    div()
                        .px_2()
                        .py_1()
                        .child(Label::new(error).color(Color::Error).size(LabelSize::Small)),
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
                    .child(
                        gpui::canvas(
                            move |bounds, _, cx| {
                                preview
                                    .update(cx, |view, cx| {
                                        let width = f32::from(bounds.size.width);
                                        if view
                                            .viewport_width
                                            .is_none_or(|previous| (previous - width).abs() > 0.1)
                                        {
                                            view.viewport_width = Some(width);
                                            cx.notify();
                                        }
                                    })
                                    .log_err();
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(
                        div()
                            .id("typst-pages")
                            .size_full()
                            .overflow_x_scroll()
                            .track_scroll(&self.horizontal_scroll)
                            .child(
                                list(self.list.clone(), cx.processor(Self::render_page))
                                    .h_full()
                                    .w(px(content_width)),
                            ),
                    ),
            )
    }
}

impl Focusable for TypstPreview {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for TypstPreview {
    type Event = StateChanged;
    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::File))
    }
    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        format!(
            "Preview {}",
            self.entry.file_name().unwrap_or_default().to_string_lossy()
        )
        .into()
    }
    fn to_item_events(_: &StateChanged, _: &mut dyn FnMut(ItemEvent)) {}
}

impl SerializableItem for TypstPreview {
    fn serialized_item_kind() -> &'static str {
        "TypstPreview"
    }
    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        workspace::delete_unloaded_items(
            alive_items,
            workspace_id,
            "typst_previews",
            &persistence::TypstPreviewDb::global(cx),
            cx,
        )
    }
    fn deserialize(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let database = persistence::TypstPreviewDb::global(cx);
        window.spawn(cx, async move |cx| {
            let state: ViewState = serde_json::from_str(
                &database
                    .get_preview(item_id, workspace_id)?
                    .context("Missing Typst preview state")?,
            )?;
            let buffer = project
                .update(cx, |project, cx| project.open_local_buffer(&state.path, cx))
                .await?;
            cx.update(|window, cx| {
                let editor =
                    cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
                Self::new(
                    editor,
                    project,
                    workspace,
                    state.following,
                    Some(state),
                    window,
                    cx,
                )
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
        let state = match serde_json::to_string(&self.state) {
            Ok(state) => state,
            Err(error) => return Some(Task::ready(Err(error.into()))),
        };
        let database = persistence::TypstPreviewDb::global(cx);
        Some(cx.background_spawn(async move {
            database.save_preview(item_id, workspace_id, state).await
        }))
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
    pub struct TypstPreviewDb(ThreadSafeConnection);
    impl Domain for TypstPreviewDb {
        const NAME: &str = stringify!(TypstPreviewDb);
        const MIGRATIONS: &[&str] = &[sql!(
            CREATE TABLE typst_previews (
                workspace_id INTEGER NOT NULL,
                item_id INTEGER NOT NULL,
                state TEXT NOT NULL,
                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id) ON DELETE CASCADE
            ) STRICT;
        )];
    }
    db::static_connection!(TypstPreviewDb, [WorkspaceDb]);
    impl TypstPreviewDb {
        query! {
            pub async fn save_preview(item_id: ItemId, workspace_id: WorkspaceId, state: String) -> Result<()> {
                INSERT OR REPLACE INTO typst_previews(item_id, workspace_id, state) VALUES (?, ?, ?)
            }
        }
        query! {
            pub fn get_preview(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<String>> {
                SELECT state FROM typst_previews WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}
