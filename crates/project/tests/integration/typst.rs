use super::*;
use pretty_assertions::assert_eq;
use settings::Settings as _;

#[gpui::test]
async fn native_typst_uses_unsaved_buffers_and_never_starts_a_server(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/typst-project"),
        json!({
            "main.typ": "#import \"chapter.typ\": value\n#value\n#tex",
            "chapter.typ": "#let value = 1",
        }),
    )
    .await;
    let project = Project::test(fs, [path!("/typst-project").as_ref()], cx).await;
    let registry = project.read_with(cx, |project, _| project.languages().clone());
    let mut servers = registry.register_fake_lsp(
        "Typst",
        FakeLspAdapter {
            name: "tinymist",
            ..Default::default()
        },
    );
    registry.add(Arc::new(Language::new(
        LanguageConfig {
            name: "Typst".into(),
            matcher: LanguageMatcher {
                path_suffixes: vec!["typ".into(), "typst".into()],
                ..Default::default()
            }
            .into(),
            ..Default::default()
        },
        None,
    )));
    cx.update(|cx| {
        typst_store::TypstSettings::override_global(
            typst_store::TypstSettings(typst_engine::Configuration {
                system_fonts: false,
                package_downloads: false,
                ..Default::default()
            }),
            cx,
        );
    });
    let chapter = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/typst-project/chapter.typ"), cx)
        })
        .await
        .unwrap();
    let main = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/typst-project/main.typ"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    assert!(
        servers.next().now_or_never().is_none(),
        "Typst started an LSP server"
    );
    chapter.update(cx, |buffer, cx| buffer.edit([(13..14, "2")], None, cx));
    let completion = project
        .update(cx, |project, cx| {
            let position = main.read(cx).len();
            project.completions(&main, position, DEFAULT_COMPLETION_CONTEXT, cx)
        })
        .await
        .unwrap();
    assert!(
        completion
            .iter()
            .flat_map(|response| &response.completions)
            .any(|completion| completion.label.filter_text() == "text")
    );
    assert!(
        completion
            .iter()
            .flat_map(|response| &response.completions)
            .all(|completion| completion.source.server_id().is_none())
    );
    let definition = project.update(cx, |project, cx| {
        project.definitions(&main, PointUtf16::new(1, 2), cx)
    });
    chapter.update(cx, |buffer, cx| buffer.edit([(0..0, "\n")], None, cx));
    let definition = definition.await.unwrap().unwrap();
    assert_eq!(definition.len(), 1);
    assert_eq!(definition[0].target.buffer, chapter);
    chapter.read_with(cx, |buffer, _| {
        let range = definition[0].target.range.to_offset(buffer);
        assert_eq!(&buffer.text()[range], "value");
    });
    chapter.update(cx, |buffer, cx| buffer.edit([(0..1, "")], None, cx));
    let hover = project
        .update(cx, |project, cx| {
            project.hover(&main, PointUtf16::new(1, 2), cx)
        })
        .await
        .unwrap();
    assert!(
        hover
            .iter()
            .flat_map(|hover| &hover.contents)
            .any(|block| block.text.contains('2'))
    );
    let prepared = project
        .update(cx, |project, cx| {
            project.prepare_rename(main.clone(), PointUtf16::new(1, 2), cx)
        })
        .await
        .unwrap();
    assert!(matches!(
        prepared,
        PrepareRenameResponse::Success {
            language_server_id: None,
            ..
        }
    ));
    let transaction = project
        .update(cx, |project, cx| {
            project.perform_rename(
                main.clone(),
                PointUtf16::new(1, 2),
                "amount".into(),
                None,
                cx,
            )
        })
        .await
        .unwrap();
    assert_eq!(transaction.0.len(), 2);
    assert!(
        chapter
            .read_with(cx, |buffer, _| buffer.text())
            .contains("#let amount = 2")
    );
    assert!(
        main.read_with(cx, |buffer, _| buffer.text())
            .contains("#amount")
    );
    for (buffer, transaction) in &transaction.0 {
        buffer.update(cx, |buffer, cx| buffer.undo_transaction(transaction.id, cx));
    }
    assert!(
        main.read_with(cx, |buffer, _| buffer.text())
            .contains("#value")
    );
    assert!(
        chapter
            .read_with(cx, |buffer, _| buffer.text())
            .contains("#let value = 2")
    );
    assert!(servers.next().now_or_never().is_none());
}

#[gpui::test]
async fn native_typst_coalesces_edits_and_merges_entry_diagnostics(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| {
        typst_store::TypstSettings::override_global(
            typst_store::TypstSettings(typst_engine::Configuration {
                system_fonts: false,
                package_downloads: false,
                ..Default::default()
            }),
            cx,
        )
    });
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/typst"),
        json!({ "main.typ": "#first_missing", "other.typ": "#second_missing", "figure.svg": "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"16\" height=\"16\"><rect width=\"16\" height=\"16\" fill=\"red\"/></svg>" }),
    )
    .await;
    let project = Project::test(fs, [path!("/typst").as_ref()], cx).await;
    project
        .read_with(cx, |project, _| project.languages().clone())
        .add(Arc::new(Language::new(
            LanguageConfig {
                name: "Typst".into(),
                matcher: LanguageMatcher {
                    path_suffixes: vec!["typ".into()],
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            },
            None,
        )));
    let main = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/typst/main.typ"), cx)
        })
        .await
        .unwrap();
    let other = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/typst/other.typ"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(200));
    cx.run_until_parked();
    assert_eq!(
        project.read_with(cx, |project, cx| project
            .diagnostic_summary(false, cx)
            .error_count),
        2
    );
    assert!(main.read_with(cx, |buffer, _| buffer.snapshot().has_diagnostics()));
    assert!(other.read_with(cx, |buffer, _| buffer.snapshot().has_diagnostics()));
    main.read_with(cx, |buffer, _| {
        let snapshot = buffer.snapshot();
        assert!(
            snapshot
                .chunks(
                    0..snapshot.len(),
                    LanguageAwareStyling {
                        tree_sitter: true,
                        diagnostics: true,
                    },
                )
                .any(|chunk| chunk.underline
                    && chunk.diagnostic_severity == Some(DiagnosticSeverity::ERROR))
        );
    });
    main.update(cx, |buffer, cx| {
        let length = buffer.len();
        buffer.edit([(0..length, "#another_missing")], None, cx);
        let length = buffer.len();
        buffer.edit(
            [(
                0..length,
                "A valid final draft\n#image(\"figure.svg\", width: 10pt)",
            )],
            None,
            cx,
        );
    });
    cx.run_until_parked();
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(200));
    cx.run_until_parked();
    assert_eq!(
        project.read_with(cx, |project, cx| project
            .diagnostic_summary(false, cx)
            .error_count),
        1
    );
    assert!(!main.read_with(cx, |buffer, _| buffer.snapshot().has_diagnostics()));
    assert!(other.read_with(cx, |buffer, _| buffer.snapshot().has_diagnostics()));
    project.update(cx, |project, cx| {
        let request = project.typst_request(&main, cx).unwrap();
        let compilation = project
            .typst_store(cx)
            .read(cx)
            .pages(&request.input.entry)
            .unwrap();
        assert_eq!(
            compilation
                .snapshot
                .source_at(path!("/typst/main.typ").as_ref())
                .unwrap()
                .text(),
            "A valid final draft\n#image(\"figure.svg\", width: 10pt)"
        );
    });
    for index in 0..4 {
        main.update(cx, |buffer, cx| {
            let length = buffer.len();
            buffer.edit([(0..length, format!("Continuous draft {index}"))], None, cx);
        });
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(5));
        cx.run_until_parked();
    }
    project.update(cx, |project, cx| {
        let request = project.typst_request(&main, cx).unwrap();
        let compilation = project
            .typst_store(cx)
            .read(cx)
            .pages(&request.input.entry)
            .unwrap();
        assert_eq!(
            compilation
                .snapshot
                .source_at(path!("/typst/main.typ").as_ref())
                .unwrap()
                .text(),
            "Continuous draft 3"
        );
    });
}
