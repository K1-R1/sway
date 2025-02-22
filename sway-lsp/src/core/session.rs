use crate::{
    capabilities::{
        self,
        formatting::get_page_text_edit,
        runnable::{Runnable, RunnableType},
    },
    core::{
        dependency,
        document::TextDocument,
        token::{Token, TokenMap, TypeDefinition},
        {traverse_parse_tree, traverse_typed_tree},
    },
    error::{DocumentError, LanguageServerError},
    utils::{self, sync::SyncWorkspace, token::to_ident_key},
};
use dashmap::DashMap;
use forc_pkg::{self as pkg};
use parking_lot::RwLock;
use pkg::manifest::ManifestFile;
use std::{path::PathBuf, sync::Arc};
use sway_core::{
    language::{
        parsed::{AstNode, ParseProgram},
        ty,
    },
    CompileResult, TypeEngine,
};
use sway_types::{Ident, Span, Spanned};
use sway_utils::helpers::get_sway_files;
use tower_lsp::lsp_types::{
    CompletionItem, Diagnostic, GotoDefinitionResponse, Location, Position, Range,
    SymbolInformation, TextDocumentContentChangeEvent, TextEdit, Url,
};

pub type Documents = DashMap<String, TextDocument>;
pub type ProjectDirectory = PathBuf;

#[derive(Default, Debug)]
pub struct CompiledProgram {
    pub parsed: Option<ParseProgram>,
    pub typed: Option<ty::TyProgram>,
}

#[derive(Debug)]
pub struct Session {
    pub documents: Documents,
    pub token_map: TokenMap,
    pub runnables: DashMap<RunnableType, Runnable>,
    pub compiled_program: RwLock<CompiledProgram>,
    pub sync: SyncWorkspace,
    pub type_engine: RwLock<TypeEngine>,
}

impl Session {
    pub fn new() -> Self {
        Session {
            documents: DashMap::new(),
            token_map: DashMap::new(),
            runnables: DashMap::new(),
            compiled_program: RwLock::new(Default::default()),
            sync: SyncWorkspace::new(),
            type_engine: <_>::default(),
        }
    }

    pub fn init(&self, uri: &Url) -> Result<ProjectDirectory, LanguageServerError> {
        *self.type_engine.write() = <_>::default();

        let manifest_dir = PathBuf::from(uri.path());
        // Create a new temp dir that clones the current workspace
        // and store manifest and temp paths
        self.sync.create_temp_dir_from_workspace(&manifest_dir)?;

        self.sync.clone_manifest_dir_to_temp()?;

        // iterate over the project dir, parse all sway files
        let _ = self.parse_and_store_sway_files();

        self.sync.watch_and_sync_manifest();

        self.sync.manifest_dir().map_err(Into::into)
    }

    pub fn shutdown(&self) {
        // shutdown the thread watching the manifest file
        let handle = self.sync.notify_join_handle.read();
        if let Some(join_handle) = &*handle {
            join_handle.abort();
        }

        // Delete the temporary directory.
        self.sync.remove_temp_dir();
    }

    /// Check if the code editor's cursor is currently over one of our collected tokens.
    pub fn token_at_position(&self, uri: &Url, position: Position) -> Option<(Ident, Token)> {
        let tokens = self.tokens_for_file(uri);
        match utils::common::ident_at_position(position, tokens) {
            Some(ident) => self.token_map.get(&to_ident_key(&ident)).map(|item| {
                let ((ident, _), token) = item.pair();
                (ident.clone(), token.clone())
            }),
            None => None,
        }
    }

    /// Find all references in the session for a given token.
    ///
    /// This is useful for the highlighting and renaming LSP capabilities.
    pub fn all_references_of_token<'s>(
        &'s self,
        token: &Token,
    ) -> impl 's + Iterator<Item = (Ident, Token)> {
        let current_type_id = self.declared_token_span(token);

        self.token_map
            .iter()
            .filter(move |item| {
                let ((_, _), token) = item.pair();
                current_type_id == self.declared_token_span(token)
            })
            .map(|item| {
                let ((ident, _), token) = item.pair();
                (ident.clone(), token.clone())
            })
    }

    /// Return a TokenMap with tokens belonging to the provided file path
    pub fn tokens_for_file<'s>(
        &'s self,
        uri: &'s Url,
    ) -> impl 's + Iterator<Item = (Ident, Token)> {
        self.token_map
            .iter()
            .filter(|item| {
                let (_, span) = item.key();
                match span.path() {
                    Some(path) => path.to_str() == Some(uri.path()),
                    None => false,
                }
            })
            .map(|item| {
                let ((ident, _), token) = item.pair();
                (ident.clone(), token.clone())
            })
    }

    /// Return the `Ident` of the declaration of the provided token.
    pub fn declared_token_ident(&self, token: &Token) -> Option<Ident> {
        token.type_def.as_ref().and_then(|type_def| match type_def {
            TypeDefinition::TypeId(type_id) => {
                utils::token::ident_of_type_id(&self.type_engine.read(), type_id)
            }
            TypeDefinition::Ident(ident) => Some(ident.clone()),
        })
    }

    /// Return the `Span` of the declaration of the provided token. This is useful for
    /// performaing == comparisons on spans. We need to do this instead of comparing
    /// the `Ident` because the `Ident` eq is only comparing the str name.
    pub fn declared_token_span(&self, token: &Token) -> Option<Span> {
        token.type_def.as_ref().and_then(|type_def| match type_def {
            TypeDefinition::TypeId(type_id) => {
                Some(utils::token::ident_of_type_id(&self.type_engine.read(), type_id)?.span())
            }
            TypeDefinition::Ident(ident) => Some(ident.span()),
        })
    }

    /// Return a reference to the `TokenMap` of the current session.
    pub fn token_map(&self) -> &TokenMap {
        &self.token_map
    }

    /// Store the text document in the session.
    pub fn store_document(&self, text_document: TextDocument) -> Result<(), DocumentError> {
        let uri = text_document.get_uri().to_string();
        self.documents
            .insert(uri.clone(), text_document)
            .map_or(Ok(()), |_| {
                Err(DocumentError::DocumentAlreadyStored { path: uri })
            })
    }

    /// Remove the text document from the session.
    pub fn remove_document(&self, url: &Url) -> Result<TextDocument, DocumentError> {
        self.documents
            .remove(url.path())
            .ok_or_else(|| DocumentError::DocumentNotFound {
                path: url.path().to_string(),
            })
            .map(|(_, text_document)| text_document)
    }

    pub fn parse_project(&self, uri: &Url) -> Result<Vec<Diagnostic>, LanguageServerError> {
        self.token_map.clear();
        self.runnables.clear();

        let manifest_dir = PathBuf::from(uri.path());
        let locked = false;
        let offline = false;

        let manifest = ManifestFile::from_dir(&manifest_dir).map_err(|_| {
            DocumentError::ManifestFileNotFound {
                dir: uri.path().into(),
            }
        })?;

        let member_manifests =
            manifest
                .member_manifests()
                .map_err(|_| DocumentError::MemberManifestsFailed {
                    dir: uri.path().into(),
                })?;

        let lock_path =
            manifest
                .lock_path()
                .map_err(|_| DocumentError::ManifestsLockPathFailed {
                    dir: uri.path().into(),
                })?;

        let plan =
            pkg::BuildPlan::from_lock_and_manifests(&lock_path, &member_manifests, locked, offline)
                .map_err(LanguageServerError::BuildPlanFailed)?;

        let mut diagnostics = Vec::new();
        let type_engine = &*self.type_engine.read();
        let results =
            pkg::check(&plan, true, type_engine).map_err(LanguageServerError::FailedToCompile)?;
        let results_len = results.len();
        for (i, res) in results.into_iter().enumerate() {
            // We can convert these destructured elements to a Vec<Diagnostic> later on.
            let CompileResult {
                value,
                warnings,
                errors,
            } = res;

            // FIXME(Centril): Refactor parse_ast_to_tokens + parse_ast_to_typed_tokens
            // due to the new API.g
            let (parsed, typed) = match value {
                None => (None, None),
                Some((pp, tp)) => (Some(pp), tp),
            };

            let parsed_res = CompileResult::new(parsed, warnings.clone(), errors.clone());
            let ast_res = CompileResult::new(typed, warnings, errors);

            let parse_program = self.compile_res_to_parse_program(&parsed_res)?;
            let typed_program = self.compile_res_to_typed_program(&ast_res)?;

            // The final element in the results is the main program.
            if i == results_len - 1 {
                // First, populate our token_map with un-typed ast nodes.
                self.parse_ast_to_tokens(parse_program, |an, tm| {
                    traverse_parse_tree::traverse_node(type_engine, an, tm)
                });

                // Next, create runnables and populate our token_map with typed ast nodes.
                self.create_runnables(typed_program);
                self.parse_ast_to_typed_tokens(typed_program, |an, tm| {
                    traverse_typed_tree::traverse_node(type_engine, an, tm)
                });

                self.save_parse_program(parse_program.to_owned().clone());
                self.save_typed_program(typed_program.to_owned().clone());

                diagnostics =
                    capabilities::diagnostic::get_diagnostics(&ast_res.warnings, &ast_res.errors);
            } else {
                // Collect tokens from dependencies and the standard library prelude.
                self.parse_ast_to_tokens(parse_program, dependency::collect_parsed_declaration);

                self.parse_ast_to_typed_tokens(
                    typed_program,
                    dependency::collect_typed_declaration,
                );
            }
        }
        Ok(diagnostics)
    }

    /// Parse the `ParseProgram` AST to populate the token map with parsed AST nodes.
    fn parse_ast_to_tokens(&self, parse_program: &ParseProgram, f: impl Fn(&AstNode, &TokenMap)) {
        let root_nodes = parse_program.root.tree.root_nodes.iter();
        let sub_nodes = parse_program
            .root
            .submodules
            .iter()
            .flat_map(|(_, submodule)| &submodule.module.tree.root_nodes);

        root_nodes
            .chain(sub_nodes)
            .for_each(|node| f(node, &self.token_map));
    }

    /// Parse the `TyProgram` AST to populate the token map with typed AST nodes.
    pub fn parse_ast_to_typed_tokens(
        &self,
        typed_program: &ty::TyProgram,
        f: impl Fn(&ty::TyAstNode, &TokenMap),
    ) {
        let root_nodes = typed_program.root.all_nodes.iter();
        let sub_nodes = typed_program
            .root
            .submodules
            .iter()
            .flat_map(|(_, submodule)| &submodule.module.all_nodes);

        root_nodes
            .chain(sub_nodes)
            .for_each(|node| f(node, &self.token_map));
    }

    /// Get a reference to the `ParseProgram` AST.
    pub fn compile_res_to_parse_program<'a>(
        &'a self,
        parsed_result: &'a CompileResult<ParseProgram>,
    ) -> Result<&'a ParseProgram, LanguageServerError> {
        parsed_result.value.as_ref().ok_or_else(|| {
            let diagnostics = capabilities::diagnostic::get_diagnostics(
                &parsed_result.warnings,
                &parsed_result.errors,
            );
            LanguageServerError::FailedToParse { diagnostics }
        })
    }

    /// Get a reference to the `TyProgram` AST.
    pub fn compile_res_to_typed_program<'a>(
        &'a self,
        ast_res: &'a CompileResult<ty::TyProgram>,
    ) -> Result<&'a ty::TyProgram, LanguageServerError> {
        ast_res
            .value
            .as_ref()
            .ok_or(LanguageServerError::FailedToParse {
                diagnostics: capabilities::diagnostic::get_diagnostics(
                    &ast_res.warnings,
                    &ast_res.errors,
                ),
            })
    }

    /// Create runnables if the `TyProgramKind` of the `TyProgram` is a script.
    pub fn create_runnables(&self, typed_program: &ty::TyProgram) {
        if let ty::TyProgramKind::Script {
            ref main_function, ..
        } = typed_program.kind
        {
            let main_fn_location = utils::common::get_range_from_span(&main_function.name.span());
            let runnable = Runnable::new(main_fn_location, typed_program.kind.tree_type());
            self.runnables.insert(RunnableType::MainFn, runnable);
        }
    }

    /// Save the `ParseProgram` AST in the session.
    pub fn save_parse_program(&self, parse_program: ParseProgram) {
        let mut program = self.compiled_program.write();
        program.parsed = Some(parse_program);
    }

    /// Save the `TyProgram` AST in the session.
    pub fn save_typed_program(&self, typed_program: ty::TyProgram) {
        let mut program = self.compiled_program.write();
        program.typed = Some(typed_program);
    }

    pub fn contains_sway_file(&self, url: &Url) -> bool {
        self.documents.contains_key(url.path())
    }

    pub fn handle_open_file(&self, uri: &Url) {
        if !self.contains_sway_file(uri) {
            if let Ok(text_document) = TextDocument::build_from_path(uri.path()) {
                let _ = self.store_document(text_document);
            }
        }
    }

    pub fn update_text_document(
        &self,
        url: &Url,
        changes: Vec<TextDocumentContentChangeEvent>,
    ) -> Option<String> {
        self.documents.get_mut(url.path()).map(|mut document| {
            changes.iter().for_each(|change| {
                document.apply_change(change);
            });
            document.get_text()
        })
    }

    pub fn token_ranges(&self, url: &Url, position: Position) -> Option<Vec<Range>> {
        let (_, token) = self.token_at_position(url, position)?;
        let token_ranges = self
            .all_references_of_token(&token)
            .map(|(ident, _)| utils::common::get_range_from_span(&ident.span()))
            .collect();

        Some(token_ranges)
    }

    pub fn token_definition_response(
        &self,
        uri: Url,
        position: Position,
    ) -> Option<GotoDefinitionResponse> {
        self.token_at_position(&uri, position)
            .and_then(|(_, token)| self.declared_token_ident(&token))
            .and_then(|decl_ident| {
                let range = utils::common::get_range_from_span(&decl_ident.span());
                decl_ident.span().path().and_then(|path| {
                    // We use ok() here because we don't care about propagating the error from from_file_path
                    Url::from_file_path(path.as_ref()).ok().and_then(|url| {
                        self.sync
                            .to_workspace_url(url)
                            .map(|url| GotoDefinitionResponse::Scalar(Location::new(url, range)))
                    })
                })
            })
    }

    pub fn completion_items(&self) -> Option<Vec<CompletionItem>> {
        Some(capabilities::completion::to_completion_items(
            self.token_map(),
        ))
    }

    pub fn symbol_information(&self, url: &Url) -> Option<Vec<SymbolInformation>> {
        let tokens = self.tokens_for_file(url);
        self.sync
            .to_workspace_url(url.clone())
            .map(|url| capabilities::document_symbol::to_symbol_information(tokens, url))
    }

    pub fn format_text(&self, url: &Url) -> Result<Vec<TextEdit>, LanguageServerError> {
        let document =
            self.documents
                .get(url.path())
                .ok_or_else(|| DocumentError::DocumentNotFound {
                    path: url.path().to_string(),
                })?;

        get_page_text_edit(Arc::from(document.get_text()), &mut <_>::default())
            .map(|page_text_edit| vec![page_text_edit])
    }

    pub fn parse_and_store_sway_files(&self) -> Result<(), LanguageServerError> {
        let temp_dir = self.sync.temp_dir()?;
        // Store the documents.
        for path in get_sway_files(temp_dir).iter().filter_map(|fp| fp.to_str()) {
            self.store_document(TextDocument::build_from_path(path)?)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{get_absolute_path, get_url};

    #[test]
    fn store_document_returns_empty_tuple() {
        let session = Session::new();
        let path = get_absolute_path("sway-lsp/test/fixtures/cats.txt");
        let document = TextDocument::build_from_path(&path).unwrap();
        let result = Session::store_document(&session, document);
        assert!(result.is_ok());
    }

    #[test]
    fn store_document_returns_document_already_stored_error() {
        let session = Session::new();
        let path = get_absolute_path("sway-lsp/test/fixtures/cats.txt");
        let document = TextDocument::build_from_path(&path).unwrap();
        Session::store_document(&session, document).expect("expected successfully stored");
        let document = TextDocument::build_from_path(&path).unwrap();
        let result = Session::store_document(&session, document)
            .expect_err("expected DocumentAlreadyStored");
        assert_eq!(result, DocumentError::DocumentAlreadyStored { path });
    }

    #[test]
    fn parse_project_returns_manifest_file_not_found() {
        let session = Session::new();
        let dir = get_absolute_path("sway-lsp/test/fixtures");
        let uri = get_url(&dir);
        let result =
            Session::parse_project(&session, &uri).expect_err("expected ManifestFileNotFound");
        assert!(matches!(
            result,
            LanguageServerError::DocumentError(
                DocumentError::ManifestFileNotFound { dir: test_dir }
            )
            if test_dir == dir
        ));
    }
}
