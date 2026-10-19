//! AI tools through publik API or the person's own key, desktop only. The setup flow, the
//! credential file and the HTTP calls live in the shared `craft-publik` crate (one per Craft app
//! family). This module is PdfCraft's thin host: the setup dialog on first launch,
//! Preferences ▸ AI, and the two tools of All tools ▸ AI assistant, Summarize and Ask about this
//! document.
//!
//! Nothing is sent, and nothing is charged, until the person clicks Summarize or Ask. A request
//! carries text only: the reading-order text that Export ▸ Text extracts, at most [`MAX_CHARS`]
//! characters of it. A document that does not allow copying its text sends nothing. Closing the
//! dialog before the request leaves stops it.

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};

pub use craft_publik::AppInfo;
use craft_publik::Provider;
use craft_publik::client::{self, Fail, Reply};
use craft_publik::ui::{Action, AiState, spawn};
use pdfcraft_engine::export::{ExportSource, Exporter};

use crate::theme::{self, Tokens};
use crate::{Dialog, PdfCraftApp, widgets};

const SUMMARY_SYSTEM: &str = "You summarize PDF documents for PdfCraft. You get text that was extracted from some pages of a PDF, so tables and columns can be out of order. First say what the document is. Then give its main points, and any dates, amounts, names and actions it asks for. Use at most eight short lines. Use plain text without markdown. Write in the language of the document.";
const ASK_SYSTEM: &str = "You answer questions about a PDF document for PdfCraft. You get text that was extracted from some pages of a PDF, so tables and columns can be out of order. Answer only from that text, and say on which page you found the answer. If the text does not contain the answer, say so. Be short. Use plain text without markdown.";

/// The most document text one request carries, in characters.
pub const MAX_CHARS: usize = 40_000;
/// The longest question kept.
const MAX_QUESTION_CHARS: usize = 2_000;
/// The `Fail::kind` of a request that the person stopped before it left.
const STOPPED: &str = "stopped";

pub struct Ai {
    pub state: AiState,
    frames: u32,
    first_run_checked: bool,
    /// Preferences (or a tool dialog) asked for the setup flow; `frame` opens it when it can.
    open_setup: bool,
    /// The dialog the setup flow goes back to when it closes: Preferences, or the tool that
    /// needed it. `None` closes it.
    setup_return: Option<Dialog>,
    scope: Scope,
    question: String,
    summary: Job,
    ask: Job,
}

impl Ai {
    pub fn new(app: AppInfo) -> Self {
        Ai {
            state: AiState::new(app),
            frames: 0,
            first_run_checked: false,
            open_setup: false,
            setup_return: None,
            scope: Scope::default(),
            question: String::new(),
            summary: Job::default(),
            ask: Job::default(),
        }
    }

    /// Do not open the setup dialog on this launch (an agent drives the app over `--control`).
    /// Preferences ▸ AI and the tools still offer it.
    pub fn skip_first_run(&mut self) {
        self.first_run_checked = true;
    }
}

/// `PUBLIK_SELF_TEST=1`: provision for real and print one line (publikhq.com/skills/setup.md).
pub fn self_test(app: &AppInfo) -> String {
    craft_publik::self_test(app)
}

/// Which pages a request reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Scope {
    /// From the first page on, until [`MAX_CHARS`].
    #[default]
    Document,
    /// The current page, the page before it and the page after it.
    Nearby,
}

impl Scope {
    /// The 0-based pages to read, in order, in a document of `count` pages that shows `current`.
    fn pages(self, current: usize, count: usize) -> Range<usize> {
        match self {
            Scope::Document => 0..count,
            Scope::Nearby => {
                let current = current.min(count.saturating_sub(1));
                current.saturating_sub(1)..current.saturating_add(2).min(count)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tool {
    Summary,
    Ask,
}

impl Tool {
    fn title(self) -> &'static str {
        match self {
            Tool::Summary => "Summarize",
            Tool::Ask => "Ask about this document",
        }
    }

    fn dialog(self) -> Dialog {
        match self {
            Tool::Summary => Dialog::AiSummary,
            Tool::Ask => Dialog::AiAsk,
        }
    }

    fn system(self) -> &'static str {
        match self {
            Tool::Summary => SUMMARY_SYSTEM,
            Tool::Ask => ASK_SYSTEM,
        }
    }

    fn button(self) -> &'static str {
        match self {
            Tool::Summary => "Summarize",
            Tool::Ask => "Ask",
        }
    }

    fn working(self) -> &'static str {
        match self {
            Tool::Summary => "Reading the pages and writing the summary…",
            Tool::Ask => "Reading the pages and writing the answer…",
        }
    }
}

/// What one request carried. The dialog shows it under the answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Sent {
    /// The document's name. It is not sent; it shows which document an answer is about.
    name: String,
    /// The first and the last 1-based page whose text was sent.
    pages: Option<(usize, usize)>,
    /// The number of pages in the document.
    of: usize,
    /// The characters of text sent.
    chars: usize,
    /// The text stopped at [`MAX_CHARS`].
    cut: bool,
}

impl Sent {
    fn describe(&self) -> String {
        let Some((first, last)) = self.pages else { return String::new() };
        let pages = if first == last { format!("page {first}") } else { format!("pages {first}–{last}") };
        let mut s = format!("PdfCraft sent the text of {pages} of {} in {}: {} characters.", self.of, self.name, thousands(self.chars));
        if self.cut {
            s.push_str(&format!(" The limit is {} characters. The text after the limit was not sent.", thousands(MAX_CHARS)));
        }
        s
    }
}

/// `40000` → `40,000`.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len().saturating_add(digits.len() / 3));
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && digits.len().saturating_sub(i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The text of `pages`, each page after a `[Page n]` line, cut at `max` characters (never inside
/// a character). `text_of` reads one 0-based page; a page it can't read counts as a page without
/// text. Returns the text, the first and last 1-based page it holds, its length in characters and
/// whether it was cut. Stops early, with what it has, once `stop` says so.
fn gather(
    pages: Range<usize>,
    max: usize,
    stop: &dyn Fn() -> bool,
    mut text_of: impl FnMut(usize) -> Result<String, String>,
) -> (String, Option<(usize, usize)>, usize, bool) {
    let mut out = String::new();
    let mut used = 0usize;
    let mut span: Option<(usize, usize)> = None;
    let mut cut = false;
    for page in pages {
        if stop() {
            break;
        }
        let text = text_of(page).unwrap_or_default();
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let room = max.saturating_sub(used);
        if room == 0 {
            cut = true;
            break;
        }
        let number = page.saturating_add(1);
        span = Some((span.map_or(number, |(first, _)| first), number));
        let block = format!("[Page {number}]\n{text}\n\n");
        let n = block.chars().count();
        if n > room {
            out.extend(block.chars().take(room));
            used = used.saturating_add(room);
            cut = true;
            break;
        }
        out.push_str(&block);
        used = used.saturating_add(n);
    }
    (out, span, used, cut)
}

fn fail(message: impl Into<String>) -> Fail {
    Fail { kind: "pdfcraft".into(), message: message.into(), ..Default::default() }
}

/// What a tool sends, gathered on the UI thread when the person clicks.
struct Request {
    source: ExportSource,
    pages: Range<usize>,
    of: usize,
    name: String,
    question: String,
}

/// Read the pages' text and send it: on a worker thread.
fn send(provider: &Provider, tool: Tool, request: Request, stop: &AtomicBool) -> Result<(Reply, Sent), Fail> {
    let Request { source, pages, of, name, question } = request;
    let stopped = || stop.load(Ordering::Relaxed);
    let read = pdfcraft_engine::guard(|| {
        let mut exporter = Exporter::from_source(source);
        gather(pages, MAX_CHARS, &stopped, |page| exporter.text(page))
    });
    let (text, span, chars, cut) = read.map_err(|e| fail(format!("PdfCraft could not read the text of this document ({e}). Nothing was sent.")))?;
    if stopped() {
        return Err(Fail { kind: STOPPED.into(), ..Default::default() });
    }
    if text.trim().is_empty() {
        return Err(fail(
            "These pages have no text that PdfCraft can read, so nothing was sent. For scanned pages, use Scan & OCR ▸ Recognize text first.",
        ));
    }
    let user = match tool {
        Tool::Summary => format!("Summarize this document. It has {of} pages.\n\nText extracted from the PDF:\n\n{text}"),
        Tool::Ask => format!("Question: {question}\n\nThe document has {of} pages. Text extracted from the PDF:\n\n{text}"),
    };
    let sent = Sent { name, pages: span, of, chars, cut };
    client::chat(provider, tool.system(), &user).map(|reply| (reply, sent))
}

/// One tool's request in flight and its last answer.
#[derive(Default)]
struct Job {
    /// The answer and what its request carried.
    result: Option<(String, Sent)>,
    error: Option<String>,
    rx: Option<Receiver<Result<(Reply, Sent), Fail>>>,
    /// Set when the dialog closes: a request that has not left yet does not leave.
    stop: Option<Arc<AtomicBool>>,
}

impl Job {
    fn busy(&self) -> bool {
        self.rx.is_some()
    }

    fn poll(&mut self, state: &mut AiState) {
        let Some(rx) = self.rx.take() else { return };
        match rx.try_recv() {
            Ok(Ok((reply, sent))) => {
                state.note_reply(&reply);
                self.result = Some((reply.text.trim().to_string(), sent));
                self.error = None;
                self.stop = None;
            }
            Ok(Err(f)) => {
                self.stop = None;
                if f.kind == STOPPED {
                    return;
                }
                state.note_fail(&f);
                // A 402 or a retired key is the notice bar's job (one message, one link); the rest is ours.
                if state.notice.is_none() {
                    self.error = Some(f.message);
                }
            }
            Err(TryRecvError::Empty) => self.rx = Some(rx),
            Err(TryRecvError::Disconnected) => {
                self.stop = None;
                self.error = Some("The request stopped unexpectedly. Try again.".into());
            }
        }
    }

    fn stop_unsent(&mut self) {
        if let Some(stop) = &self.stop {
            stop.store(true, Ordering::Relaxed);
        }
    }

    fn start(&mut self, ctx: &egui::Context, provider: Provider, tool: Tool, request: Request) {
        self.result = None;
        self.error = None;
        let stop = Arc::new(AtomicBool::new(false));
        self.stop = Some(Arc::clone(&stop));
        self.rx = Some(spawn(ctx, move || send(&provider, tool, request, &stop)));
    }
}

/// Once per frame, before drawing: finish background work, stop a request whose dialog closed,
/// and open the setup dialog when it is due and nothing else is open.
pub(crate) fn frame(app: &mut PdfCraftApp, ctx: &egui::Context) {
    let open = app.dialog;
    // From Preferences ▸ AI the setup flow takes the Preferences dialog's place, and goes back to it.
    let blocked = app.palette_open
        || app.image_import.is_some()
        || app.pending_full_screen.is_some()
        || (app.modal_open() && open != Some(Dialog::Preferences));
    let Some(ai) = app.ai.as_mut() else { return };
    ai.state.poll();
    ai.summary.poll(&mut ai.state);
    ai.ask.poll(&mut ai.state);
    if open != Some(Dialog::AiSummary) {
        ai.summary.stop_unsent();
    }
    if open != Some(Dialog::AiAsk) {
        ai.ask.stop_unsent();
    }
    // The first launch asks once the window is up (a few frames in). "Later" ends that for good.
    if !ai.first_run_checked {
        ai.frames = ai.frames.saturating_add(1);
        ctx.request_repaint();
        if ai.frames > 3 {
            ai.first_run_checked = true;
            if ai.state.wants_first_run() || ai.state.owes_first_card() {
                ai.open_setup = true;
            }
        }
    }
    if !ai.open_setup || blocked {
        return;
    }
    ai.open_setup = false;
    ai.setup_return = open;
    ai.state.begin();
    app.dialog = Some(Dialog::AiSetup);
}

/// All tools ▸ AI assistant and the palette run these ids; `false` for any other id.
pub(crate) fn run(app: &mut PdfCraftApp, command: &str) -> bool {
    match command {
        "ai.summary" => app.dialog = Some(Dialog::AiSummary),
        "ai.ask" => app.dialog = Some(Dialog::AiAsk),
        "ai.translate" => app.notify_tr("Translate is not available yet. Ask about this document can translate a short part of the text."),
        _ => return false,
    }
    true
}

/// The dialog body of `dialog` (one of the AI dialogs). Returns whether to close it, and the dialog
/// to show next instead.
pub(crate) fn dialog(app: &mut PdfCraftApp, ui: &mut egui::Ui, t: &Tokens, dialog: Dialog) -> (bool, Option<Dialog>) {
    match dialog {
        Dialog::AiSummary => tool(app, ui, t, Tool::Summary),
        Dialog::AiAsk => tool(app, ui, t, Tool::Ask),
        _ => setup(app, ui, t),
    }
}

fn heading(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(tl!(text)).font(theme::semibold(18.0)));
    ui.add_space(6.0);
}

fn note(ui: &mut egui::Ui, t: &Tokens, text: &str) {
    ui.label(egui::RichText::new(text).small().color(t.text_muted));
}

/// A message and a Close button. Returns whether Close was clicked.
fn message(ui: &mut egui::Ui, text: &str) -> bool {
    ui.label(text);
    ui.add_space(10.0);
    widgets::pill_button(ui, tl!("Close"), true).clicked()
}

const OFF: &str = "AI tools are off in this run because PdfCraft has no settings folder.";

fn setup(app: &mut PdfCraftApp, ui: &mut egui::Ui, _t: &Tokens) -> (bool, Option<Dialog>) {
    heading(ui, "AI");
    let Some(ai) = app.ai.as_mut() else {
        return (message(ui, OFF), None);
    };
    match ai.state.setup_ui(ui) {
        Action::None => (false, None),
        Action::OpenUrl(url) => {
            app.open_url(&url);
            (false, None)
        }
        Action::Close => {
            let back = ai.setup_return.take();
            (back.is_none(), back)
        }
    }
}

fn tool(app: &mut PdfCraftApp, ui: &mut egui::Ui, t: &Tokens, tool: Tool) -> (bool, Option<Dialog>) {
    heading(ui, tool.title());
    // The active document: its name, page count, current page and whether its text may be copied.
    let doc = app.active_ids().and_then(|(view, id)| {
        let doc = app.session.get(id)?;
        let current = app.views.get(view).map_or(0, |v| v.current);
        Some((doc.name.clone(), doc.info.pages.len(), current, doc.check_copying().is_ok()))
    });
    let ctx = ui.ctx().clone();
    let Some(ai) = app.ai.as_mut() else {
        return (message(ui, OFF), None);
    };
    let provider = ai.state.provider();
    if !provider.is_set_up() {
        ui.label("This tool needs AI. Set it up first: publik API, with no key to paste, or your own key.");
        ui.add_space(10.0);
        let mut out = (false, None);
        ui.horizontal(|ui| {
            if widgets::pill_button(ui, "Set up AI…", true).clicked() {
                ai.state.begin();
                ai.setup_return = Some(tool.dialog());
                out = (false, Some(Dialog::AiSetup));
            }
            if widgets::pill_button(ui, tl!("Close"), false).clicked() {
                out = (true, None);
            }
        });
        return out;
    }
    let Some((name, of, current, copying)) = doc else {
        return (message(ui, tl!("Open a document first")), None);
    };
    if !copying {
        let text = format!("{}. PdfCraft sends nothing from this document.", crate::copy_refused());
        return (message(ui, &text), None);
    }
    let Ai { state, scope, question, summary, ask, .. } = ai;
    let job = match tool {
        Tool::Summary => summary,
        Tool::Ask => ask,
    };
    let busy = job.busy();
    let mut start = false;
    let mut close = false;
    let nearby = scope_label(Scope::Nearby.pages(current, of));
    ui.add_enabled_ui(!busy, |ui| {
        ui.label(format!("Which text of {name} goes to the model:"));
        widgets::radio_value(ui, scope, Scope::Document, format!("The whole document, up to the first {} characters", thousands(MAX_CHARS)));
        widgets::radio_value(ui, scope, Scope::Nearby, nearby);
        note(ui, t, "PdfCraft sends only the text of these pages, as Export ▸ Text reads it. It does not send the PDF file.");
        if tool == Tool::Ask {
            ui.add_space(6.0);
            ui.add(
                widgets::block(question)
                    .desired_rows(2)
                    .desired_width(f32::INFINITY)
                    .char_limit(MAX_QUESTION_CHARS)
                    .hint_text("What does this document say about the deadline?"),
            );
        }
    });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if busy {
            ui.spinner();
            ui.label(tool.working());
        } else {
            let ready = tool == Tool::Summary || !question.trim().is_empty();
            if ui.add_enabled_ui(ready, |ui| widgets::pill_button(ui, tool.button(), true)).inner.clicked() {
                start = true;
            }
        }
    });
    if let Some(e) = &job.error {
        ui.add_space(4.0);
        ui.colored_label(ui.visuals().error_fg_color, e);
    }
    if let Some((answer, sent)) = &job.result {
        ui.add_space(6.0);
        egui::Frame::new().fill(t.hover).corner_radius(egui::CornerRadius::same(6)).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                ui.add(egui::Label::new(answer.as_str()).selectable(true).wrap());
            });
        });
        ui.add_space(4.0);
        note(ui, t, &sent.describe());
        ui.add_space(4.0);
        if widgets::pill_button(ui, tl!("Copy"), false).clicked() {
            ctx.copy_text(answer.clone());
        }
    }
    ui.add_space(4.0);
    let action = state.result_bar(ui);
    ui.add_space(8.0);
    if widgets::pill_button(ui, tl!("Close"), false).clicked() {
        close = true;
    }
    if let Action::OpenUrl(url) = action {
        app.open_url(&url);
    }
    if start {
        start_tool(app, &ctx, tool, provider);
    }
    (close, None)
}

fn scope_label(pages: Range<usize>) -> String {
    let (first, last) = (pages.start.saturating_add(1), pages.end);
    if last <= first { format!("This page (page {first})") } else { format!("This page and the pages next to it (pages {first}–{last})") }
}

/// Gather what the clicked tool sends and start it.
fn start_tool(app: &mut PdfCraftApp, ctx: &egui::Context, tool: Tool, provider: Provider) {
    let Some((view, id)) = app.active_ids() else { return };
    let Some(doc) = app.session.get(id) else { return };
    // Checked again here: nothing leaves a document that does not allow copying its text.
    if doc.check_copying().is_err() {
        return;
    }
    let of = doc.info.pages.len();
    let current = app.views.get(view).map_or(0, |v| v.current);
    let (source, name) = (doc.export_source(), doc.name.clone());
    let Some(ai) = app.ai.as_mut() else { return };
    let request = Request { source, pages: ai.scope.pages(current, of), of, name, question: ai.question.trim().to_string() };
    match tool {
        Tool::Summary => ai.summary.start(ctx, provider, tool, request),
        Tool::Ask => ai.ask.start(ctx, provider, tool, request),
    }
}

/// Preferences ▸ AI: the provider, its status and its controls.
pub(crate) fn preferences_section(ui: &mut egui::Ui, app: &mut PdfCraftApp, t: &Tokens) {
    ui.label(egui::RichText::new("AI").font(theme::semibold(13.0)));
    let Some(ai) = app.ai.as_mut() else {
        note(ui, t, OFF);
        return;
    };
    note(
        ui,
        t,
        "Summarize and Ask about this document (All tools ▸ AI assistant) use publik API or your own key. They send text only when you click.",
    );
    ui.add_space(4.0);
    let mut open_setup = false;
    let action = ai.state.options_ui(ui, &mut open_setup);
    if open_setup {
        ai.open_setup = true;
    }
    if let Action::OpenUrl(url) = action {
        app.open_url(&url);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pages(texts: &[&str]) -> impl FnMut(usize) -> Result<String, String> {
        let texts: Vec<String> = texts.iter().map(|s| s.to_string()).collect();
        move |p| texts.get(p).cloned().ok_or_else(|| "no such page".to_string())
    }

    #[test]
    fn text_is_gathered_page_by_page_and_skips_empty_pages() {
        let (text, span, chars, cut) = gather(0..4, 1000, &|| false, pages(&["One", "  ", "Three", "Four"]));
        assert_eq!(text, "[Page 1]\nOne\n\n[Page 3]\nThree\n\n[Page 4]\nFour\n\n");
        assert_eq!(span, Some((1, 4)));
        assert_eq!(chars, text.chars().count());
        assert!(!cut);
    }

    #[test]
    fn text_is_cut_at_the_limit_on_a_character_boundary() {
        // 12 characters end inside the second "é" when counted in bytes.
        let (text, span, chars, cut) = gather(0..3, 12, &|| false, pages(&["ééééé", "more", "rest"]));
        assert_eq!(chars, 12);
        assert_eq!(text, "[Page 1]\nééé");
        assert_eq!(span, Some((1, 1)));
        assert!(cut);
        // A page that fits exactly leaves no room, and the next page with text says so.
        let (_, span, _, cut) = gather(0..2, "[Page 1]\nab\n\n".chars().count(), &|| false, pages(&["ab", "cd"]));
        assert_eq!(span, Some((1, 1)));
        assert!(cut);
    }

    #[test]
    fn unreadable_and_missing_pages_and_a_stop_send_nothing_more() {
        let (text, span, _, cut) = gather(0..5, 100, &|| false, |_| Err("broken".to_string()));
        assert!(text.is_empty() && span.is_none() && !cut);
        let (text, span, _, _) = gather(0..3, 100, &|| true, pages(&["a", "b", "c"]));
        assert!(text.is_empty() && span.is_none());
        let (text, ..) = gather(0..0, 100, &|| false, pages(&[]));
        assert!(text.is_empty());
    }

    #[test]
    fn nearby_pages_stay_inside_the_document() {
        assert_eq!(Scope::Nearby.pages(0, 10), 0..2);
        assert_eq!(Scope::Nearby.pages(4, 10), 3..6);
        assert_eq!(Scope::Nearby.pages(9, 10), 8..10);
        assert_eq!(Scope::Nearby.pages(50, 10), 8..10);
        assert_eq!(Scope::Nearby.pages(0, 1), 0..1);
        assert_eq!(Scope::Nearby.pages(0, 0), 0..0);
        assert_eq!(Scope::Document.pages(3, 7), 0..7);
        assert_eq!(scope_label(0..1), "This page (page 1)");
        assert_eq!(scope_label(3..6), "This page and the pages next to it (pages 4–6)");
    }

    #[test]
    fn the_dialog_says_what_was_sent() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(40_000), "40,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
        let sent = Sent { name: "a.pdf".into(), pages: Some((1, 12)), of: 30, chars: 40_000, cut: true };
        assert_eq!(
            sent.describe(),
            "PdfCraft sent the text of pages 1–12 of 30 in a.pdf: 40,000 characters. The limit is 40,000 characters. The text after the limit was not sent."
        );
        let one = Sent { name: "b.pdf".into(), pages: Some((2, 2)), of: 3, chars: 120, cut: false };
        assert_eq!(one.describe(), "PdfCraft sent the text of page 2 of 3 in b.pdf: 120 characters.");
        assert_eq!(Sent::default().describe(), "");
    }

    #[test]
    fn the_catalogue_tools_open_their_dialogs() {
        let mut app = PdfCraftApp::new();
        assert!(run(&mut app, "ai.summary"));
        assert_eq!(app.dialog, Some(Dialog::AiSummary));
        assert!(run(&mut app, "ai.ask"));
        assert_eq!(app.dialog, Some(Dialog::AiAsk));
        assert!(!run(&mut app, "file.open"));
        // Every AI item in the catalogue is handled here.
        let ai = pdfcraft_engine::catalog::group("ai").map(|g| g.sections.iter().flat_map(|s| s.items.iter()).map(|i| i.command).collect::<Vec<_>>());
        for id in ai.unwrap_or_default() {
            assert!(run(&mut PdfCraftApp::new(), id), "{id}");
        }
    }

    fn harness(ai: Option<Ai>) -> egui_kittest::Harness<'static, PdfCraftApp> {
        let bytes = pdfcraft_engine::Session::new().create_from_text("Words", "Hello world").unwrap();
        let ai = std::cell::Cell::new(ai);
        egui_kittest::Harness::builder().with_size(egui::vec2(1200.0, 900.0)).build_eframe(move |_| {
            let mut app = PdfCraftApp::new();
            app.set_option("language", "en").unwrap();
            app.open_bytes("words.pdf", None, bytes.as_ref().clone()).unwrap();
            app.ai = ai.take();
            app
        })
    }

    /// An AI state with nothing set up: an own settings folder, and a slug no credential file has.
    /// `None` when the environment carries a publik key, which would count as set up.
    fn fresh_ai(name: &str) -> Option<Ai> {
        if std::env::var_os("PUBLIK_API_KEY").is_some() {
            return None;
        }
        let dir = std::env::temp_dir().join(format!("pdfcraft-ai-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Some(Ai::new(AppInfo { slug: "pdfcraft-ui-test", name: "PdfCraft", version: "0", app_token: "", dialects: &[], config_dir: Some(dir) }))
    }

    #[test]
    fn without_settings_the_tools_say_they_are_off_and_send_nothing() {
        use egui_kittest::kittest::Queryable;
        let mut h = harness(None);
        // All tools ▸ AI assistant ▸ Summarize opens the dialog.
        h.state_mut().select_tool("ai");
        h.run_steps(3);
        h.get_by_label("Summarize").click();
        h.run_steps(3);
        assert_eq!(h.state().dialog, Some(Dialog::AiSummary));
        h.get_by_label(OFF);
        h.get_by_label("Close").click();
        h.run_steps(2);
        assert!(h.state().dialog.is_none());
    }

    #[test]
    fn the_first_launch_asks_once_and_later_closes_it() {
        use egui_kittest::kittest::Queryable;
        let Some(ai) = fresh_ai("first-run") else { return };
        let mut h = harness(Some(ai));
        h.run_steps(8);
        assert_eq!(h.state().dialog, Some(Dialog::AiSetup), "the setup dialog opens by itself");
        h.get_by_label("Use my own key");
        h.get_by_label("Later").click();
        h.run_steps(3);
        assert!(h.state().dialog.is_none());
        assert!(h.state().ai.as_ref().is_some_and(|ai| ai.state.settings.setup_done), "Later ends it for good");
        // An agent's run never sees it.
        let Some(mut ai) = fresh_ai("agent") else { return };
        ai.skip_first_run();
        let mut h = harness(Some(ai));
        h.run_steps(8);
        assert!(h.state().dialog.is_none());
    }

    #[test]
    fn a_tool_without_ai_offers_the_setup_and_comes_back() {
        use egui_kittest::kittest::Queryable;
        let Some(mut ai) = fresh_ai("tool") else { return };
        ai.skip_first_run();
        let mut h = harness(Some(ai));
        h.state_mut().run_command("ai.ask");
        h.run_steps(3);
        assert_eq!(h.state().dialog, Some(Dialog::AiAsk));
        h.get_by_label("Set up AI…").click();
        h.run_steps(3);
        assert_eq!(h.state().dialog, Some(Dialog::AiSetup));
        h.get_by_label("Later").click();
        h.run_steps(3);
        assert_eq!(h.state().dialog, Some(Dialog::AiAsk), "back to the tool that asked");
        assert!(h.state().ai.as_ref().is_some_and(|ai| ai.summary.rx.is_none() && ai.ask.rx.is_none()), "nothing was sent");
    }
}
