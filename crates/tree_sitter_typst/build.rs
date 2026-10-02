fn main() {
    println!("cargo:rerun-if-changed=src/parser.c");
    println!("cargo:rerun-if-changed=src/scanner.c");
    println!("cargo:rerun-if-changed=src/unicode.h");
    cc::Build::new()
        .include("src")
        .file("src/parser.c")
        .file("src/scanner.c")
        .warnings(false)
        .compile("tree-sitter-typst");
}
