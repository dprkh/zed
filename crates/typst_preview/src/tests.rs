use super::*;
use gpui::TestAppContext;
use multi_buffer::ToOffset as _;
use settings::Settings as _;
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use util::path;

struct PreviewTestRoot(Entity<TypstPreview>);

impl Render for PreviewTestRoot {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().w(px(600.0)).h(px(500.0)).child(self.0.clone())
    }
}

#[gpui::test]
async fn shared_preview_retains_pages_on_errors_and_restores_state(cx: &mut TestAppContext) {
    cx.update(|cx| {
        cx.set_global(db::AppDatabase::test_new());
        workspace::AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
        project::typst_store::TypstSettings::override_global(
            project::typst_store::TypstSettings(typst_engine::Configuration {
                system_fonts: false,
                package_downloads: false,
                ..Default::default()
            }),
            cx,
        );
    });
    let filesystem = fs::FakeFs::new(cx.executor());
    filesystem
        .insert_tree(
            path!("/typst"),
            serde_json::json!({
                "main.typ": "First page\n#pagebreak()\nSecond page",
            }),
        )
        .await;
    let project = Project::test(filesystem, [path!("/typst").as_ref()], cx).await;
    project
        .read_with(cx, |project, _| project.languages().clone())
        .add(Arc::new(language::Language::new(
            language::LanguageConfig {
                name: "Typst".into(),
                matcher: language::LanguageMatcher {
                    path_suffixes: vec!["typ".into()],
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            },
            None,
        )));
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/typst/main.typ"), cx)
        })
        .await
        .unwrap();
    let (workspace, cx) =
        cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
    let (preview, editor) = workspace.update_in(cx, |workspace, window, cx| {
        let editor =
            cx.new(|cx| Editor::for_buffer(buffer.clone(), Some(project.clone()), window, cx));
        workspace.active_pane().update(cx, |pane, cx| {
            pane.add_item(Box::new(editor.clone()), true, true, None, window, cx)
        });
        let source_pane = workspace.active_pane().clone();
        assert!(TypstPreview::is_typst_file(&editor, cx));
        TypstPreview::open_preview_to_the_side_of_pane(
            workspace,
            editor.clone(),
            source_pane.clone(),
            window,
            cx,
        );
        let preview_pane = workspace
            .panes()
            .iter()
            .find(|pane| {
                pane.read(cx)
                    .items_of_type::<TypstPreview>()
                    .next()
                    .is_some()
            })
            .unwrap()
            .clone();
        let preview = preview_pane
            .read(cx)
            .items_of_type::<TypstPreview>()
            .next()
            .unwrap();
        assert_eq!(preview.read(cx).editor, editor);
        TypstPreview::open_preview_in_pane(
            workspace,
            editor.clone(),
            preview_pane.clone(),
            window,
            cx,
        );
        assert_eq!(workspace.panes().len(), 2);
        assert_eq!(
            preview_pane
                .read(cx)
                .items_of_type::<TypstPreview>()
                .count(),
            1
        );
        TypstPreview::open_preview_in_pane(
            workspace,
            editor.clone(),
            source_pane.clone(),
            window,
            cx,
        );
        assert!(
            source_pane
                .read(cx)
                .active_item()
                .unwrap()
                .downcast::<TypstPreview>()
                .is_some()
        );
        assert_eq!(
            source_pane.read(cx).items_of_type::<TypstPreview>().count(),
            1
        );
        TypstPreview::open_preview_in_pane(
            workspace,
            editor.clone(),
            source_pane.clone(),
            window,
            cx,
        );
        assert_eq!(
            source_pane.read(cx).items_of_type::<TypstPreview>().count(),
            1
        );
        (preview, editor)
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();
    let successful = preview.read_with(cx, |preview, _| preview.compilation.clone().unwrap());
    assert_eq!(successful.page_sizes().len(), 2);
    let state_changes = Arc::new(AtomicUsize::new(0));
    let subscription = cx.update(|_, cx| {
        let state_changes = state_changes.clone();
        cx.subscribe(&preview, move |_, _: &StateChanged, _| {
            state_changes.fetch_add(1, Ordering::SeqCst);
        })
    });
    editor.update_in(cx, |editor, window, cx| {
        let offset = buffer.read(cx).len() - 2;
        editor.change_selections(Default::default(), window, cx, |selections| {
            selections.select_ranges([MultiBufferOffset(offset)..MultiBufferOffset(offset)])
        });
    });
    cx.run_until_parked();
    assert_eq!(preview.read_with(cx, |preview, _| preview.state.page), 1);
    assert!(state_changes.load(Ordering::SeqCst) > 0);
    drop(subscription);
    preview.update_in(cx, |preview, window, cx| {
        preview.state.cursor_follow = false;
        preview.zoom(1.2, window, cx);
        preview.change_page(1, cx);
        preview.state.pinned_entry = Some(preview.entry.clone());
        assert!(!preview.state.fit_width);
        assert_eq!(preview.state.page, 1);
    });
    cx.update(|window, cx| window.replace_root(cx, |_, _| PreviewTestRoot(preview.clone())));
    for _ in 0..4 {
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
    }
    preview.read_with(cx, |preview, _| {
        assert_eq!(preview.list.logical_scroll_top().item_ix, 1);
        let bounds = preview.list.bounds_for_item(1).unwrap();
        let viewport = preview.list.viewport_bounds();
        assert!(viewport.size.height > px(0.0));
        assert!(bounds.origin.y <= viewport.origin.y + px(24.0));
    });
    preview.update(cx, |preview, _| {
        preview.list.scroll_to(ListOffset {
            item_ix: 1,
            offset_in_item: px(80.0),
        });
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
    let (images, scroll) = preview.read_with(cx, |preview, _| {
        assert_eq!(preview.images.len(), 2);
        (
            preview
                .images
                .iter()
                .map(|(page, cached)| (*page, cached.image.id))
                .collect::<HashMap<_, _>>(),
            preview.list.logical_scroll_top(),
        )
    });
    let replacements = Arc::new(AtomicUsize::new(0));
    let subscription = cx.update(|_, cx| {
        let store = preview.read(cx).store.clone();
        let preview = preview.clone();
        let images = images.clone();
        let replacements = replacements.clone();
        cx.subscribe(&store, move |_, _: &project::typst_store::Updated, cx| {
            let preview = preview.read(cx);
            for (page, image_id) in &images {
                assert_eq!(preview.images[page].image.id, *image_id);
            }
            let current = preview.list.logical_scroll_top();
            assert_eq!(current.item_ix, scroll.item_ix);
            assert_eq!(current.offset_in_item, scroll.offset_in_item);
            replacements.fetch_add(1, Ordering::SeqCst);
        })
    });
    buffer.update(cx, |buffer, cx| {
        let start = buffer.text().find("Second").unwrap();
        buffer.edit([(start..start + 6, "Edited")], None, cx)
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(25));
    cx.run_until_parked();
    assert!(replacements.load(Ordering::SeqCst) > 0);
    drop(subscription);
    for _ in 0..4 {
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
    }
    preview.read_with(cx, |preview, _| {
        assert_eq!(preview.images[&0].image.id, images[&0]);
        assert_ne!(preview.images[&1].image.id, images[&1]);
        let current = preview.list.logical_scroll_top();
        assert_eq!(current.item_ix, scroll.item_ix);
        assert_eq!(current.offset_in_item, scroll.offset_in_item);
    });
    let successful = preview.read_with(cx, |preview, _| preview.compilation.clone().unwrap());
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "#missing\n")], None, cx)
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();
    preview.read_with(cx, |preview, _| {
        assert!(preview.error.is_some());
        assert!(Arc::ptr_eq(
            &successful,
            preview.compilation.as_ref().unwrap()
        ));
    });
    assert!(buffer.read_with(cx, |buffer, _| buffer.snapshot().has_diagnostics()));
    assert_eq!(
        project.read_with(cx, |project, cx| project
            .diagnostic_summary(false, cx)
            .error_count),
        1
    );
    preview.read_with(cx, |preview, cx| {
        assert_eq!(
            preview.store.read(cx).source_range(
                &preview.entry,
                &typst_engine::Location {
                    path: preview.entry.clone(),
                    range: 0..5
                },
                buffer.read(cx)
            ),
            9..14
        );
        assert_eq!(
            preview.store.read(cx).source_range(
                &preview.entry,
                &typst_engine::Location {
                    path: preview.entry.clone(),
                    range: 0..0
                },
                buffer.read(cx)
            ),
            9..9
        );
    });
    preview.update_in(cx, |preview, window, cx| {
        preview.navigate(
            Navigation::Source(typst_engine::Location {
                path: preview.entry.clone(),
                range: 0..5,
            }),
            window,
            cx,
        );
    });
    cx.run_until_parked();
    editor.read_with(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let selection = editor.selections.newest_anchor();
        assert_eq!(selection.start.to_offset(&snapshot).0, 9);
        assert_eq!(selection.end.to_offset(&snapshot).0, 14);
    });
    let (state, zoom) = preview.read_with(cx, |preview, _| {
        (
            serde_json::to_string(&preview.state).unwrap(),
            preview.state.zoom,
        )
    });
    let (database, workspace_database) = cx.update(|_, cx| {
        (
            persistence::TypstPreviewDb::global(cx),
            workspace::WorkspaceDb::global(cx),
        )
    });
    let workspace_id = workspace_database.next_id().await.unwrap();
    let item_id = 123;
    database
        .save_preview(item_id, workspace_id, state.clone())
        .await
        .unwrap();
    assert_eq!(
        database.get_preview(item_id, workspace_id).unwrap(),
        Some(state)
    );
    let restore = workspace.update_in(cx, |workspace, window, cx| {
        TypstPreview::deserialize(
            project,
            workspace.weak_handle(),
            workspace_id,
            item_id,
            window,
            cx,
        )
    });
    let restored_preview = restore.await.unwrap();
    restored_preview.read_with(cx, |preview, _| {
        assert_eq!(preview.state.page, 1);
        assert_eq!(preview.state.zoom, zoom);
        assert!(preview.state.pinned_entry.is_some());
        assert!(!preview.state.cursor_follow);
    });
    let cleanup = workspace.update_in(cx, |_, window, cx| {
        TypstPreview::cleanup(workspace_id, Vec::new(), window, cx)
    });
    cleanup.await.unwrap();
    assert!(
        database
            .get_preview(item_id, workspace_id)
            .unwrap()
            .is_none()
    );
    buffer.update(cx, |buffer, cx| {
        let pages = (0..12)
            .map(|page| format!("Page {page}"))
            .collect::<Vec<_>>()
            .join("\n#pagebreak()\n");
        let length = buffer.len();
        buffer.edit(
            [(
                0..length,
                format!("#set page(width: 100pt, height: 100pt, margin: 10pt)\n{pages}"),
            )],
            None,
            cx,
        );
    });
    preview.update(cx, |preview, cx| {
        preview.state.zoom = 0.1;
        preview.change_page(-1, cx);
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(25));
    cx.run_until_parked();
    for _ in 0..6 {
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
    }
    let images = preview.read_with(cx, |preview, _| {
        assert_eq!(preview.images.len(), 12);
        preview
            .images
            .iter()
            .map(|(page, cached)| (*page, cached.image.id))
            .collect::<HashMap<_, _>>()
    });
    for _ in 0..4 {
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
    }
    preview.read_with(cx, |preview, _| {
        for (page, image_id) in images {
            assert_eq!(preview.images[&page].image.id, image_id);
        }
        assert!(preview.raster_tasks.is_empty());
    });
}
