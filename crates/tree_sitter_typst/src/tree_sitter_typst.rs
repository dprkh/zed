use tree_sitter_language::LanguageFn;

unsafe extern "C" {
    fn tree_sitter_typst() -> *const ();
}

// Upstream revision abe60cbed7986ee475d93f816c1be287f220c5d8 has an old
// tree-sitter::Language binding; LanguageFn avoids mixing incompatible Rust ABIs.
pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_typst) };
pub const NODE_TYPES: &str = include_str!("node-types.json");

#[cfg(test)]
mod tests {
    #[test]
    fn built_in_queries_match_the_vendored_grammar() {
        let language = super::LANGUAGE.into();
        for (name, query) in [
            (
                "highlights",
                include_str!("../../grammars/src/typst/highlights.scm"),
            ),
            (
                "brackets",
                include_str!("../../grammars/src/typst/brackets.scm"),
            ),
            (
                "indents",
                include_str!("../../grammars/src/typst/indents.scm"),
            ),
            (
                "outline",
                include_str!("../../grammars/src/typst/outline.scm"),
            ),
            (
                "injections",
                include_str!("../../grammars/src/typst/injections.scm"),
            ),
            (
                "overrides",
                include_str!("../../grammars/src/typst/overrides.scm"),
            ),
            (
                "textobjects",
                include_str!("../../grammars/src/typst/textobjects.scm"),
            ),
        ] {
            tree_sitter::Query::new(&language, query)
                .unwrap_or_else(|error| panic!("Invalid Typst {name} query: {error}"));
        }
    }

    #[test]
    fn loads_native_grammar() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&super::LANGUAGE.into())
            .expect("Typst grammar");
        let tree = parser
            .parse("= Heading\n#let greet(name) = [Hello #name]\n$ x^2 $", None)
            .expect("Typst parse");
        assert!(!tree.root_node().has_error());
    }
}
