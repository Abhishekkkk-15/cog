use std::path::Path;

use tree_sitter::Language;

/// Shared between `search_semantic`/`semantic_replace` (find/match symbols)
/// and `memory::manager::index_file` (chunk a file for embedding) — both
/// need the same "what counts as a chunkable symbol in this language" rule.
pub struct LangSpec {
    pub language: Language,
    pub symbol_kinds: &'static [&'static str],
}

pub fn lang_spec_for(path: &Path) -> Option<LangSpec> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => Some(LangSpec {
            language: tree_sitter_rust::LANGUAGE.into(),
            symbol_kinds: &["function_item", "struct_item", "impl_item", "enum_item", "trait_item"],
        }),
        Some("py") => Some(LangSpec { language: tree_sitter_python::LANGUAGE.into(), symbol_kinds: &["function_definition", "class_definition"] }),
        Some("js") | Some("jsx") | Some("mjs") => Some(LangSpec {
            language: tree_sitter_javascript::LANGUAGE.into(),
            symbol_kinds: &["function_declaration", "class_declaration", "method_definition"],
        }),
        _ => None,
    }
}
