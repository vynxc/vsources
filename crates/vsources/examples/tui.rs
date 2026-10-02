//! A terminal UI over the `vsources` SDK: search a title on Cinemeta
//! (`https://v3-cinemeta.strem.io/manifest.json` — no key needed),
//! fan the selected hit out across the provider catalog, and play the
//! chosen stream in `mpv` (hotlink headers included).
//!
//! Run it with a TMDB key in a `.env` file at the repo root (search
//! works without one, resolving does not — the providers use TMDB for
//! metadata). `.env` (or `.env.local`):
//!
//! ```text
//! TMDB_API_KEY=...   # or TMDB_ACCESS_TOKEN=...
//! ```
//!
//! ```text
//! cargo run -p vsources --example tui
//! ```
//!
//! Keys: `Tab`/`↑↓` move between the query fields, the search results,
//! episode picker and stream table; `←→` or `m`/`s` toggle movie/series.
//! `Enter` searches, chooses a movie or series, resolves the chosen episode,
//! then plays. `Esc` returns from episodes to series results; `Ctrl+q` quits.
//! `mpv` needs to be on
//! `PATH`.
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
use vsources::{Engine, EngineBuilder, EngineError, MediaRef, MediaType, Stream};

// Cinemeta client — free catalog search, used for the query field so
// the TUI works without TMDB credentials.
mod cinemeta;
use cinemeta::SearchState;

/// Braille spinner frames — one per 100 ms poll tick.
const SPINNER: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";
/// The query form's field labels, in order (the title is a free-text
/// search handled by the Cinemeta client).
const FIELDS: [&str; 2] = ["Search", "Kind"];
/// When resolved streams are considered stale: short-lived tokens
/// (`AniKage`'s relay expires them in ~3 min; the upstream override is
/// `TTL = 3 min`) can 400 in the player after this long.
const STALE_AFTER: Duration = Duration::from_secs(150);

/// Where key input goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// The query form.
    Form,
    /// The Cinemeta search results.
    Results,
    /// The episode picker.
    Episodes,
    /// The stream table.
    Table,
}

/// The application state.
struct App {
    /// The engine, built on first resolve and shared with the in-flight
    /// task — lazy so a missing TMDB key is a status message, not a
    /// startup crash.
    engine: Option<std::sync::Arc<Engine>>,
    /// Query fields: search and kind. The search text
    /// lives in [`Self::search`] — `fields[0]` mirrors it for the form.
    fields: [String; 2],
    /// Selected series metadata, including episode titles and air dates.
    episodes: cinemeta::Episodes,
    episode_table: TableState,
    /// The Cinemeta catalog search and its results.
    search: cinemeta::Search,
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
    /// When the current streams were resolved — short-lived stream
    /// tokens (`AniKage`'s relay expires them in ~3 min) mean a stale
    /// table can 400 in the player, so Enter re-resolves past that.
    resolved_at: Option<std::time::Instant>,
    /// The media the current table describes (for the re-resolve).
    resolved_media: Option<MediaRef>,
}

impl App {
    /// Start from the default query.
    fn new() -> Self {
        Self {
            engine: None,
            fields: [String::new(), "movie".to_string()],
            episodes: cinemeta::Episodes::default(),
            episode_table: TableState::default(),
            field: 0,
            focus: Focus::Form,
            streams: Vec::new(),
            table: TableState::default(),
            resolving: false,
            tick: 0,
            status: "type a title — Enter searches, Enter again resolves, Enter in the table plays"
                .to_string(),
            answer: None,
            batches: None,
            search: cinemeta::Search::new(),
            resolved_at: None,
            resolved_media: None,
        }
    }

    /// The selected stream, when there is one.
    fn selected(&self) -> Option<&Stream> {
        self.table
            .selected()
            .and_then(|index| self.streams.get(index))
    }

    /// The `MediaRef` the selected search hit describes, or a
    /// status-line reason.
    fn media(&self) -> Result<MediaRef, String> {
        let hit = self.search.media()?;
        if hit.kind == MediaType::Series {
            if self.episodes.imdb_id.as_deref() != Some(hit.imdb_id.as_str()) {
                return Err("select the series to load its episodes".into());
            }
            let episode = self.episodes.selected().ok_or("select an episode first")?;
            Ok(hit.into_media_ref(Some(episode.season), Some(episode.episode)))
        } else {
            Ok(hit.into_media_ref(None, None))
        }
    }

    /// The current kind (movie/series) per the kind field.
    fn kind(&self) -> MediaType {
        if self.fields[1].trim().eq_ignore_ascii_case("series") {
            MediaType::Series
        } else {
            MediaType::Movie
        }
    }

    /// Run a Cinemeta catalog search for the typed query, or — when
    /// results are already on screen — resolve the selection instead.
    ///
    /// Search needs no credentials; resolve needs TMDB (the providers
    /// lean on it for metadata), so the fetcher is built standalone when
    /// the engine isn't up yet.
    fn search(&mut self) {
        // Keep the search client's query in sync with the form field.
        self.search.query = self.fields[0].clone();
        if self.search.state == SearchState::Done && self.search.media().is_ok() {
            // Already showing results for this query: resolve instead.
            self.pick_result();
            return;
        }
        self.episodes = cinemeta::Episodes::default();
        if let Some(engine) = self.engine.as_ref() {
            self.search.start(std::sync::Arc::clone(engine.fetcher()));
        } else if let Ok(fetcher) = vsources_net::ChromeFetcher::builder().build() {
            self.search.start(std::sync::Arc::new(fetcher));
            self.status = "searching cinemeta…".to_string();
        } else {
            self.status = "could not build a fetcher for search".to_string();
        }
        if self.search.state == SearchState::Loading {
            self.status = "searching cinemeta…".to_string();
        }
    }

    /// Movies resolve directly; series open the episode picker.
    fn pick_result(&mut self) {
        let hit = match self.search.media() {
            Ok(hit) => hit,
            Err(message) => {
                self.status = message;
                return;
            }
        };
        if hit.kind == MediaType::Movie {
            self.resolve();
            return;
        }
        self.focus = Focus::Episodes;
        if self.episodes.imdb_id.as_deref() == Some(hit.imdb_id.as_str())
            && (self.episodes.loading || !self.episodes.items.is_empty())
        {
            return;
        }
        let fetcher = if let Some(engine) = self.engine.as_ref() {
            std::sync::Arc::clone(engine.fetcher())
        } else {
            match vsources_net::ChromeFetcher::builder().build() {
                Ok(fetcher) => std::sync::Arc::new(fetcher),
                Err(error) => {
                    self.status = format!("fetcher failed: {error}");
                    return;
                }
            }
        };
        self.episodes.start(fetcher, hit.imdb_id);
        self.status = "loading episodes…".into();
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
            None => match EngineBuilder::new()
                .with_default_providers()
                // Every provider races at once — this TUI exists to feel
                // the per-provider speed, not to babysit a queue.
                .concurrency(100)
                .build()
            {
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
        let title = self
            .search
            .selected()
            .map_or_else(|| self.fields[0].trim().to_string(), |hit| hit.name.clone());
        self.status = format!("resolving {title}…");
        let (progress, batches) = mpsc::unbounded_channel();
        let (sender, receiver) = oneshot::channel();
        self.batches = Some(batches);
        self.answer = Some(receiver);
        self.resolved_media = Some(media.clone());
        self.resolved_at = Some(std::time::Instant::now());
        tokio::spawn(resolve_task(engine, media, progress, sender));
    }

    /// Whether the table's streams may have expired (short-lived
    /// tokens). When true, playing should re-resolve first.
    fn streams_stale(&self) -> bool {
        self.resolved_at
            .is_some_and(|at| at.elapsed() > STALE_AFTER)
    }

    /// Re-run the last resolve (fresh tokens), keeping the selection
    /// topic so the user lands on a comparable row.
    fn re_resolve(&mut self) -> bool {
        let Some(media) = self.resolved_media.clone() else {
            return false;
        };
        self.resolve_ref(media);
        true
    }

    /// Resolve a known media reference directly (no search involved).
    fn resolve_ref(&mut self, media: MediaRef) {
        if self.resolving {
            return;
        }
        let engine = match self.engine.clone() {
            Some(engine) => engine,
            None => match EngineBuilder::new()
                .with_default_providers()
                .concurrency(100)
                .build()
            {
                Ok(engine) => {
                    let engine = std::sync::Arc::new(engine);
                    self.engine = Some(std::sync::Arc::clone(&engine));
                    engine
                }
                Err(error) => {
                    self.status = format!("engine build failed: {error}");
                    return;
                }
            },
        };
        let title = self
            .search
            .selected()
            .map(|hit| hit.name.clone())
            .unwrap_or_default();
        self.resolving = true;
        self.streams.clear();
        self.table.select(None);
        self.status = format!("re-resolving {title} (fresh tokens)…");
        let (progress, batches) = mpsc::unbounded_channel();
        let (sender, receiver) = oneshot::channel();
        self.batches = Some(batches);
        self.answer = Some(receiver);
        self.resolved_media = Some(media.clone());
        self.resolved_at = Some(std::time::Instant::now());
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
                        self.search
                            .selected()
                            .map_or(self.fields[0].trim(), |hit| hit.name.as_str()),
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

    /// Toggle the kind field between movie and series (and reset any
    /// search results from the other catalog).
    fn toggle_kind(&mut self, series: bool) {
        self.fields[1] = if series { "series" } else { "movie" }.to_string();
        self.search.set_kind(self.kind());
        self.episodes = cinemeta::Episodes::default();
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
            // The stream table: navigate, play, or leave.
            (Focus::Table, KeyCode::Esc) => self.focus = Focus::Form,
            (Focus::Table, KeyCode::Tab) => {
                self.focus = Focus::Results;
            }
            (Focus::Table, KeyCode::Up | KeyCode::Char('j')) => {
                self.step_selection(usize::MAX);
            }
            (Focus::Table, KeyCode::Down | KeyCode::Char('k')) => {
                self.step_selection(1);
            }
            // The search results: navigate and pick.
            (Focus::Results, KeyCode::Tab) => {
                self.focus = Focus::Form;
                self.field = 0;
            }
            (Focus::Results, KeyCode::Up | KeyCode::Char('j')) => {
                self.search.step_selection(-1);
            }
            (Focus::Results, KeyCode::Down | KeyCode::Char('k')) => {
                self.search.step_selection(1);
            }
            (Focus::Results, KeyCode::Enter) => self.pick_result(),
            (Focus::Episodes, KeyCode::Up | KeyCode::Char('k')) => self.episodes.step(-1),
            (Focus::Episodes, KeyCode::Down | KeyCode::Char('j')) => self.episodes.step(1),
            (Focus::Episodes, KeyCode::Enter) => self.resolve(),
            (Focus::Episodes, KeyCode::Esc | KeyCode::BackTab) => self.focus = Focus::Results,
            (Focus::Episodes, KeyCode::Tab) => {
                self.focus = if self.streams.is_empty() {
                    Focus::Form
                } else {
                    Focus::Table
                };
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
                // Editing the query invalidates the current results.
                if self.field == 0 {
                    self.search.invalidate();
                    self.episodes = cinemeta::Episodes::default();
                }
            }
            (Focus::Form, KeyCode::Backspace) if self.field != 1 => {
                self.fields[self.field].pop();
                self.search.invalidate();
                self.episodes = cinemeta::Episodes::default();
            }
            (Focus::Form, KeyCode::Enter) => self.search(),
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
    if let Some(selection) = &stream.meta.audio_selection {
        // mpv audio IDs are one-based; the SDK's audio index is zero-based.
        args.push(format!("--aid={}", u64::from(selection.audio_index) + 1));
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
    // Load `.env` (then `.env.local`) from the repo root when present;
    // real environment variables always win.
    for name in [".env", ".env.local"] {
        let _ = dotenvy::from_filename(name);
    }
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
                // Short-lived tokens (AniKage's ~3-min relay expiry, and
                // similar providers) 400 in the player once stale —
                // refresh them before handing the URL to mpv.
                if app.streams_stale() && app.re_resolve() {
                    continue;
                }
                app.status = play_mpv(terminal, &stream);
            }
        }
        if let Some(message) = app.search.poll() {
            app.status = message;
            if app.search.state == SearchState::Done && !app.search.hits.is_empty() {
                app.focus = Focus::Results;
            }
        }
        if let Some(message) = app.episodes.poll() {
            app.status = message;
        }
        app.poll_answer();
    }
}

/// Render one frame: the query form, (search results,) the stream
/// table, and the status line.
fn draw(frame: &mut Frame, app: &mut App) {
    let [form, table, status] = Layout::vertical([
        Constraint::Length(u16::try_from(FIELDS.len()).unwrap_or(u16::MAX) + 2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    draw_form(frame, app, form);

    // The search results share the stream table's space: shown while the
    // user is picking a title, hidden once a resolve starts.
    let showing_results = !app.search.hits.is_empty() && app.search.state != SearchState::Idle;
    let (results, table) = if showing_results {
        (
            ratatui::layout::Rect::new(
                table.x,
                table.y,
                table.width,
                (table.height / 2).clamp(3, 10),
            ),
            ratatui::layout::Rect::new(
                table.x,
                table.y + (table.height / 2).clamp(3, 10),
                table.width,
                table.height.saturating_sub((table.height / 2).clamp(3, 10)),
            ),
        )
    } else {
        (table, table)
    };
    if showing_results {
        if app.episodes.imdb_id.is_some() && app.focus != Focus::Results {
            draw_episodes(frame, app, results);
        } else {
            draw_results(frame, app, results);
        }
    }
    draw_table(frame, app, table);
    draw_status(frame, app, status);
}

/// The query form: two labeled fields with the active one highlighted.
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
    let block = Block::bordered().title(" vsources — search + resolve ");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// The Cinemeta search results: title, year, `IMDb` id.
fn draw_results(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let spinner = SPINNER
        .chars()
        .nth(app.tick % SPINNER.chars().count())
        .unwrap_or_default();
    let title = match app.search.state {
        SearchState::Loading => format!(" Search {spinner} "),
        _ => " Search results ".to_string(),
    };
    let rows = app
        .search
        .hits
        .iter()
        .enumerate()
        .map(|(index, hit)| {
            let style = if app.search.selection == Some(index) {
                Style::new().reversed()
            } else {
                Style::new()
            };
            Line::from(vec![
                Span::styled(
                    format!(" {:>2} ", index + 1),
                    Style::new().fg(Color::DarkGray),
                ),
                Span::styled(format!("{:<40}", hit.name), style),
                Span::styled(
                    format!(" {:<6}", hit.release),
                    Style::new().fg(Color::DarkGray),
                ),
                Span::styled(hit.imdb_id.clone(), Style::new().fg(Color::DarkGray)),
            ])
        })
        .collect::<Vec<_>>();
    let list = Paragraph::new(rows)
        .block(Block::bordered().title(title))
        .style(if app.focus == Focus::Results {
            Style::new().fg(Color::Yellow)
        } else {
            Style::new()
        });
    frame.render_widget(list, area);
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
                    Focus::Form => "Tab/↑↓ fields · ←→ or m/s kind · Enter search",
                    Focus::Results => "↑↓ select · Enter choose · Tab back to query",
                    Focus::Episodes => "↑↓ episode · Enter resolve · Esc series · Tab streams",
                    Focus::Table => "↑↓ select · Enter/p mpv · Tab results · q quit",
                },
                Style::new().fg(Color::DarkGray),
            ),
        ]
    };
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// A scrollable list ordered and grouped by season, with episode titles and dates.
fn draw_episodes(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let rows = app.episodes.items.iter().map(|ep| {
        Row::new(vec![
            Cell::from(format!("S{:02}E{:02}", ep.season, ep.episode)),
            Cell::from(ep.title.clone()),
            Cell::from(
                ep.released
                    .as_deref()
                    .unwrap_or("")
                    .split('T')
                    .next()
                    .unwrap_or("")
                    .to_string(),
            ),
        ])
    });
    app.episode_table.select(app.episodes.selection);
    let title = if app.episodes.loading {
        " Episodes — loading "
    } else {
        " Episodes — ↑↓ select, Enter resolve "
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Min(20),
            Constraint::Length(12),
        ],
    )
    .row_highlight_style(Style::new().reversed())
    .highlight_symbol("▶ ")
    .block(Block::bordered().title(title));
    frame.render_stateful_widget(table, area, &mut app.episode_table);
}

#[cfg(test)]
mod playback_tests {
    use super::*;
    fn selected_series() -> App {
        let mut app = App::new();
        app.search.hits = vec![cinemeta::SearchHit {
            imdb_id: "tt1234567".into(),
            name: "Series".into(),
            release: "2020-".into(),
            kind: MediaType::Series,
        }];
        app.search.selection = Some(0);
        app.search.mark_fresh();
        app
    }

    #[test]
    fn a_series_requires_an_episode_and_resolves_the_selected_season() {
        let mut app = selected_series();
        assert!(app.media().is_err());
        app.episodes.imdb_id = Some("tt1234567".into());
        app.episodes.items = vec![cinemeta::Episode {
            season: 2,
            episode: 3,
            title: "Return".into(),
            released: Some("2026-01-02".into()),
        }];
        app.episodes.selection = Some(0);
        let media = app.media().unwrap_or_else(|e| panic!("selection: {e}"));
        assert_eq!(media.season, Some(2));
        assert_eq!(media.episode, Some(3));
        assert_eq!(media.id, vsources::MediaId::Imdb("tt1234567".into()));
        app.search.hits[0].imdb_id = "tt7654321".into();
        assert!(
            app.media().is_err(),
            "old episode list must not resolve a different series"
        );
    }

    #[test]
    fn movies_resolve_without_episode_metadata() {
        let mut app = selected_series();
        app.search.hits[0].kind = MediaType::Movie;
        let media = app.media().unwrap_or_else(|e| panic!("movie: {e}"));
        assert_eq!(media.kind, MediaType::Movie);
        assert_eq!(media.season, None);
        assert_eq!(media.episode, None);
    }

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
        stream.meta.audio_selection = Some(vsources::AudioSelection {
            language: vsources::types::CountryCode::En,
            audio_index: 1,
        });
        let args = mpv_args(&stream);
        assert!(args.iter().any(|arg| arg == "--aid=2"));
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
