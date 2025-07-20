use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use itertools::Itertools;
use tokio::sync::Mutex;
use tower_lsp_server::jsonrpc::{Error, Result};
use tower_lsp_server::{Client, LanguageServer, LspService, Server};
use tower_lsp_server::{UriExt, lsp_types::*};
use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIteratorMut, Tree, TreeCursor};

struct Backend {
    client: Client,
    documents: Arc<tokio::sync::Mutex<HashMap<PathBuf, Document>>>,
}

struct Document {
    text: String,
    tree: Tree,
}

impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: Some(false),
                    },
                })),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "qbels".to_string(),
                version: None,
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "server initialized!")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;

        if let Err(e) = self.update_document(&uri, text).await {
            self.client
                .log_message(MessageType::ERROR, format!("Error opening document: {}", e))
                .await
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params
            .content_changes
            .into_iter()
            .next()
            .map(|change| change.text)
            .unwrap_or_default();

        if let Err(e) = self.update_document(&uri, text).await {
            self.client
                .log_message(
                    MessageType::ERROR,
                    format!("Error changing document: {}", e),
                )
                .await
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) -> () {
        let uri = params.text_document.uri;
        if let Some(path) = uri.to_file_path() {
            self.documents.lock().await.remove(&path.to_path_buf());
        }
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let position = params.position;

        let path = params
            .text_document
            .uri
            .to_file_path()
            .unwrap()
            .to_path_buf();

        let documents = self.documents.lock().await;
        let Some(doc) = documents.get(&path) else {
            return Err(Error::internal_error());
        };

        let root_node = doc.tree.root_node();
        let (row, col) = (position.line as usize, position.character as usize);

        let cursor_node = root_node
            .named_descendant_for_point_range(
                tree_sitter::Point::new(row, col),
                tree_sitter::Point::new(row, col + 1),
            )
            .unwrap();

        if let Some((rename_node, _)) = self.find_ident_node(cursor_node) {
            let start = rename_node.start_position();
            let end = rename_node.end_position();

            Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
                range: Range {
                    start: Position {
                        line: start.row as u32,
                        character: start.column as u32,
                    },
                    end: Position {
                        line: end.row as u32,
                        character: end.column as u32,
                    },
                },
                placeholder: rename_node
                    .utf8_text(doc.text.as_bytes())
                    .unwrap()
                    .to_string(),
            }))
        } else {
            Ok(None)
        }
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let position = params.text_document_position.position;

        let path = params
            .text_document_position
            .text_document
            .uri
            .to_file_path()
            .unwrap()
            .to_path_buf();

        let documents = self.documents.lock().await;

        let Some(doc) = documents.get(&path) else {
            return Err(Error::internal_error());
        };

        let root_node = doc.tree.root_node();
        let (row, col) = (position.line as usize, position.character as usize);

        let cursor_node = root_node
            .named_descendant_for_point_range(
                tree_sitter::Point::new(row, col),
                tree_sitter::Point::new(row, col + 1),
            )
            .unwrap();

        if let Some((node, kind)) = self.find_ident_node(cursor_node) {
            let rename_from = match kind {
                IdentKind::Local | IdentKind::Label => {
                    let Some(func_def) = Self::find_funcdef_from_child_node(node) else {
                        return Err(Error::invalid_params(
                            "Can't rename local or label outside funcdef",
                        ));
                    };

                    func_def
                }
                IdentKind::Global | IdentKind::Aggregete => root_node,
            };

            let mut cursor = doc.tree.walk();
            let old_name = node.utf8_text(doc.text.as_bytes()).unwrap();
            self.client.log_message(MessageType::ERROR, old_name).await;
            let nodes_to_rename =
                Self::rename_ident_from_node(old_name, &rename_from, kind, &mut cursor, &doc.text);

            let edits = nodes_to_rename
                .iter()
                .map(|node| {
                    let start = node.start_position();
                    let end = node.end_position();

                    TextEdit {
                        range: Range {
                            start: Position {
                                line: start.row as u32,
                                character: start.column as u32,
                            },
                            end: Position {
                                line: end.row as u32,
                                character: end.column as u32,
                            },
                        },
                        new_text: params.new_name.clone(),
                    }
                })
                .collect::<Vec<_>>();

            Ok(Some(WorkspaceEdit {
                changes: Some(HashMap::from([(
                    params.text_document_position.text_document.uri,
                    edits,
                )])),
                ..Default::default()
            }))
        } else {
            Err(Error::internal_error())
        }
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let position = params.text_document_position.position;

        let path = params
            .text_document_position
            .text_document
            .uri
            .to_file_path()
            .unwrap()
            .to_path_buf();

        let documents = self.documents.lock().await;
        let Some(doc) = documents.get(&path) else {
            return Err(Error::internal_error());
        };

        let root_node = doc.tree.root_node();
        let (row, col) = (position.line as usize, position.character as usize);

        let cursor_node = root_node
            .named_descendant_for_point_range(
                tree_sitter::Point::new(row, col),
                tree_sitter::Point::new(row, col + 1),
            )
            .unwrap();

        if let Some((ident, kind)) = self.find_ident_node(cursor_node) {
            let ident_name = ident.utf8_text(doc.text.as_bytes()).unwrap();

            let query_text = format!(
                "({} name: (IDENT) @name (#eq? @name \"{}\")) @global",
                kind.kind(),
                ident_name
            );

            let mut cursor = QueryCursor::new();
            let query = Query::new(&tree_sitter_qbe::LANGUAGE.into(), &query_text).unwrap();

            match kind {
                IdentKind::Local | IdentKind::Label => {
                    let Some(funcdef) = Self::find_funcdef_from_child_node(ident) else {
                        return Ok(None);
                    };

                    cursor.set_byte_range(funcdef.range().start_byte..funcdef.range().end_byte);
                }
                _ => {}
            }

            let mut captures = cursor.captures(&query, root_node, doc.text.as_bytes());

            let mut nodes = vec![];
            while let Some(next) = captures.next_mut() {
                for cap in next.0.captures {
                    if cap.node.kind() == kind.kind() {
                        nodes.push(cap.node);
                    }
                }
            }

            let nodes: Vec<_> = nodes
                .iter()
                .map(|last| Location {
                    uri: params.text_document_position.text_document.uri.clone(),
                    range: Range {
                        start: Position {
                            line: last.start_position().row as u32,
                            character: last.start_position().column as u32,
                        },
                        end: Position {
                            line: last.end_position().row as u32,
                            character: last.end_position().column as u32,
                        },
                    },
                })
                .unique()
                .collect();

            return Ok(match nodes.len() {
                0 => None,
                _ => Some(nodes),
            });
        }

        Ok(None)
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let position = params.text_document_position_params.position;

        let path = params
            .text_document_position_params
            .text_document
            .uri
            .to_file_path()
            .unwrap()
            .to_path_buf();

        let documents = self.documents.lock().await;
        let Some(doc) = documents.get(&path) else {
            return Err(Error::internal_error());
        };

        let root_node = doc.tree.root_node();
        let (row, col) = (position.line as usize, position.character as usize);

        let cursor_node = root_node
            .named_descendant_for_point_range(
                tree_sitter::Point::new(row, col),
                tree_sitter::Point::new(row, col + 1),
            )
            .unwrap();

        if let Some((ident, kind)) = self.find_ident_node(cursor_node) {
            let ident_name = ident.utf8_text(doc.text.as_bytes()).unwrap();

            let query_text = match kind {
                IdentKind::Local => format!(
                    r#"
                        (INST assignment: (LOCAL name: (IDENT) @name (#eq? @name "{0}")) @local)
                        (FUNCDEF params: (FUNCDEF_PARAMS (FUNCDEF_PARAM name: (LOCAL name: (IDENT) @name (#eq? @name "{0}")) @local)))
                    "#,
                    ident_name
                ),
                IdentKind::Label => format!(
                    "(BLOCK label: (LABEL name: (IDENT) @name (#eq? @name \"{}\")) @label)",
                    ident_name
                ),

                IdentKind::Global => format!(
                    r#"
                        (FUNCDEF name: (GLOBAL name: (IDENT) @name (#eq? @name "{0}")) @global)
                        (DATADEF name: (GLOBAL name: (IDENT) @name (#eq? @name "{0}")) @global)
                    "#,
                    ident_name
                ),
                IdentKind::Aggregete => format!(
                    "(TYPEDEF (AGGREGATE name: (IDENT) @name (#eq? @name \"{}\")) @aggregate)",
                    ident_name
                ),
            };

            let mut cursor = QueryCursor::new();
            let query = Query::new(&tree_sitter_qbe::LANGUAGE.into(), &query_text).unwrap();

            match kind {
                IdentKind::Local | IdentKind::Label => {
                    let Some(funcdef) = Self::find_funcdef_from_child_node(ident) else {
                        return Ok(None);
                    };

                    cursor.set_byte_range(funcdef.range().start_byte..ident.range().end_byte);
                }
                _ => {}
            }

            let mut captures = cursor.captures(&query, root_node, doc.text.as_bytes());

            let mut nodes = vec![];
            while let Some(next) = captures.next_mut() {
                for cap in next.0.captures {
                    if cap.node.kind() == kind.kind() {
                        nodes.push(cap.node);
                    }
                }
            }

            let nodes: Vec<_> = nodes
                .iter()
                .map(|last| Location {
                    uri: params
                        .text_document_position_params
                        .text_document
                        .uri
                        .clone(),
                    range: Range {
                        start: Position {
                            line: last.start_position().row as u32,
                            character: last.start_position().column as u32,
                        },
                        end: Position {
                            line: last.end_position().row as u32,
                            character: last.end_position().column as u32,
                        },
                    },
                })
                .unique()
                .collect();

            return Ok(match nodes.len() {
                0 => None,
                1 => Some(GotoDefinitionResponse::Scalar(nodes[0].clone())),
                _ => Some(GotoDefinitionResponse::Array(nodes)),
            });
        }

        Ok(None)
    }
}

impl Backend {
    fn find_ident_node<'a>(&self, node: Node<'a>) -> Option<(Node<'a>, IdentKind)> {
        if let Some(kind) = self.is_renamable(&node) {
            return Some((node.named_child(0)?, kind));
        } else if node.kind() == "IDENT" {
            if let Some(parent) = node.parent() {
                if let Some(kind) = self.is_renamable(&parent) {
                    return Some((node, kind));
                }
            }
        }

        None
    }

    fn is_renamable<'a>(&self, node: &Node<'a>) -> Option<IdentKind> {
        Some(match node.kind() {
            "LOCAL" => IdentKind::Local,
            "GLOBAL" => IdentKind::Global,
            "AGGREGATE" => IdentKind::Aggregete,
            "LABEL" => IdentKind::Label,
            _ => return None,
        })
    }

    fn parse(&self, text: &str) -> tree_sitter::Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_qbe::LANGUAGE.into())
            .expect("Error loading qbe grammar");

        parser.parse(&text, None).unwrap()
    }

    fn find_funcdef_from_child_node<'a>(child: Node<'a>) -> Option<Node<'a>> {
        if child.kind() == "FUNCDEF" {
            return Some(child);
        }

        Self::find_funcdef_from_child_node(child.parent()?)
    }

    fn rename_ident_from_node<'a>(
        old_name: &str,
        node: &Node<'a>,
        kind: IdentKind,
        cursor: &mut TreeCursor<'a>,
        src: &str,
    ) -> Vec<Node<'a>> {
        let mut children = vec![];

        for child in node.children(cursor).collect::<Vec<Node<'a>>>() {
            if child.kind() == kind.kind() {
                let ident = child.named_child(0).unwrap();
                if ident.utf8_text(src.as_bytes()).unwrap() == old_name {
                    children.push(ident);
                }
            } else {
                children.extend(Self::rename_ident_from_node(
                    old_name, &child, kind, cursor, src,
                ));
            }
        }

        children
    }

    async fn update_document(&self, uri: &Uri, text: String) -> Result<()> {
        let path = uri.to_file_path().unwrap().to_path_buf();
        let tree = self.parse(&text);

        let mut documents = self.documents.lock().await;
        documents.insert(path, Document { text, tree });

        Ok(())
    }
}

#[derive(Clone, Copy)]
enum IdentKind {
    Local,
    Global,
    Aggregete,
    Label,
}

impl IdentKind {
    pub fn kind(&self) -> &'static str {
        match self {
            IdentKind::Local => "LOCAL",
            IdentKind::Global => "GLOBAL",
            IdentKind::Aggregete => "AGGREGATE",
            IdentKind::Label => "LABEL",
        }
    }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| Backend {
        client,
        documents: Arc::new(Mutex::new(HashMap::new())),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
