---
title: Typst
description: "Edit Typst with native completion, diagnostics, and live page preview."
---

# Typst

Open a local folder and a `.typ` file to use built-in Typst editing and live
preview. Typst support uses the compiler directly and does not start a language
server. You do not need Tinymist or a Typst extension.

## Live preview {#live-preview}

Save your file, then click the eye button in the editor toolbar to open a preview.
Alt-click the button to open it in a split. You can also choose **Open Typst
Preview** from the editor context menu.

{#action typst::OpenPreviewToTheSide} opens a preview in a split with
{#kb typst::OpenPreviewToTheSide}. On macOS, press `Cmd+K`, release it, then press
`V`. {#action typst::OpenPreview} opens a preview in the current pane with
{#kb typst::OpenPreview}.

The preview renders native pages inside Zed. It updates from unsaved changes in
all open files, including imported files. An error appears above the preview and
in Zed's diagnostics UI; the last successful pages remain visible.

Use the preview toolbar to change pages, zoom, fit the page width, and follow the
source cursor. Click rendered content to navigate to its source. Document links
navigate between pages or open their destination.

{#action typst::OpenFollowingPreview} follows the active Typst editor. **Pin main**
keeps compiling the current entry file while you switch to its imported files.
The preview restores its entry, page, zoom, and follow settings with the workspace.
Text selection in rendered pages is not supported.

## Editing {#editing}

Typst includes syntax highlighting, indentation, an outline, completion snippets,
hover information, signature help, go to definition, references, document
highlights, and rename. Formatting uses Typstyle through {#action editor::Format},
including selected ranges, and respects your preferred line length.

Rename resolves source bindings and checks the proposed edits before applying
one undoable transaction. It rejects ambiguous labels, syntax errors, name
collisions, and bindings that cannot be renamed safely. Rename does not modify
packages or string-based lookups.

## Configuration {#configuration}

Add a `typst` object to your settings file, or to `.zed/settings.json` for a
project:

```json [settings]
{
  "typst": {
    "root": ".",
    "main_file": "main.typ",
    "font_paths": ["fonts"],
    "inputs": { "edition": "draft" },
    "system_fonts": true,
    "package_downloads": true
  }
}
```

`root` defaults to the opened folder. Relative root paths resolve from that
folder; `main_file` and font paths resolve from the Typst root. Without
`main_file`, the active file is the compilation entry.

Project font paths, system fonts, and embedded Typst fonts are available to the
compiler. Standard packages use Typst's package cache and download missing
packages when `package_downloads` is enabled. Disable it for offline use.

Restricted projects use the opened folder as their root and disable package
downloads, system font discovery, and project configuration overrides. Native
Typst support currently requires local files; SSH remote projects do not use this
engine.

See [language settings](../configuring-languages.md) for editor options.
