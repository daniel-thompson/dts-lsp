use std::collections::HashMap;
use std::fs::metadata;
use std::fs::read_dir;
use std::fs::read_to_string;
use tower_lsp::jsonrpc::Error;
use tower_lsp::jsonrpc::Result;
#[allow(clippy::wildcard_imports)]
use tower_lsp::lsp_types::*;
use tower_lsp::Client;
use tower_lsp::{LanguageServer, LspService, Server};
use tree_sitter::Parser;
use tree_sitter::Point;
use tree_sitter::Query;
use tree_sitter::QueryCursor;
use tree_sitter::Tree;

mod file_depot;
mod includes_depot;
mod labels_depot;
mod logger;
mod references_depot;
mod utils;

#[cfg(test)]
mod tests;

use file_depot::FileDepot;
use includes_depot::IncludesDepot;
use labels_depot::LabelsDepot;
use logger::{log_message, Logger};
use utils::convert_range;

use references_depot::ReferencesDepot;
use utils::is_header;

struct Backend {
    data: Data,
    client: Option<Client>,
    process_neighbours: bool,
}

impl Backend {
    fn new(client: Client) -> Self {
        Backend {
            data: Data::new(),
            process_neighbours: true,
            client: Some(client),
        }
    }

    async fn get_includes_path(&self) -> String {
        let default = ".".to_string();

        let cfg_item = vec![ConfigurationItem {
            scope_uri: None,
            section: Some("dts-lsp".to_string()),
        }];

        let cfg = match self.client.clone() {
            None => return default,
            Some(x) => x.configuration(cfg_item).await,
        };

        info!("got cfg: {:?}", cfg);

        if let Ok(cfg) = cfg {
            let cfg = &cfg[0];
            let cfg = cfg.get("bindings_includes");
            if let Some(cfg) = cfg {
                return cfg.to_string();
            }
        }

        default
    }

    fn process_labels(&self, tree: &Tree, uri: &Url, text: &str) {
        let mut cursor = QueryCursor::new();

        let q = Query::new(
            &tree_sitter_devicetree::language(),
            "(node label: (identifier)@id)",
        )
        .unwrap();
        let matches = cursor.matches(&q, tree.root_node(), text.as_bytes());
        let mut labels = Vec::new();
        for m in matches {
            let nodes = m.nodes_for_capture_index(0);
            for node in nodes {
                let label = node.utf8_text(text.as_bytes()).unwrap();
                let range = node.range();
                labels.push((label, uri, range));
            }
        }

        for (label, uri, range) in labels {
            self.data.ld.add_label(label, uri, convert_range(&range));
        }
    }

    fn process_includes(&self, tree: &Tree, uri: &Url, text: &str) -> Vec<Url> {
        let mut cursor = QueryCursor::new();
        let q = Query::new(
            &tree_sitter_devicetree::language(),
            "[
            (dtsi_include path: (string_literal)@id)
            (preproc_include path: (string_literal)@id)
            (preproc_include path: (system_lib_string)@id)
            ]",
        )
        .unwrap();
        let matches = cursor.matches(&q, tree.root_node(), text.as_bytes());
        let mut v = Vec::new();
        let mut logs = Vec::new();
        for m in matches {
            let nodes = m.nodes_for_capture_index(0);
            for node in nodes {
                let label = node.utf8_text(text.as_bytes()).unwrap();
                let mut needs_fixup = false;
                if label.ends_with('>') {
                    needs_fixup = true;
                }
                let label = label.trim_matches('"');
                let label = label.trim_matches('<');
                let label = label.trim_matches('>');
                let range = node.range();
                let pos = range.start_point;
                let mut new_url = uri.join(label).unwrap();
                if needs_fixup {
                    new_url = self.data.fd.get_real_path(label).unwrap();
                }
                v.push(new_url.clone());
                self.data.fd.add_include(uri, &new_url);
                logs.push(format!(
                    "INCLUDE<{}>: {}, {}",
                    node.kind(),
                    new_url,
                    pos.row
                ));
            }
        }
        for msg in logs {
            info!("{}", &msg);
        }
        v
    }

    fn process_references(&self, tree: &Tree, uri: &Url, text: &str) {
        let mut cursor = QueryCursor::new();

        let q = Query::new(
            &tree_sitter_devicetree::language(),
            "(reference label: (identifier)@id)",
        )
        .unwrap();
        let matches = cursor.matches(&q, tree.root_node(), text.as_bytes());
        let mut references = Vec::new();
        for m in matches {
            let nodes = m.nodes_for_capture_index(0);
            for node in nodes {
                let label = node.utf8_text(text.as_bytes()).unwrap();
                let range = node.range();
                references.push((label, uri, range));
            }
        }

        for (label, uri, range) in references {
            info!("LABEL = {label}");
            self.data
                .rd
                .add_reference(label, uri, convert_range(&range));
        }
    }

    fn process_defines(&self, tree: &Tree, uri: &Url, text: &str) {
        let mut cursor = QueryCursor::new();

        let q = Query::new(
            &tree_sitter_devicetree::language(),
            "[
            (preproc_def name: (identifier)@name value: (preproc_arg)@id)
            (preproc_function_def name: (identifier)@name parameters: (preproc_params) value: (preproc_arg)@id)
            ]",
        )
        .unwrap();
        let matches = cursor.matches(&q, tree.root_node(), text.as_bytes());
        for m in matches {
            let nodes = m
                .nodes_for_capture_index(0)
                .zip(m.nodes_for_capture_index(1));
            for (name, value) in nodes {
                let def_name = name.utf8_text(text.as_bytes()).unwrap();
                let value = value.utf8_text(text.as_bytes()).unwrap();
                let value = value.trim_end().trim_start();
                self.data
                    .id
                    .add_define(def_name, uri, convert_range(&name.range()), value);
                info!("KEK define = {def_name} -> {value}");
            }
        }
    }

    fn handle_file(&self, uri: &Url, text: Option<String>) -> Vec<Url> {
        if !utils::extension_one_of(uri, &["dts", "dtsi", "h"]) {
            return Vec::new();
        }

        let Ok(path) = uri.to_file_path() else {
            error!("Invalid url {}", uri);
            return Vec::new();
        };

        let text = match text.map_or(read_to_string(path), Ok) {
            Ok(x) => x,
            Err(e) => {
                warn!("can't read file {}: {}", uri, e.kind());
                return Vec::new();
            }
        };

        match self.data.fd.insert(uri, &text) {
            file_depot::InsertResult::Exists => return Vec::new(),
            file_depot::InsertResult::Modified => {
                self.data.ld.invalidate(uri);
                self.data.rd.invalidate(uri);
            }
            file_depot::InsertResult::Ok => (),
        };

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_devicetree::language())
            .unwrap();
        let tree = parser.parse(&text, None).unwrap();

        self.process_defines(&tree, uri, &text);
        self.data.id.dump();
        if is_header(uri) {
            return Vec::new();
        }

        self.process_labels(&tree, uri, &text);
        self.process_references(&tree, uri, &text);
        self.process_includes(&tree, uri, &text)
    }

    fn open_neighbours(&self, uri: &Url) {
        let d = uri.join(".").unwrap();
        let Ok(path) = d.to_file_path() else {
            error!("Invalid url {}", d);
            return;
        };

        // Skip if client has opened a buffer for a file that has some
        // directories in its path that have not been created yet.
        let Ok(files) = read_dir(path) else {
            return;
        };

        for f in files {
            let p = f.unwrap().path();
            if !metadata(&p).unwrap().is_file() {
                continue;
            }
            let u = Url::from_file_path(p).unwrap();
            if self.data.fd.exist(&u) {
                continue;
            }
            self.handle_file(&u, None);
        }
    }
}

struct Data {
    fd: FileDepot,
    ld: LabelsDepot,
    rd: ReferencesDepot,
    id: IncludesDepot,
}

impl Data {
    fn new() -> Data {
        let fd = FileDepot::new();
        Data {
            ld: LabelsDepot::new(&fd),
            rd: ReferencesDepot::new(&fd),
            id: IncludesDepot::new(&fd),
            fd,
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: None,
                    },
                })),
                ..ServerCapabilities::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let x = self.get_includes_path().await;
        info!("include_path: {x}");

        info!("server initialized!");
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = &params.text_document.uri;

        info!("Open file: {uri}");

        let text = params.text_document.text.as_str();
        let mut includes = self.handle_file(uri, Some(text.to_string()));

        while let Some(new_url) = includes.pop() {
            let mut tmp = self.handle_file(&new_url, None);
            includes.append(&mut tmp);
        }

        self.data.fd.dump();
        self.data.ld.dump();
        self.data.rd.dump();
        self.data.id.dump();

        if self.process_neighbours {
            self.open_neighbours(uri);
        }
    }

    async fn goto_definition(
        &self,
        input: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let location = input.text_document_position_params.position;
        let location = Point::new(location.line as usize, location.character as usize);
        let uri = input.text_document_position_params.text_document.uri;
        let Some(text) = self.data.fd.get_text(&uri) else {
            return Ok(None);
        };
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_devicetree::language())
            .unwrap();
        let tree = parser.parse(&text, None).unwrap();
        if let Some(node) = tree
            .root_node()
            .named_descendant_for_point_range(location, location)
        {
            let label = node.utf8_text(text.as_bytes()).unwrap();

            let parent_kind = node.parent().map(|x| x.kind());
            let node_kind = node.kind();

            return match (node_kind, parent_kind) {
                ("identifier", Some("reference")) => {
                    let labels = self.data.ld.find_label(&uri, label);
                    let res: Vec<Location> = labels
                        .clone()
                        .into_iter()
                        .map(|x| Location::new(x.uri, x.range))
                        .collect();

                    match res.len() {
                        0 => Ok(None),
                        1 => Ok(Some(GotoDefinitionResponse::Scalar(res[0].clone()))),
                        _ => Ok(Some(GotoDefinitionResponse::Array(res))),
                    }
                }
                ("identifier", _) => match self.data.id.find_define(&uri, label) {
                    None => Ok(None),
                    Some(x) => {
                        let res = Location::new(x.uri, x.range);
                        Ok(Some(GotoDefinitionResponse::Scalar(res)))
                    }
                },
                _ => Ok(None),
            };
        }

        Ok(None)
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let location = params.text_document_position.position;
        let location = Point::new(location.line as usize, location.character as usize);
        let uri = params.text_document_position.text_document.uri;

        let Some(text) = self.data.fd.get_text(&uri) else {
            warn!("No text found for file {uri}");
            return Ok(None);
        };

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_devicetree::language())
            .unwrap();
        let tree = parser.parse(&text, None).unwrap();
        if let Some(node) = tree
            .root_node()
            .named_descendant_for_point_range(location, location)
        {
            let label = node.utf8_text(text.as_bytes()).unwrap();

            if let (Some(parent), v) = (node.parent(), self.data.rd.find_references(&uri, label)) {
                if parent.kind() == "node" {
                    let mut res = Vec::new();
                    for x in v {
                        res.push(Location::new(x.uri, x.range));
                    }
                    return Ok(Some(res));
                }
            }
        }
        Ok(None)
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let location = params.position;
        let location = Point::new(location.line as usize, location.character as usize);
        let uri = params.text_document.uri;
        let Some(text) = self.data.fd.get_text(&uri) else {
            warn!("No text found for file {uri}");
            return Ok(None);
        };
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_devicetree::language())
            .unwrap();
        let tree = parser.parse(&text, None).unwrap();

        if let Some(node) = tree
            .root_node()
            .named_descendant_for_point_range(location, location)
        {
            let name = node.utf8_text(text.as_bytes()).unwrap();
            let range = node.range();

            let labels = self.data.ld.find_label(&uri, name);
            let references = self.data.rd.find_references(&uri, name);

            if labels.len() + references.len() > 0 {
                return Ok(Some(PrepareRenameResponse::Range(convert_range(&range))));
            }
        }

        Err(Error::new(tower_lsp::jsonrpc::ErrorCode::InvalidParams))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let location = params.text_document_position.position;
        let location = Point::new(location.line as usize, location.character as usize);
        let uri = params.text_document_position.text_document.uri;
        let Some(text) = self.data.fd.get_text(&uri) else {
            warn!("No text found for file {uri}");
            return Ok(None);
        };

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_devicetree::language())
            .unwrap();
        let tree = parser.parse(&text, None).unwrap();
        if let Some(node) = tree
            .root_node()
            .named_descendant_for_point_range(location, location)
        {
            let name = node.utf8_text(text.as_bytes()).unwrap();
            let mut result: HashMap<Url, Vec<TextEdit>> = HashMap::new();

            let labels = self.data.ld.find_label(&uri, name);
            let references = self.data.rd.find_references(&uri, name);

            for label in &labels {
                self.data.ld.rename(&label.uri, name, &params.new_name);
            }

            for reference in &references {
                self.data.rd.rename(&reference.uri, name, &params.new_name);
            }

            // TODO: check that labels in single file are ordered from bottom to top
            for symbol in labels.iter().chain(references.iter()) {
                let e = result.entry(symbol.uri.clone()).or_default();
                e.push(TextEdit::new(symbol.range, params.new_name.clone()));
            }

            for (uri, edits) in &result {
                self.data.fd.apply_edits(uri, edits);
            }

            if !result.is_empty() {
                return Ok(Some(WorkspaceEdit {
                    changes: Some(result),
                    document_changes: None,
                    change_annotations: None,
                }));
            }
        }

        Err(Error::new(tower_lsp::jsonrpc::ErrorCode::InvalidParams))
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        info!("Close file: {}", params.text_document.uri);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = &params.text_document.uri;

        info!("Change file: {uri}");

        let text = &params.content_changes[0].text;
        let mut includes = self.handle_file(uri, Some(text.to_string()));

        while let Some(new_url) = includes.pop() {
            let mut tmp = self.handle_file(&new_url, None);
            includes.append(&mut tmp);
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        info!("Save file: {}", params.text_document.uri);
    }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| {
        let handle = tokio::runtime::Handle::current();
        Logger::set(Logger::Lsp(handle, client.clone()));
        Backend::new(client)
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
