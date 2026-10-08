use super::*;
use fs::Fs as _;
use gpui::{TestAppContext, VisualTestContext};
use pdf_document::{ParsedPdf, TextGlyph};
use std::time::Duration;
use util::path;

const SAMPLE: &[u8] = include_bytes!("../fixtures/sample.pdf");
const ROTATED: &[u8] = include_bytes!("../fixtures/rotated.pdf");
const PASSWORD: &[u8] = include_bytes!("../fixtures/password.pdf");

#[test]
fn image_only_and_unicode_pdfs() {
    let image = parse(include_bytes!("../fixtures/image.pdf"));
    assert!(image.text_page(0).unwrap().text.is_empty());
    let raster = image.rasterize(0, 1.0).unwrap();
    assert!(
        raster
            .pixels()
            .any(|pixel| pixel.0[0] > 150 && pixel.0[1] < 80)
    );
    let unicode = parse(include_bytes!("../fixtures/unicode.pdf"));
    let text = unicode.text_page(0).unwrap();
    assert_eq!(text.text, "éfi文");
    assert_eq!(text.highlights(&(3..4)).count(), 1);
    assert_eq!(text.glyphs[1].range, 2..4);
}

fn parse(bytes: &[u8]) -> ParsedPdf {
    ParsedPdf::parse(Arc::new(bytes.to_vec()), "").unwrap()
}

#[test]
fn pdf_raster_text_and_links() {
    let document = parse(SAMPLE);
    assert_eq!(document.sizes, vec![(420.0, 300.0), (300.0, 400.0)]);
    let text = document.text_page(0).unwrap();
    assert!(text.text.contains("Native PDF in Zed"), "{}", text.text);
    assert!(
        text.text
            .contains("Select text, copy it, and search every page."),
        "{}",
        text.text
    );
    let start = text.text.find("Native").unwrap();
    let rectangle = text.highlights(&(start..start + 6)).next().unwrap();
    let hit = text.hit(rectangle.center()).unwrap();
    assert!((start..=start + 6).contains(&hit));
    assert!(Arc::ptr_eq(&text, &document.text_page(0).unwrap()));
    assert_eq!(text.links.len(), 1);
    assert!(matches!(
        text.links[0].target,
        LinkTarget::Page { page: 1, .. }
    ));
    let second = document.text_page(1).unwrap();
    assert_eq!(
        second.links[0].target,
        LinkTarget::Url("https://zed.dev".into())
    );
    let raster = document.rasterize(0, 1.0).unwrap();
    assert_eq!(raster.dimensions(), (420, 300));
    assert!(raster.pixels().any(|pixel| pixel.0[0] < 150));
}

#[test]
fn rotation_and_crop_share_render_and_text_coordinates() {
    let document = parse(ROTATED);
    assert_eq!(document.sizes[0], (260.0, 380.0));
    let raster = document.rasterize(0, 1.0).unwrap();
    assert_eq!(raster.dimensions(), (260, 380));
    let text = document.text_page(0).unwrap();
    assert!(text.text.contains("Native PDF in Zed"));
    let glyph = text.glyphs.first().unwrap();
    assert!(glyph.bounds.x0 >= 0.0 && glyph.bounds.x1 <= 260.0);
    assert!(glyph.bounds.y0 >= 0.0 && glyph.bounds.y1 <= 380.0);
    assert!(text.links[0].bounds.x1 <= 260.0);
    assert_eq!(text.text, parse(SAMPLE).text_page(0).unwrap().text);
}

fn many_pages(count: usize) -> Vec<u8> {
    let pages = (0..count)
        .map(|page| format!("{} 0 R", page * 2 + 4))
        .collect::<Vec<_>>()
        .join(" ");
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        format!("<< /Type /Pages /Kids [{pages}] /Count {count} >>"),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    for page in 0..count {
        objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 420 300] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>", page * 2 + 5));
        let content = format!("BT /F1 24 Tf 40 240 Td (Page {}) Tj ET", page + 1);
        objects.push(format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ));
    }
    let mut bytes = b"%PDF-1.7\n".to_vec();
    let mut offsets = vec![0];
    for (number, object) in objects.iter().enumerate() {
        offsets.push(bytes.len());
        bytes.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", number + 1).as_bytes());
    }
    let cross_reference = bytes.len();
    bytes.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len()).as_bytes());
    for offset in offsets.iter().skip(1) {
        bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    bytes.extend_from_slice(
        format!(
            "trailer\n<< /Root 1 0 R /Size {} >>\nstartxref\n{cross_reference}\n%%EOF\n",
            offsets.len()
        )
        .as_bytes(),
    );
    bytes
}

#[gpui::test]
async fn large_pdf_renders_visible_pages_and_keeps_images_during_zoom(cx: &mut TestAppContext) {
    init_test(cx);
    let filesystem = fs::FakeFs::new(cx.executor());
    filesystem
        .insert_tree(path!("/pdf"), serde_json::json!({}))
        .await;
    filesystem
        .insert_file(path!("/pdf/large.pdf"), many_pages(120))
        .await;
    let project = Project::test(filesystem, [path!("/pdf").as_ref()], cx).await;
    let (workspace, cx) =
        cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
    cx.update(|window, cx| {
        window.replace_root(cx, |window, cx| {
            workspace::MultiWorkspace::new(workspace.clone(), window, cx)
        });
    });
    let path = project.read_with(cx, |project, cx| ProjectPath {
        worktree_id: project.worktrees(cx).next().unwrap().read(cx).id(),
        path: util::rel_path::rel_path("large.pdf").into(),
    });
    let item = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(path, None, true, window, cx)
        })
        .await
        .unwrap();
    let view = item.downcast::<PdfView>().unwrap();
    draw(cx);
    view.read_with(cx, |view, _| {
        assert_eq!(view.list.item_count(), 120);
        assert!(view.pages.len() < 120 && !view.pages.is_empty());
        assert!(view.pages.values().map(|page| page.bytes).sum::<usize>() <= CACHE_BYTES);
    });
    let images = view.read_with(cx, |view, _| view.pages.keys().copied().collect::<Vec<_>>());
    view.update_in(cx, |view, _, cx| {
        view.zoom(1.2, cx);
        assert!(images.iter().all(|page| view.pages.contains_key(page)));
    });
    draw(cx);
    for factor in [0.5, 1.2, 1.2, 0.8] {
        view.update_in(cx, |view, _, cx| view.zoom(factor, cx));
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }
    draw(cx);
    view.update_in(cx, |view, window, cx| {
        view.change_page(119, 0.0, window, cx)
    });
    draw(cx);
    view.read_with(cx, |view, _| {
        assert!(view.pages.contains_key(&119));
        assert!(view.tasks.len() <= 2);
    });
}

#[test]
fn invalid_and_password_protected_pdfs() {
    use hayro::hayro_syntax::LoadPdfError;
    assert!(matches!(
        ParsedPdf::parse(Arc::new(b"invalid".to_vec()), ""),
        Err(LoadPdfError::Invalid)
    ));
    assert!(matches!(
        ParsedPdf::parse(Arc::new(PASSWORD.to_vec()), ""),
        Err(LoadPdfError::Decryption(_))
    ));
    assert!(ParsedPdf::parse(Arc::new(PASSWORD.to_vec()), "wrong").is_err());
    let unlocked = ParsedPdf::parse(Arc::new(PASSWORD.to_vec()), "example").unwrap();
    assert_eq!(unlocked.sizes.len(), 2);
    assert!(
        unlocked
            .text_page(0)
            .unwrap()
            .text
            .contains("Native PDF in Zed")
    );
}

#[test]
fn spatial_blocks_preserve_columns_and_unicode() {
    fn glyph(text: &str, x: f64, y: f64) -> TextGlyph {
        TextGlyph {
            text: text.into(),
            bounds: kurbo::Rect::new(x, y - 8.0, x + 10.0, y + 2.0),
            origin: kurbo::Point::new(x, y),
            advance: kurbo::Point::new(x + 10.0, y),
            range: 0..0,
        }
    }
    let text = TextPage::from_glyphs(vec![
        glyph("é", 10.0, 20.0),
        glyph("fi", 100.0, 20.0),
        glyph("文", 10.0, 40.0),
        glyph("B", 100.0, 40.0),
    ]);
    assert_eq!(text.text, "é\n文\nfi\nB");
    assert_eq!(text.glyphs[2].range, 7..9);
    assert!(text.highlights(&(8..9)).next().is_some());
}

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        cx.set_global(db::AppDatabase::test_new());
        workspace::AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
    });
}

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();
    for _ in 0..5 {
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
    }
}

#[gpui::test]
async fn pdf_opening_splitting_selection_search_and_reload(cx: &mut TestAppContext) {
    init_test(cx);
    let filesystem = fs::FakeFs::new(cx.executor());
    filesystem
        .insert_tree(path!("/pdf"), serde_json::json!({}))
        .await;
    filesystem
        .insert_file(path!("/pdf/SAMPLE.PDF"), SAMPLE.to_vec())
        .await;
    let project = Project::test(filesystem.clone(), [path!("/pdf").as_ref()], cx).await;
    let (workspace, cx) =
        cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
    cx.update(|window, cx| {
        window.replace_root(cx, |window, cx| {
            workspace::MultiWorkspace::new(workspace.clone(), window, cx)
        });
    });
    let path = project.read_with(cx, |project, cx| ProjectPath {
        worktree_id: project.worktrees(cx).next().unwrap().read(cx).id(),
        path: util::rel_path::rel_path("SAMPLE.PDF").into(),
    });
    let item = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(path.clone(), None, true, window, cx)
        })
        .await
        .unwrap();
    let view = item.downcast::<PdfView>().unwrap();
    draw(cx);
    assert!(view.read_with(cx, |view, _| !view.pages.is_empty()));
    let (start, end, link) = view.read_with(cx, |view, cx| {
        let text = view.text.get(&0).unwrap();
        let bounds = view.list.bounds_for_item(0).unwrap();
        let width = view.document.read(cx).parsed.as_ref().unwrap().sizes[0].0 * view.scale;
        let origin = bounds.origin + point((bounds.size.width - px(width)) / 2.0, px(PAGE_SPACING));
        let position =
            |x: f64, y: f64| origin + point(px(x as f32 * view.scale), px(y as f32 * view.scale));
        let start = text.glyphs[0].bounds;
        let end = text.glyphs[5].bounds;
        let link = text.links[0].bounds.center();
        (
            position(start.x0 - 1.0, start.center().y),
            position(end.x1 + 0.5, end.center().y),
            position(link.x, link.y),
        )
    });
    cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
    view.read_with(cx, |view, _| {
        assert!(
            view.selection.is_some(),
            "Mouse down at {start:?} missed the page; viewport {:?}",
            view.list.viewport_bounds()
        )
    });
    cx.simulate_mouse_move(end, MouseButton::Left, Default::default());
    cx.simulate_mouse_up(end, MouseButton::Left, Default::default());
    assert_eq!(view.read_with(cx, |view, _| view.selected_text()), "Native");
    cx.simulate_click(link, Default::default());
    draw(cx);
    view.read_with(cx, |view, _| {
        assert!(
            view.list
                .bounds_for_item(1)
                .unwrap()
                .intersects(&view.list.viewport_bounds())
        )
    });
    view.update_in(cx, |view, window, cx| {
        view.page_input
            .update(cx, |editor, cx| editor.set_text("1", window, cx));
        view.page_input.focus_handle(cx).focus(window, cx);
    });
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert_eq!(view.read_with(cx, |view, _| view.state.page), 0);
    let same = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(path, None, true, window, cx)
        })
        .await
        .unwrap();
    assert_eq!(same.item_id(), view.entity_id());
    let split = view
        .update_in(cx, |view, window, cx| view.clone_on_split(None, window, cx))
        .await
        .unwrap();
    assert_eq!(
        split.read_with(cx, |view, _| view.document.clone()),
        view.read_with(cx, |view, _| view.document.clone())
    );
    split.update_in(cx, |view, _, cx| view.zoom(2.0, cx));
    assert_ne!(
        split.read_with(cx, |view, _| view.state.mode),
        view.read_with(cx, |view, _| view.state.mode)
    );
    view.update_in(cx, |view, window, cx| {
        view.select_all(&SelectAll, window, cx);
        view.copy(&Copy, window, cx);
    });
    cx.run_until_parked();
    let copied = cx.update(|_, cx| cx.read_from_clipboard().unwrap().text().unwrap());
    assert!(copied.contains("Native PDF in Zed") && copied.contains("A second page"));
    let query = Arc::new(
        SearchQuery::text(
            "alpha",
            false,
            false,
            false,
            Default::default(),
            Default::default(),
            false,
            None,
        )
        .unwrap(),
    );
    let (matches, token) = view
        .update_in(cx, |view, window, cx| {
            view.find_matches_with_token(query, window, cx)
        })
        .await;
    assert_eq!(matches.len(), 2);
    view.update_in(cx, |view, window, cx| {
        view.update_matches(&matches, Some(0), token, window, cx);
        view.activate_match(0, &matches, token, window, cx);
    });
    draw(cx);
    view.read_with(cx, |view, _| {
        let found = &matches[0];
        let bounds = view
            .text
            .get(&found.page)
            .unwrap()
            .highlights(&found.range)
            .next()
            .unwrap();
        let position = view.list.bounds_for_item(found.page).unwrap().origin.y
            + px(PAGE_SPACING + bounds.center().y as f32 * view.scale);
        assert!(f32::from(position - view.list.viewport_bounds().center().y).abs() < 2.0);
    });
    let state = view.read_with(cx, |view, _| ViewState {
        zoom: 1.25,
        mode: ZoomMode::Custom,
        page: 1,
        offset: 22.0,
        ..view.state.clone()
    });
    let (database, workspaces) = cx.update(|_, cx| {
        (
            persistence::PdfViewDb::global(cx),
            workspace::WorkspaceDb::global(cx),
        )
    });
    let workspace_id = workspaces.next_id().await.unwrap();
    database
        .save_view(987, workspace_id, serde_json::to_string(&state).unwrap())
        .await
        .unwrap();
    let restored = workspace
        .update_in(cx, |workspace, window, cx| {
            PdfView::deserialize(
                project.clone(),
                workspace.weak_handle(),
                workspace_id,
                987,
                window,
                cx,
            )
        })
        .await
        .unwrap();
    restored.read_with(cx, |view, _| {
        assert_eq!(view.state.page, 1);
        assert_eq!(view.state.zoom, 1.25);
        assert_eq!(view.state.offset, 22.0);
    });
    workspace
        .update_in(cx, |_, window, cx| {
            PdfView::cleanup(workspace_id, Vec::new(), window, cx)
        })
        .await
        .unwrap();
    assert!(database.get_view(987, workspace_id).unwrap().is_none());
    let original = view.read_with(cx, |view, cx| view.document.read(cx).generation);
    filesystem
        .insert_file(path!("/pdf/SAMPLE.PDF"), ROTATED.to_vec())
        .await;
    draw(cx);
    view.read_with(cx, |view, cx| {
        assert!(view.document.read(cx).generation > original);
        assert_eq!(
            view.document.read(cx).parsed.as_ref().unwrap().sizes[0],
            (260.0, 380.0)
        );
        assert!(view.matches.is_empty());
    });
    filesystem
        .insert_file(path!("/pdf/SAMPLE.PDF"), b"partial write".to_vec())
        .await;
    draw(cx);
    view.read_with(cx, |view, cx| {
        assert!(view.document.read(cx).error.is_some());
        assert_eq!(
            view.document.read(cx).parsed.as_ref().unwrap().sizes.len(),
            2
        );
    });
    filesystem
        .insert_file(path!("/pdf/SAMPLE.PDF"), SAMPLE.to_vec())
        .await;
    draw(cx);
    filesystem
        .rename(
            path!("/pdf/SAMPLE.PDF").as_ref(),
            path!("/pdf/renamed.pdf").as_ref(),
            Default::default(),
        )
        .await
        .unwrap();
    draw(cx);
    assert_eq!(
        view.read_with(cx, |view, cx| view.tab_content_text(0, cx)),
        "renamed.pdf"
    );
    let renamed_path = project.read_with(cx, |project, cx| ProjectPath {
        worktree_id: project.worktrees(cx).next().unwrap().read(cx).id(),
        path: util::rel_path::rel_path("renamed.pdf").into(),
    });
    let same = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(renamed_path, None, true, window, cx)
        })
        .await
        .unwrap();
    assert_eq!(same.item_id(), view.entity_id());
    filesystem
        .remove_file(path!("/pdf/renamed.pdf").as_ref(), Default::default())
        .await
        .unwrap();
    draw(cx);
    assert!(view.read_with(cx, |view, cx| view.has_deleted_file(cx)));
}

#[gpui::test]
async fn password_and_view_state_are_separate(cx: &mut TestAppContext) {
    init_test(cx);
    let filesystem = fs::FakeFs::new(cx.executor());
    filesystem
        .insert_tree(path!("/pdf"), serde_json::json!({}))
        .await;
    filesystem
        .insert_file(path!("/pdf/locked.pdf"), PASSWORD.to_vec())
        .await;
    let project = Project::test(filesystem, [path!("/pdf").as_ref()], cx).await;
    let path = project.read_with(cx, |project, cx| ProjectPath {
        worktree_id: project.worktrees(cx).next().unwrap().read(cx).id(),
        path: util::rel_path::rel_path("locked.pdf").into(),
    });
    let document = cx
        .update(|cx| PdfDocument::open(project, path, cx))
        .await
        .unwrap();
    assert!(document.read_with(cx, |document, _| document.password_required));
    document.update(cx, |document, cx| document.unlock("example".into(), cx));
    cx.run_until_parked();
    assert!(!document.read_with(cx, |document, _| document.password_required));
    let state = ViewState {
        path: path!("/pdf/locked.pdf").into(),
        zoom: 1.25,
        mode: ZoomMode::Custom,
        page: 1,
        offset: 55.0,
    };
    let saved = serde_json::to_string(&state).unwrap();
    assert!(!saved.contains("example"));
    let restored: ViewState = serde_json::from_str(&saved).unwrap();
    assert_eq!(restored.page, 1);
    assert_eq!(restored.offset, 55.0);
    assert_eq!(restored.zoom, 1.25);
}
