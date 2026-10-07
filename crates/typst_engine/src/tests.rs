use super::*;

fn engine(root: &Path) -> Engine {
    Engine::new(
        Configuration {
            system_fonts: false,
            package_downloads: false,
            ..Default::default()
        },
        root,
    )
}

fn input(root: &Path, files: &[(&str, &str)]) -> Input {
    Input {
        root: root.to_owned(),
        entry: root.join("main.typ"),
        generation: 1,
        files: files
            .iter()
            .map(|(path, text)| (root.join(path), Arc::from(*text)))
            .collect(),
        known_files: files.iter().map(|(path, _)| root.join(path)).collect(),
    }
}

#[test]
fn unsaved_imports_and_snapshot_isolation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    std::fs::write(root.join("chapter.typ"), "#let title = [Saved]")?;
    let engine = engine(root);
    let first = engine.snapshot(input(
        root,
        &[
            ("main.typ", "#import \"chapter.typ\": title\n#title"),
            ("chapter.typ", "#let title = [Unsaved]"),
        ],
    ))?;
    let result = first.compile();
    assert!(result.document.is_some(), "{:?}", result.diagnostics);
    assert!(result.diagnostics.is_empty());
    assert!(!result.page_sizes().is_empty());
    assert!(result.svg(0)?.contains("<svg"));
    let definition = first
        .definition(result.document.as_ref(), &root.join("main.typ"), 30)?
        .context("Missing import definition")?;
    assert_eq!(definition.path, root.join("chapter.typ"));
    let second = engine.snapshot(input(
        root,
        &[
            ("main.typ", "#import \"chapter.typ\": title\n#title"),
            ("chapter.typ", "#let title = [New draft]"),
        ],
    ))?;
    assert!(
        second
            .source_at(&root.join("chapter.typ"))?
            .text()
            .contains("New draft")
    );
    assert!(
        first
            .source_at(&root.join("chapter.typ"))?
            .text()
            .contains("Unsaved")
    );
    Ok(())
}

#[test]
fn completion_signature_hover_and_unicode() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let engine = engine(root);
    let text = "你好\n#text(size: 12pt)[Hello]\n#tex";
    let snapshot = engine.snapshot(input(root, &[("main.typ", text)]))?;
    let (_, completions) = snapshot
        .completions(None, &root.join("main.typ"), text.len(), true)?
        .context("Missing completions")?;
    assert!(
        completions
            .iter()
            .any(|completion| completion.label == "text")
    );
    let signature = snapshot
        .signature(&root.join("main.typ"), text.find("12pt").unwrap())?
        .context("Missing signature")?;
    assert!(signature.label.starts_with("text("));
    assert!(
        snapshot
            .hover(None, &root.join("main.typ"), text.find("text").unwrap())?
            .is_some()
    );
    assert!(
        snapshot
            .completions(None, &root.join("main.typ"), 1, true)
            .is_err()
    );
    Ok(())
}

#[test]
fn diagnostics_and_source_navigation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let engine = engine(root);
    let snapshot = engine.snapshot(input(root, &[("main.typ", "Hello world\n#missing")]))?;
    let result = snapshot.compile();
    assert!(result.document.is_none());
    let diagnostic = result.diagnostics.first().context("Missing diagnostic")?;
    assert!(diagnostic.message.contains("unknown variable"));
    let location = diagnostic
        .location
        .as_ref()
        .context("Missing diagnostic location")?;
    assert_eq!(location.path, root.join("main.typ"));
    assert_eq!(location.range, 13..20);
    let snapshot = engine.snapshot(input(root, &[("main.typ", "Hello world")]))?;
    let result = snapshot.compile();
    let (page, x, y) = result
        .jump_from_source(&root.join("main.typ"), 3)
        .context("Missing source to page mapping")?;
    assert_eq!(page, 0);
    assert!(result.jump_from_page(page, x, y).is_some());
    Ok(())
}

#[test]
fn native_rasterization_and_unchanged_page_fingerprints() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let engine = engine(root);
    let source = "#set page(width: 100pt, height: 100pt, margin: 10pt, fill: rgb(\"#ff0000\"))\nFirst page\n#pagebreak()\nSecond page";
    let first = engine
        .snapshot(input(root, &[("main.typ", source)]))?
        .compile();
    assert_eq!(first.page_sizes(), &[(100.0, 100.0), (100.0, 100.0)]);
    let raster = first.rasterize(0, 2.0)?;
    assert_eq!((raster.width, raster.height), (200, 200));
    assert_eq!(raster.pixels.len(), 200 * 200 * 4);
    assert_eq!(&raster.pixels[..4], &[255, 0, 0, 255]);
    assert!(first.rasterize(2, 1.0).is_err());
    assert!(first.rasterize(0, 0.0).is_err());
    assert!(first.rasterize(0, f32::NAN).is_err());
    let second = engine
        .snapshot(input(
            root,
            &[("main.typ", &source.replace("First", "Edited"))],
        ))?
        .compile();
    assert_ne!(first.page_fingerprint(0), second.page_fingerprint(0));
    assert_eq!(first.page_fingerprint(1), second.page_fingerprint(1));
    Ok(())
}

#[test]
fn explicit_pagebreaks_separate_auto_height_pages() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let engine = engine(root);
    for settings in ["", "#set page(height: auto)\n"] {
        for (body, expected) in [
            (
                "First page\n#pagebreak()\nSecond page\n\nAnother paragraph",
                2,
            ),
            ("First page\n#pagebreak()", 2),
            ("First page\n#pagebreak()\n#pagebreak()\nThird page", 3),
            ("#pagebreak(weak: true)\nFirst page", 1),
        ] {
            let source = format!("{settings}{body}");
            let compilation = engine
                .snapshot(input(root, &[("main.typ", &source)]))?
                .compile();
            assert!(
                compilation.diagnostics.is_empty(),
                "{:?}",
                compilation.diagnostics
            );
            assert_eq!(compilation.page_sizes().len(), expected, "{source}");
            for page in 0..expected {
                let raster = compilation.rasterize(page, 0.25)?;
                assert!(raster.width > 0 && raster.height > 0);
            }
            if let Some(offset) = source.find("Second page") {
                let (page, _, _) = compilation
                    .jump_from_source(&root.join("main.typ"), offset)
                    .context("Missing second-page source mapping")?;
                assert_eq!(page, 1);
                if !settings.is_empty() {
                    assert_ne!(compilation.page_sizes()[0].1, compilation.page_sizes()[1].1);
                }
            }
        }
    }
    Ok(())
}

#[test]
fn formatting_respects_selection_and_utf8() -> Result<()> {
    let text = "你好\n#let first=1+2\n#let second=3+4\n";
    let start = text.find("second").unwrap();
    let (range, replacement) = format(text, 80, Some(start..text.len() - 1))?;
    assert!(range.start >= text.find("#let second").unwrap());
    assert!(replacement.contains("3 + 4"));
    assert!(format(text, 80, Some(1..2)).is_err());
    assert!(format("#let =", 80, None).is_err());
    Ok(())
}

#[test]
fn references_distinguish_shadowing_and_import_aliases() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let text = "#let amount = 1\n#amount\n#{let amount = 2; amount}\n#amount";
    let snapshot = engine(root).snapshot(input(root, &[("main.typ", text)]))?;
    let index = references::Index::new(&snapshot)?;
    let locations = index.references(&root.join("main.typ"), 5)?;
    assert_eq!(locations.len(), 3, "{locations:?}");
    let edits = index.rename(&snapshot, &root.join("main.typ"), 5, "total")?;
    assert_eq!(edits.len(), 3);
    let snapshot = engine(root).snapshot(input(
        root,
        &[
            (
                "main.typ",
                "#import \"chapter.typ\": amount as total\n#total\n#amount",
            ),
            ("chapter.typ", "#let amount = 2\n#amount"),
        ],
    ))?;
    let index = references::Index::new(&snapshot)?;
    let locations = index.references(&root.join("main.typ"), 37)?;
    assert_eq!(locations.len(), 2, "{locations:?}");
    assert!(
        locations
            .iter()
            .all(|location| location.path == root.join("main.typ"))
    );
    Ok(())
}

#[test]
fn rename_labels_and_reject_unsafe_edits() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let snapshot = engine(root).snapshot(input(
        root,
        &[(
            "main.typ",
            "= Heading <heading>\n@heading\n#let first = 1\n#let second = 2\n#first",
        )],
    ))?;
    let index = references::Index::new(&snapshot)?;
    assert_eq!(
        index
            .rename(&snapshot, &root.join("main.typ"), 12, "section")?
            .len(),
        2
    );
    assert!(
        index
            .rename(&snapshot, &root.join("main.typ"), 33, "second")
            .is_err()
    );
    assert!(
        index
            .rename(&snapshot, &root.join("main.typ"), 33, "invalid name")
            .is_err()
    );
    let snapshot = engine(root).snapshot(input(
        root,
        &[("main.typ", "#let first = 1\n#first\n#let =")],
    ))?;
    assert!(
        references::Index::new(&snapshot)?
            .rename(&snapshot, &root.join("main.typ"), 5, "total")
            .is_err()
    );
    Ok(())
}

#[test]
fn rename_named_parameters_module_fields_and_unopened_imports() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let main = "#import \"chapter.typ\" as chapter\n#chapter.amount\n#chapter.greet(size: 2)";
    std::fs::write(
        root.join("chapter.typ"),
        "#let amount = 1\n#let greet(size: 1) = size",
    )?;
    let mut input = input(root, &[("main.typ", main)]);
    input.known_files.push(root.join("chapter.typ"));
    let snapshot = engine(root).snapshot(input)?;
    let index = references::Index::new(&snapshot)?;
    let edits = index.rename(
        &snapshot,
        &root.join("main.typ"),
        main.find("amount").unwrap(),
        "total",
    )?;
    assert_eq!(edits.len(), 2, "{edits:?}");
    let edits = index.rename(
        &snapshot,
        &root.join("main.typ"),
        main.find("size").unwrap(),
        "width",
    )?;
    assert_eq!(edits.len(), 3, "{edits:?}");
    let edits = index.rename(
        &snapshot,
        &root.join("main.typ"),
        main.find("as chapter").unwrap() + 3,
        "section",
    )?;
    assert_eq!(edits.len(), 3, "{edits:?}");
    Ok(())
}

#[test]
fn implicit_module_names_and_duplicate_labels_are_not_renamed() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let snapshot = engine(root).snapshot(input(
        root,
        &[
            (
                "main.typ",
                "#import \"chapter.typ\"\n#chapter.amount\n<x>\n<x>\n@x",
            ),
            ("chapter.typ", "#let amount = 1"),
        ],
    ))?;
    let index = references::Index::new(&snapshot)?;
    assert!(
        index
            .rename(&snapshot, &root.join("main.typ"), 25, "section")
            .is_err()
    );
    assert!(
        index
            .rename(&snapshot, &root.join("main.typ"), 46, "section")
            .is_err()
    );
    Ok(())
}

#[test]
fn compiled_labels_images_fonts_and_inputs() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    std::fs::write(
        root.join("figure.svg"),
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32"><circle cx="16" cy="16" r="12" fill="red"/></svg>"#,
    )?;
    let text = "#set heading(numbering: \"1.\")\n#set text(font: \"Libertinus Serif\")\n= Heading <intro>\n#image(\"figure.svg\", width: 20pt)\n#sys.inputs.at(\"mode\")\nSee @intro";
    let engine = Engine::new(
        Configuration {
            system_fonts: false,
            package_downloads: false,
            inputs: BTreeMap::from([("mode".into(), "draft".into())]),
            ..Default::default()
        },
        root,
    );
    let snapshot = engine.snapshot(input(root, &[("main.typ", text)]))?;
    let compilation = snapshot.compile();
    assert!(
        compilation.document.is_some(),
        "{:?}",
        compilation.diagnostics
    );
    assert!(compilation.svg(0)?.contains("<image"));
    let raster = compilation.rasterize(0, 1.0)?;
    assert!(
        raster
            .pixels
            .chunks_exact(4)
            .any(|pixel| pixel == [255, 0, 0, 255]),
        "The native preview did not render the SVG image"
    );
    let definition = snapshot
        .definition(
            compilation.document.as_ref(),
            &root.join("main.typ"),
            text.rfind("intro").unwrap() + 1,
        )?
        .context("Missing label definition")?;
    assert_eq!(definition.path, root.join("main.typ"));
    let draft = engine.snapshot(input(root, &[("main.typ", &format!("{text}\n@in"))]))?;
    let (_, completions) = draft
        .completions(
            compilation.document.as_ref(),
            &root.join("main.typ"),
            text.len() + 4,
            true,
        )?
        .context("Missing label completions")?;
    assert!(
        completions
            .iter()
            .any(|completion| completion.label.contains("intro")),
        "{completions:?}"
    );
    Ok(())
}
