//! GtkSourceView completion backed by the [`LspClient`]. A provider is added to
//! each cell's editor; when GtkSourceView asks it to populate, we sync the
//! document to `sqls`, request `textDocument/completion` at the cursor, and
//! return the items as proposals.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gio, glib};
use sourceview5::subclass::prelude::*;

use crate::lsp::LspClient;
use crate::runtime;

// ---------------------------------------------------------------------------
// Proposal — one completion item
// ---------------------------------------------------------------------------

mod proposal_imp {
    use super::*;

    #[derive(Default)]
    pub struct LspProposal {
        pub label: RefCell<String>,
        pub insert: RefCell<String>,
        pub detail: RefCell<String>,
        pub icon: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LspProposal {
        const NAME: &'static str = "SqweelLspProposal";
        type Type = super::LspProposal;
        type Interfaces = (sourceview5::CompletionProposal,);
    }

    impl ObjectImpl for LspProposal {}
    impl CompletionProposalImpl for LspProposal {}
}

glib::wrapper! {
    pub struct LspProposal(ObjectSubclass<proposal_imp::LspProposal>)
        @implements sourceview5::CompletionProposal;
}

impl LspProposal {
    fn new(label: &str, insert: &str, detail: &str, icon: &str) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        imp.label.replace(label.to_string());
        imp.insert.replace(insert.to_string());
        imp.detail.replace(detail.to_string());
        imp.icon.replace(icon.to_string());
        obj
    }

    fn label(&self) -> String {
        self.imp().label.borrow().clone()
    }
    fn insert(&self) -> String {
        self.imp().insert.borrow().clone()
    }
    fn detail(&self) -> String {
        self.imp().detail.borrow().clone()
    }
    fn icon(&self) -> String {
        self.imp().icon.borrow().clone()
    }
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

mod provider_imp {
    use super::*;

    #[derive(Default)]
    pub struct LspCompletionProvider {
        pub client: RefCell<Option<LspClient>>,
        pub uri: RefCell<String>,
        pub buffer: RefCell<Option<sourceview5::Buffer>>,
        pub version: RefCell<Option<Arc<AtomicI64>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LspCompletionProvider {
        const NAME: &'static str = "SqweelLspCompletionProvider";
        type Type = super::LspCompletionProvider;
        type Interfaces = (sourceview5::CompletionProvider,);
    }

    impl ObjectImpl for LspCompletionProvider {}

    impl CompletionProviderImpl for LspCompletionProvider {
        fn title(&self) -> Option<glib::GString> {
            Some("sqls".into())
        }

        fn priority(&self, _context: &sourceview5::CompletionContext) -> i32 {
            0
        }

        // Pop up as the user types identifiers or after a dot.
        fn is_trigger(&self, _iter: &gtk::TextIter, c: char) -> bool {
            c.is_alphabetic() || c == '_' || c == '.'
        }

        fn display(
            &self,
            _context: &sourceview5::CompletionContext,
            proposal: &sourceview5::CompletionProposal,
            cell: &sourceview5::CompletionCell,
        ) {
            let Some(p) = proposal.downcast_ref::<super::LspProposal>() else { return };
            // `display` is called once per column; fill each appropriately
            // (otherwise the label renders duplicated across every column).
            use sourceview5::CompletionColumn;
            match cell.column() {
                CompletionColumn::Icon => {
                    cell.set_icon_name(&p.icon());
                    cell.set_margin_end(8);
                }
                CompletionColumn::TypedText => {
                    cell.set_text(Some(&p.label()));
                    cell.set_margin_end(16);
                }
                CompletionColumn::After => {
                    let detail = p.detail();
                    cell.set_text((!detail.is_empty()).then_some(detail.as_str()));
                    cell.add_css_class("dim-label");
                }
                _ => cell.set_text(None),
            }
        }

        fn activate(
            &self,
            context: &sourceview5::CompletionContext,
            proposal: &sourceview5::CompletionProposal,
        ) {
            let Some(p) = proposal.downcast_ref::<super::LspProposal>() else { return };
            let text = p.insert();
            if let Some((mut start, mut end)) = context.bounds() {
                let buffer = start.buffer();
                buffer.delete(&mut start, &mut end);
                buffer.insert(&mut start, &text);
            }
        }

        fn populate_future(
            &self,
            _context: &sourceview5::CompletionContext,
        ) -> Pin<Box<dyn Future<Output = Result<gio::ListModel, glib::Error>>>> {
            let client = self.client.borrow().clone();
            let uri = self.uri.borrow().clone();
            let buffer = self.buffer.borrow().clone();
            let version = self.version.borrow().clone();

            Box::pin(async move {
                let empty =
                    || Ok(gio::ListStore::new::<super::LspProposal>().upcast::<gio::ListModel>());

                let (Some(client), Some(buffer), Some(version)) = (client, buffer, version) else {
                    return empty();
                };

                // Cursor position + full document text (read on the main thread).
                let mark = buffer.get_insert();
                let iter = buffer.iter_at_mark(&mark);
                let line = iter.line() as u32;
                let character = iter.line_offset() as u32;
                let (s, e) = buffer.bounds();
                let text = buffer.text(&s, &e, false).to_string();

                // Sync the document so completion reflects what's on screen.
                let v = version.fetch_add(1, Ordering::SeqCst) + 1;
                client.did_change(&uri, v, &text);

                // Run the LSP request on the tokio runtime.
                let rx = runtime::spawn(async move {
                    client.completion(&uri, line, character).await
                });
                let items = match rx.recv().await {
                    Ok(Ok(items)) => items,
                    _ => return empty(),
                };

                let store = gio::ListStore::new::<super::LspProposal>();
                for item in items {
                    let insert = item
                        .insert_text
                        .clone()
                        .unwrap_or_else(|| item.label.clone());
                    let detail = item
                        .detail
                        .clone()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| kind_label(item.kind).to_string());
                    store.append(&super::LspProposal::new(
                        &item.label,
                        &insert,
                        &detail,
                        kind_icon(item.kind),
                    ));
                }
                Ok(store.upcast::<gio::ListModel>())
            })
        }
    }
}

glib::wrapper! {
    pub struct LspCompletionProvider(ObjectSubclass<provider_imp::LspCompletionProvider>)
        @implements sourceview5::CompletionProvider;
}

impl LspCompletionProvider {
    pub fn new(
        client: LspClient,
        uri: &str,
        buffer: &sourceview5::Buffer,
        version: Arc<AtomicI64>,
    ) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        imp.client.replace(Some(client));
        imp.uri.replace(uri.to_string());
        imp.buffer.replace(Some(buffer.clone()));
        imp.version.replace(Some(version));
        obj
    }
}

// ---------------------------------------------------------------------------
// Hover provider
// ---------------------------------------------------------------------------

mod hover_imp {
    use super::*;

    #[derive(Default)]
    pub struct LspHoverProvider {
        pub client: RefCell<Option<LspClient>>,
        pub uri: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LspHoverProvider {
        const NAME: &'static str = "SqweelLspHoverProvider";
        type Type = super::LspHoverProvider;
        type Interfaces = (sourceview5::HoverProvider,);
    }

    impl ObjectImpl for LspHoverProvider {}

    impl HoverProviderImpl for LspHoverProvider {
        fn populate_future(
            &self,
            context: &sourceview5::HoverContext,
            display: &sourceview5::HoverDisplay,
        ) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>>>> {
            let client = self.client.borrow().clone();
            let uri = self.uri.borrow().clone();
            let display = display.clone();
            // Position of the hovered word.
            let pos = context.bounds().map(|(start, _)| {
                (start.line() as u32, start.line_offset() as u32)
            });

            Box::pin(async move {
                let none = || Err(glib::Error::new(glib::FileError::Noent, "no hover"));
                let (Some(client), Some((line, character))) = (client, pos) else {
                    return none();
                };
                let rx =
                    runtime::spawn(async move { client.hover(&uri, line, character).await });
                match rx.recv().await {
                    Ok(Some(text)) => {
                        let label = gtk::Label::builder()
                            .label(text)
                            .selectable(true)
                            .wrap(true)
                            .xalign(0.0)
                            .margin_top(6)
                            .margin_bottom(6)
                            .margin_start(8)
                            .margin_end(8)
                            .build();
                        label.add_css_class("monospace");
                        display.append(&label);
                        Ok(())
                    }
                    _ => none(),
                }
            })
        }
    }
}

glib::wrapper! {
    pub struct LspHoverProvider(ObjectSubclass<hover_imp::LspHoverProvider>)
        @implements sourceview5::HoverProvider;
}

impl LspHoverProvider {
    pub fn new(client: LspClient, uri: &str) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().client.replace(Some(client));
        obj.imp().uri.replace(uri.to_string());
        obj
    }
}

/// A symbolic icon name for a completion item kind (tables, columns, keywords…).
fn kind_icon(kind: Option<lsp_types::CompletionItemKind>) -> &'static str {
    use lsp_types::CompletionItemKind as K;
    match kind {
        Some(K::FIELD | K::PROPERTY | K::VARIABLE | K::VALUE) => "view-list-symbolic",
        Some(K::CLASS | K::STRUCT | K::INTERFACE | K::MODULE | K::ENUM) => "view-grid-symbolic",
        Some(K::FUNCTION | K::METHOD) => "system-run-symbolic",
        Some(K::KEYWORD | K::OPERATOR) => "format-text-rich-symbolic",
        _ => "text-x-generic-symbolic",
    }
}

/// A short human label for an item kind, shown when the server gives no detail.
fn kind_label(kind: Option<lsp_types::CompletionItemKind>) -> &'static str {
    use lsp_types::CompletionItemKind as K;
    match kind {
        Some(K::FIELD | K::PROPERTY) => "column",
        Some(K::CLASS | K::STRUCT | K::INTERFACE) => "table",
        Some(K::FUNCTION | K::METHOD) => "function",
        Some(K::KEYWORD) => "keyword",
        Some(K::MODULE) => "schema",
        _ => "",
    }
}
