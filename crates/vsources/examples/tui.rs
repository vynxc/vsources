//! A terminal UI over the `vsources` SDK: type a media id, fan out the
//! provider catalog, and play the selected stream in `mpv` (hotlink
//! headers included).
//!
//! Run it with a TMDB key:
//!
//! ```text
//! TMDB_API_KEY=... cargo run -p vsources --example tui
//! ```
//!
//! Keys: `Tab`/`↑↓` move between the query fields (and into the stream
//! table), `←→` or `m`/`s` toggle movie/series, `Enter` resolves (and
//! plays in the table), `q` quits. `mpv` needs to be on `PATH`.
//!
//! Results are progressive: streams land in the table as providers
//! answer — re-sorted as they arrive — instead of waiting for the whole
//! fan-out to finish.

use std::process::Command;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, TableState};
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::{mpsc, oneshot};
use vsources::{Engine, EngineBuilder, EngineError, MediaId, MediaRef, MediaType, Stream};

/// Braille spinner frames — one per 100 ms poll tick.
const SPINNER: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";
/// The query form's field labels, in order.
const FIELDS: [&str; 4] = ["Id", "Kind", "Season", "Episode"];

/// Where key input goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// The query form.
    Form,
    /// The stream table.
    Table,
}

/// The application state.
struct App {
    /// The engine, built on first resolve and shared with the in-flight
    /// task — lazy so a missing TMDB key is a status message, not a
    /// startup crash.
    engine: Option<std::sync::Arc<Engine>>,
    /// Query fields: id, kind, season, episode.
    fields: [String; 4],
    /// The active form field.
    field: usize,
    /// Form or table focus.
    focus: Focus,
    /// The resolved streams.
    streams: Vec<Stream>,
    /// The table's selection state.
    table: TableState,
    /// Whether a resolve is in flight.
    resolving: bool,
    /// Spinner tick (poll iterations so far).
    tick: usize,
    /// The one-line status message.
    status: String,
    /// The pending final resolve answer.
    answer: Option<oneshot::Receiver<Result<Vec<Stream>, EngineError>>>,
    /// Live merged snapshots from the in-flight resolve, one per
    /// provider that yields streams.
    batches: Option<mpsc::UnboundedReceiver<Vec<Stream>>>,
}

impl App {
    /// Start from the default query.
    fn new() -> Self {
        Self {
            engine: None,
            fields: [
                String::new(),
                "movie".to_string(),
                String::new(),
                String::new(),
            ],
            field: 0,
            focus: Focus::Form,
            streams: Vec::new(),
            table: TableState::default(),
            resolving: false,
            tick: 0,
            status: "type an id — tmdb:209867 (Frieren), 27205 (Inception), or tt1375666"
                .to_string(),
            answer: None,
            batches: None,
        }
    }

    /// The selected stream, when there is one.
    fn selected(&self) -> Option<&Stream> {
        self.table
            .selected()
            .and_then(|index| self.streams.get(index))
    }

    /// The `MediaRef` the form describes, or a status-line reason.
    fn media(&self) -> Result<MediaRef, String> {
        let id = MediaId::parse(self.fields[0].trim())
            .ok_or_else(|| "unrecognized id — want tmdb:27205, 27205, or tt1375666".to_string())?;
        let kind = if self.fields[1].trim().eq_ignore_ascii_case("series") {
            MediaType::Series
        } else {
            MediaType::Movie
        };
        let season = self.fields[2].trim().parse::<u32>().ok();
        let episode = self.fields[3].trim().parse::<u32>().ok();
        Ok(MediaRef {
            id,
            kind,
            season,
            episode,
        })
    }

    /// Fan the query out across the provider catalog.
    fn resolve(&mut self) {
        if self.resolving {
            return;
        }
        let media = match self.media() {
            Ok(media) => media,
            Err(message) => {
                self.status = message;
                return;
            }
        };
        let engine = match self.engine.clone() {
            Some(engine) => engine,
            None => match EngineBuilder::new().with_default_providers().build() {
                Ok(engine) => {
                    let engine = std::sync::Arc::new(engine);
                    self.engine = Some(std::sync::Arc::clone(&engine));
                    engine
                }
                Err(error) => {
                    self.status = match error {
                        EngineError::NoTmdb => {
                            "set TMDB_API_KEY (or TMDB_ACCESS_TOKEN) and try again".to_string()
                        }
                        other => format!("engine build failed: {other}"),
                    };
                    return;
                }
            },
        };
        self.resolving = true;
        self.streams.clear();
        self.table.select(None);
        self.focus = Focus::Form;
        self.status = format!("resolving {}…", self.fields[0].trim());
        let (progress, batches) = mpsc::unbounded_channel();
        let (sender, receiver) = oneshot::channel();
        self.batches = Some(batches);
        self.answer = Some(receiver);
        tokio::spawn(resolve_task(engine, media, progress, sender));
    }

    /// Fold live snapshots into the table, then drain the final answer.
    ///
    /// Snapshots replace the table wholesale (each is the full merged
    /// view) while keeping the selection pinned to its stream across
    /// the re-sorts; the final answer clears the in-flight state.
    fn poll_answer(&mut self) {
        if let Some(batches) = self.batches.as_mut() {
            while let Ok(snapshot) = batches.try_recv() {
                let pinned = self
                    .table
                    .selected()
                    .and_then(|index| self.streams.get(index))
                    .map(|stream| stream.url.clone());
                self.streams = snapshot;
                let selection =
                    pinned.and_then(|url| self.streams.iter().position(|stream| stream.url == url));
                self.table
                    .select(selection.or_else(|| (!self.streams.is_empty()).then_some(0)));
                if self.resolving {
                    self.status = format!(
                        "resolving {}… {} streams so far",
                        self.fields[0].trim(),
                        self.streams.len()
                    );
                }
            }
        }

        let Some(receiver) = self.answer.as_mut() else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(streams)) => {
                self.resolving = false;
                self.answer = None;
                self.batches = None;
                self.status = if streams.is_empty() {
                    "no streams found — try another id, or check the site is up".to_string()
                } else {
                    format!("{} streams — Enter plays in mpv", streams.len())
                };
                self.streams = streams;
                if self.streams.is_empty() {
                    self.table.select(None);
                } else if self.table.selected().is_none() {
                    self.table.select(Some(0));
                }
            }
            Ok(Err(error)) => {
                self.resolving = false;
                self.answer = None;
                self.batches = None;
                self.status = format!("resolve failed: {error}");
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.resolving = false;
                self.answer = None;
                self.batches = None;
                self.status = "the resolve task died".to_string();
            }
        }
    }
    /// Cycle form fields (wrapping into the table when results exist).
    fn next_field(&mut self) {
        self.focus = Focus::Form;
        self.field = (self.field + 1) % FIELDS.len();
        if self.field == 0 && !self.streams.is_empty() {
            self.focus = Focus::Table;
        }
    }

    /// Cycle backwards.
    fn previous_field(&mut self) {
        if self.focus == Focus::Table || (self.field == 0 && !self.streams.is_empty()) {
            self.focus = Focus::Form;
            self.field = FIELDS.len() - 1;
        } else if self.focus == Focus::Form {
            self.field = self.field.saturating_sub(1);
        }
    }

    /// Toggle the kind field between movie and series.
    fn toggle_kind(&mut self, series: bool) {
        self.fields[1] = if series { "series" } else { "movie" }.to_string();
    }

    /// Handle one key press; returns `true` to quit.
    fn key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'q'))
        {
            return true;
        }
        if key.kind != KeyEventKind::Press {
            return false;
        }
        if self.focus == Focus::Table && matches!(key.code, KeyCode::Char('q')) {
            return true;
        }
        match (self.focus, key.code) {
            // The table: navigate, play, or leave.
            (Focus::Table, KeyCode::Esc) => self.focus = Focus::Form,
            (Focus::Table, KeyCode::Tab) => {
                self.focus = Focus::Form;
                self.field = 0;
            }
            (Focus::Table, KeyCode::Up | KeyCode::Char('j')) => {
                self.step_selection(usize::MAX);
            }
            (Focus::Table, KeyCode::Down | KeyCode::Char('k')) => {
                self.step_selection(1);
            }
            // The form: field navigation and editing.
            (Focus::Form, KeyCode::Tab | KeyCode::Down) => self.next_field(),
            (Focus::Form, KeyCode::BackTab | KeyCode::Up) => self.previous_field(),
            // `m`/`s` and arrows both flip the movie/series toggle.
            (Focus::Form, KeyCode::Left | KeyCode::Char('m')) if self.field == 1 => {
                self.toggle_kind(false);
            }
            (Focus::Form, KeyCode::Right | KeyCode::Char('s')) if self.field == 1 => {
                self.toggle_kind(true);
            }
            (Focus::Form, KeyCode::Char(character)) => {
                self.fields[self.field].push(character);
            }
            (Focus::Form, KeyCode::Backspace) if self.field != 1 => {
                self.fields[self.field].pop();
            }
            (Focus::Form, KeyCode::Enter) => self.resolve(),
            // Enter and `p` in the table are handled by the caller (they
            // need the terminal for the mpv interlude) and land here.
            _ => {}
        }
        false
    }

    /// Move the table selection by `step` (saturating, wrap-free).
    fn step_selection(&mut self, step: usize) {
        let last = self.streams.len().saturating_sub(1);
        let current = self.table.selected().unwrap_or(0);
        let next = if step == usize::MAX {
            current.saturating_sub(1)
        } else {
            (current + step).min(last)
        };
        self.table.select(Some(next));
    }
}

/// One in-flight resolve: owns its inputs, streams merged snapshots
/// through `progress` as providers land, and delivers the final answer
/// through `done`, so the event loop never awaits it directly.
async fn resolve_task(
    engine: std::sync::Arc<Engine>,
    media: MediaRef,
    progress: mpsc::UnboundedSender<Vec<Stream>>,
    done: oneshot::Sender<Result<Vec<Stream>, EngineError>>,
) {
    let _ = done.send(engine.resolve_progressive(&media, progress).await);
}

/// The mpv argument list for one stream: the URL, the label as the
/// window title, and every hotlink header (Referer/UA/Origin) mpv must
/// send — the request-headers equivalent of the upstream proxy.
fn mpv_args(stream: &Stream) -> Vec<String> {
    let mut args = vec![stream.url.as_str().to_string()];
    if let Some(label) = stream.label.as_deref() {
        args.push(format!("--title={label}"));
    }
    for (name, value) in &stream.meta.request_headers {
        if name.eq_ignore_ascii_case("User-Agent") {
            args.push(format!("--user-agent={value}"));
        }
        args.push(format!("--http-header-fields-append={name}: {value}"));
    }
    if stream.format == vsources::types::Format::Hls {
        args.push("--demuxer-lavf-o=allowed_extensions=ALL,allowed_segment_extensions=ALL,extension_picky=0,protocol_whitelist=[http,https,tcp,tls,crypto,data]".to_string());
    }
    args
}

/// Play one stream in mpv (with the terminal suspended around it) and
/// answer the status line for it.
fn play_mpv(terminal: &mut DefaultTerminal, stream: &Stream) -> String {
    ratatui::restore();
    let outcome = Command::new("mpv").args(mpv_args(stream)).status();
    *terminal = ratatui::init();
    match outcome {
        Ok(status) if status.success() => "mpv finished".to_string(),
        Ok(status) => format!("mpv exited with {status}"),
        Err(error) => format!("failed to start mpv: {error} (is it on PATH?)"),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new();
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    result
}

/// The draw/poll loop: draws, polls keys (100 ms), drains the resolve
/// answer, and handles the mpv interlude on `Enter`.
fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        app.tick = app.tick.wrapping_add(1);
        terminal.draw(|frame| draw(frame, app))?;

        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            if app.key(key) {
                return Ok(());
            }
            if app.focus == Focus::Table
                && matches!(key.code, KeyCode::Enter | KeyCode::Char('p'))
                && key.kind == KeyEventKind::Press
                && let Some(stream) = app.selected().cloned()
            {
                app.status = play_mpv(terminal, &stream);
            }
        }
        app.poll_answer();
    }
}

/// Render one frame: the query form, the stream table, the status line.
fn draw(frame: &mut Frame, app: &mut App) {
    let [form, table, status] = Layout::vertical([
        Constraint::Length(u16::try_from(FIELDS.len()).unwrap_or(u16::MAX) + 2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_form(frame, app, form);
    draw_table(frame, app, table);
    draw_status(frame, app, status);
}

/// The query form: four labeled fields with the active one highlighted.
fn draw_form(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let lines = FIELDS
        .iter()
        .zip(&app.fields)
        .enumerate()
        .map(|(index, (name, value))| {
            let active = app.focus == Focus::Form && app.field == index;
            let style = if active {
                Style::new().fg(Color::Yellow).bold()
            } else {
                Style::new()
            };
            let cursor = if active { "▏" } else { "" };
            Line::from(vec![
                Span::styled(format!("{name:<8}"), Style::new().bold()),
                Span::styled(format!("{value}{cursor}"), style),
            ])
        })
        .collect::<Vec<_>>();
    let block = Block::bordered().title(" vsources — 47 providers (needs TMDB_API_KEY) ");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// The stream table: rank, quality, format, dub/sub, provider, label.
fn draw_table(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let rows = app
        .streams
        .iter()
        .enumerate()
        .map(|(index, stream)| {
            let quality = stream.meta.resolution.map_or_else(
                || stream.meta.quality.clone().unwrap_or_default(),
                |height| format!("{height}p"),
            );
            let track = match (stream.meta.dubbed, stream.meta.subbed) {
                (Some(true), Some(true)) => "dub|sub".to_string(),
                (Some(true), _) => "dub".to_string(),
                (_, Some(true)) => "sub".to_string(),
                _ => String::new(),
            };
            let provider = stream
                .meta
                .source_label
                .clone()
                .or_else(|| stream.meta.source_id.clone())
                .unwrap_or_default();
            Row::new(vec![
                Cell::from((index + 1).to_string()),
                Cell::from(quality),
                Cell::from(format!("{:?}", stream.format)),
                Cell::from(track),
                Cell::from(provider),
                Cell::from(stream.label.clone().unwrap_or_default()),
            ])
        })
        .collect::<Vec<_>>();
    let header = Row::new(vec![
        Cell::from("#".bold()),
        Cell::from("Quality".bold()),
        Cell::from("Fmt".bold()),
        Cell::from("Track".bold()),
        Cell::from("Provider".bold()),
        Cell::from("Title".bold()),
    ]);
    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Length(9),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(14),
            Constraint::Min(20),
        ],
    )
    .header(header)
    .row_highlight_style(Style::new().reversed())
    .highlight_symbol("▶ ")
    .block(Block::bordered().title(" Streams "));
    frame.render_stateful_widget(table, area, &mut app.table);
}

/// The status line: spinner while resolving, hints otherwise.
fn draw_status(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let spans = if app.resolving {
        let spinner = SPINNER
            .chars()
            .nth(app.tick % SPINNER.chars().count())
            .unwrap_or_default();
        vec![
            Span::styled(format!("{spinner} "), Style::new().fg(Color::Cyan)),
            Span::from(app.status.clone()),
        ]
    } else {
        vec![
            Span::styled(app.status.clone(), Style::new()),
            Span::raw("   "),
            Span::styled(
                match app.focus {
                    Focus::Form => "Tab/↑↓ fields · ←→ or m/s kind · Enter resolve",
                    Focus::Table => "↑↓ select · Enter/p mpv · Tab edit · q quit",
                },
                Style::new().fg(Color::DarkGray),
            ),
        ]
    };
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod playback_tests {
    use super::*;
    #[test]
    fn hls_player_keeps_all_headers_and_accepts_video_with_image_extensions() {
        let mut stream = Stream::new(
            url::Url::parse("https://cdn.example/master.m3u8")
                .unwrap_or_else(|e| panic!("URL: {e}")),
            vsources::types::Format::Hls,
        );
        stream
            .meta
            .request_headers
            .insert("Origin".into(), "https://origin.example".into());
        stream
            .meta
            .request_headers
            .insert("Referer".into(), "https://origin.example/".into());
        let args = mpv_args(&stream);
        assert_eq!(
            args.iter()
                .filter(|s| s.starts_with("--http-header-fields-append="))
                .count(),
            2
        );
        assert!(
            args.iter()
                .any(|s| s.contains("allowed_segment_extensions=ALL"))
        );
    }
}
