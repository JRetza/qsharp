// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// Represents a span of text used by the Language Server API
#[derive(Debug, PartialEq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

#[derive(Copy, Clone, Debug, PartialEq)]
#[allow(clippy::module_name_repetitions)]
pub enum CompletionItemKind {
    // It would have been nice to match these enum values to the ones used by
    // VS Code and Monaco, but unfortunately those two disagree on the values.
    // So we define our own unique enum here to reduce confusion.
    Function,
    Interface,
    Keyword,
    Module,
}

#[derive(Debug)]
#[allow(clippy::module_name_repetitions)]
pub struct CompletionList {
    pub items: Vec<CompletionItem>,
}

#[derive(Debug)]
#[allow(clippy::module_name_repetitions)]
pub struct CompletionItem {
    pub label: String,
    pub kind: CompletionItemKind,
    pub sort_text: Option<String>,
    pub detail: Option<String>,
    pub additional_text_edits: Option<Vec<(Span, String)>>,
}

impl CompletionItem {
    #[must_use]
    pub fn new(label: String, kind: CompletionItemKind) -> Self {
        CompletionItem {
            label,
            kind,
            sort_text: None,
            detail: None,
            additional_text_edits: None,
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct Definition {
    pub source: String,
    pub offset: u32,
}

#[derive(Debug, PartialEq)]
pub struct Hover {
    pub contents: String,
    pub span: Span,
}