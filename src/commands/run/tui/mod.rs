//! The in-terminal run UI (ratatui).
//!
//! Renders phases, logs, outcomes, and operator prompts, and feeds operator
//! input back to the run loop. [`state`] holds the state machine; `render`
//! draws each frame.

mod render;
pub mod state;

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::PathBuf;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use execution_engine::ui::{UiComponent, UiRequestData};
use futures::StreamExt;
use ratatui::prelude::CrosstermBackend;
use ratatui::Terminal;
use station_protocol::StationEvent;
use tokio::sync::{broadcast, mpsc};

use state::{ActiveUiRequest, ComponentState, TuiState};

const IMAGE_CACHE_CAPACITY: usize = 32;

/// Hard cap on the failure-memoization set. A pathological procedure
/// that references thousands of distinct broken image paths would
/// otherwise accumulate entries for the whole process lifetime. Once
/// the cap is hit we stop memoizing — the worst that happens is the
/// next unseen broken path gets retried once more than necessary.
const MAX_FAILED_PATHS: usize = 64;

/// Bounded LRU of decoded images + their ratatui-image protocol state.
/// Eviction stops unbounded memory growth on long-running procedures that
/// show many distinct images.
pub struct ImageCache {
    procedure_dir: PathBuf,
    picker: ratatui_image::picker::Picker,
    capacity: usize,
    /// Insertion/access order for LRU eviction. Most-recently-used at back.
    order: VecDeque<String>,
    protocols: HashMap<String, ratatui_image::protocol::StatefulProtocol>,
    /// Paths that failed to open or decode. We remember them so the
    /// render loop doesn't retry the open()+decode() per frame (~60 Hz
    /// of I/O on a permanently-broken image). Cleared only when the
    /// cache is dropped — a missing file at render time is almost
    /// always permanent for this run.
    failed: HashSet<String>,
}

impl ImageCache {
    fn new(procedure_dir: PathBuf) -> Self {
        let picker = ratatui_image::picker::Picker::from_query_stdio()
            .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks());
        Self {
            procedure_dir,
            picker,
            capacity: IMAGE_CACHE_CAPACITY,
            order: VecDeque::new(),
            protocols: HashMap::new(),
            failed: HashSet::new(),
        }
    }

    pub fn get_or_load(
        &mut self,
        path: &str,
    ) -> Option<&mut ratatui_image::protocol::StatefulProtocol> {
        // Normalize the cache key so `./assets/x.png` and `assets/x.png`
        // don't hit the decode path twice. The canonicalized form from
        // disk is our canonical key; if the file doesn't exist we fall
        // back to the raw input (still better than two separate cache
        // entries for the same render call).
        let full_path = self.procedure_dir.join(path);
        let key = full_path
            .canonicalize()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string());

        if self.failed.contains(&key) {
            return None;
        }
        if !self.protocols.contains_key(&key) {
            // `ImageReader::open` infers format from extension. Guess from
            // the actual bytes too so a mislabeled file (e.g. .png that's
            // really a JPEG) still decodes instead of being shelved as
            // "failed" and replaced with the `[Image: path]` fallback.
            let loaded = image::ImageReader::open(&full_path)
                .ok()
                .and_then(|r| r.with_guessed_format().ok())
                .and_then(|r| r.decode().ok());
            let Some(img) = loaded else {
                // Stop memoizing past the cap — better to retry occasionally
                // than to let a pathological procedure leak forever.
                if self.failed.len() < MAX_FAILED_PATHS {
                    self.failed.insert(key);
                }
                return None;
            };
            let protocol = self.picker.new_resize_protocol(img);
            self.protocols.insert(key.clone(), protocol);
            self.order.push_back(key.clone());
            while self.order.len() > self.capacity {
                if let Some(victim) = self.order.pop_front() {
                    self.protocols.remove(&victim);
                }
            }
        } else if let Some(pos) = self.order.iter().position(|p| p == &key) {
            if pos + 1 < self.order.len() {
                let k = self
                    .order
                    .remove(pos)
                    .expect("position() returned Some => index in bounds");
                self.order.push_back(k);
            }
        }
        self.protocols.get_mut(&key)
    }
}

/// Messages produced by the select! loop; fed into the single update entry
/// point (TEA-style). Centralizing event sources here keeps ordering and
/// fairness explicit instead of relying on `try_recv` polling.
enum Msg {
    Key(KeyEvent),
    /// Terminal resized. No state change required — the next `terminal.draw()`
    /// picks up the new geometry — but surfacing as a typed message keeps the
    /// select! arm free of a bare `continue` and lets a reader see the
    /// branch exists.
    Resize,
    InputError(io::Error),
    /// Boxed: `StationEvent` is large (one variant ~360 bytes), so
    /// inlining it would bloat every `Msg` and trip `large_enum_variant`.
    Station(Box<StationEvent>),
    StationLagged(u64),
    StationClosed,
    UiRequest(UiRequestData),
    UiChannelClosed,
    Tick,
}

pub async fn run_tui(
    rx: broadcast::Receiver<StationEvent>,
    ui_rx: mpsc::Receiver<UiRequestData>,
    procedure_dir: PathBuf,
    presence_ctx: Option<PresenceContext>,
    presence_rx: mpsc::Receiver<StationEvent>,
    // The run's cancel token. Ctrl-X writes `cancel()` (graceful) on
    // first press and `kill()` (force) on the second — same surface
    // the dashboard's Stop/Kill button uses, so the keyboard shortcut
    // and the WS button are functionally identical.
    cancel: super::cancel::CancelToken,
) -> crate::error::CliResult<()> {
    install_panic_hook();
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let result = event_loop(
        &mut terminal,
        rx,
        ui_rx,
        procedure_dir,
        presence_ctx,
        presence_rx,
        cancel,
    )
    .await;

    // Unconditional cleanup even if the loop returned Err.
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    result
}

/// Identity + outbound channel the TUI uses to publish its own
/// presence on focus / keystroke. The station-mode caller wires
/// `self_user_id` to the station's installation id and `tx` to the
/// run's event broadcast — the managed publisher already re-publishes
/// broadcast events to the centrifugo status channel, so presence
/// rides the same pipe as phase events. Standalone runs pass `None`
/// and the TUI never publishes.
pub struct PresenceContext {
    pub self_user_id: String,
    pub display_name: String,
    pub color: String,
    pub tx: broadcast::Sender<station_protocol::StationEvent>,
}

/// TUI-local presence state: last-published focus / draft plus a
/// monotonic seq. Publishes on focus change + after a short draft
/// debounce so one publish per keystroke isn't 30 publishes per
/// second on a fast typist.
struct PresenceLocal {
    ctx: Option<PresenceContext>,
    seq: u32,
    focus_req: Option<String>,
    focus_comp: Option<String>,
    draft_value: Option<String>,
    last_publish: Option<tokio::time::Instant>,
    last_keystroke: Option<tokio::time::Instant>,
}

impl PresenceLocal {
    fn new(ctx: Option<PresenceContext>) -> Self {
        Self {
            ctx,
            seq: 0,
            focus_req: None,
            focus_comp: None,
            draft_value: None,
            last_publish: None,
            last_keystroke: None,
        }
    }

    /// Set/clear the focused component. Publishes immediately so the
    /// dashboard sees the ring flip over in lockstep with the TUI
    /// operator's Tab/BackTab navigation.
    async fn set_focus(&mut self, request_id: Option<&str>, component_key: Option<&str>) {
        self.focus_req = request_id.map(str::to_string);
        self.focus_comp = component_key.map(str::to_string);
        // Focus change drops any in-flight draft lock — the new
        // component owns its own draft value starting now.
        self.draft_value = None;
        self.publish_now().await;
    }

    /// Update the draft value. Debounces the publish so rapid
    /// keystrokes don't hammer the broker. Call `tick` from the TUI
    /// tick handler to actually flush pending drafts.
    fn update_draft(&mut self, value: String) {
        self.draft_value = Some(value);
        self.last_keystroke = Some(tokio::time::Instant::now());
    }

    /// Called from the TUI tick. Publishes the draft when enough time
    /// has passed since the last keystroke (debounce) OR when the
    /// heartbeat budget has elapsed since the last publish (so
    /// receivers don't evict us as stale while we're still focused).
    async fn tick(&mut self) {
        let now = tokio::time::Instant::now();
        let has_focus_or_draft = self.focus_req.is_some() || self.draft_value.is_some();
        if !has_focus_or_draft {
            return;
        }
        let debounce = std::time::Duration::from_millis(120);
        let heartbeat = std::time::Duration::from_millis(2_500);
        let debounced_due = self
            .last_keystroke
            .map(|k| now.duration_since(k) >= debounce)
            .unwrap_or(false)
            && self
                .last_publish
                .map(|p| p < self.last_keystroke.expect("keystroke set"))
                .unwrap_or(true);
        let heartbeat_due = self
            .last_publish
            .map(|p| now.duration_since(p) >= heartbeat)
            .unwrap_or(true);
        if debounced_due || heartbeat_due {
            self.publish_now().await;
        }
    }

    async fn publish_now(&mut self) {
        let Some(ctx) = self.ctx.as_ref() else { return };
        self.seq = self.seq.wrapping_add(1);
        let focus = match (&self.focus_req, &self.focus_comp) {
            (Some(r), Some(c)) => Some(station_protocol::PresenceFocus {
                request_id: r.clone(),
                component_key: c.clone(),
            }),
            _ => None,
        };
        let draft = match (&self.focus_req, &self.focus_comp, &self.draft_value) {
            (Some(r), Some(c), Some(v)) => Some(station_protocol::PresenceDraft {
                request_id: r.clone(),
                component_key: c.clone(),
                value: v.clone(),
                cursor_pos: None,
            }),
            _ => None,
        };
        let updated_at: u32 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0);
        let event = station_protocol::StationEvent::Presence(station_protocol::PresencePayload {
            user_id: ctx.self_user_id.clone(),
            display_name: ctx.display_name.clone(),
            color: ctx.color.clone(),
            focus,
            draft,
            seq: self.seq,
            updated_at,
        });
        // Broadcast send is non-async; the managed publisher task
        // downstream drains at its own pace. `send` only errors when
        // there are no receivers — which means no one's listening,
        // not a real failure, so we drop silently.
        let _ = ctx.tx.send(event);
        self.last_publish = Some(tokio::time::Instant::now());
    }
}

/// Register a panic hook that restores the terminal and then chains the
/// existing hook so color_eyre / better_panic / default backtraces still
/// fire. Follows the ratatui panic-hook recipe.
fn install_panic_hook() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(info);
    }));
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut rx: broadcast::Receiver<StationEvent>,
    mut ui_rx: mpsc::Receiver<UiRequestData>,
    procedure_dir: PathBuf,
    presence_ctx: Option<PresenceContext>,
    mut presence_rx: mpsc::Receiver<StationEvent>,
    cancel: super::cancel::CancelToken,
) -> crate::error::CliResult<()> {
    let mut app = TuiState::new();
    let mut images = ImageCache::new(procedure_dir);
    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(crate::config::timeouts::TUI_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    if let Some(ref ctx) = presence_ctx {
        app.self_user_id = Some(ctx.self_user_id.clone());
    }
    let mut presence = PresenceLocal::new(presence_ctx);

    let mut grace_deadline: Option<tokio::time::Instant> = None;
    let mut station_closed = false;

    loop {
        terminal.draw(|f| render::draw(f, &app, &mut images))?;

        // Start the grace countdown once the run is complete: gives the
        // operator a moment to read the final frame before tear-down.
        if app.done && grace_deadline.is_none() {
            grace_deadline =
                Some(tokio::time::Instant::now() + crate::config::timeouts::TUI_CLOSE_GRACE);
        }

        // One-shot: the lag warning is visible for exactly one frame.
        app.lag_warning = None;

        let grace_sleep = async {
            match grace_deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending::<()>().await,
            }
        };

        let msg = tokio::select! {
            biased;

            _ = grace_sleep => break,

            maybe_input = input.next() => match maybe_input {
                // Filter on `KeyEventKind::Press`. Windows ConPTY reports
                // both Press and Release for every keystroke (and Repeat
                // for held keys); Unix ttys only emit Press. Without this
                // guard every key registered twice on Windows: the
                // operator typed `1`, the TUI saw `11`. Linux/macOS were
                // unaffected because Release/Repeat never fire there.
                Some(Ok(Event::Key(k))) if k.kind == KeyEventKind::Press => Msg::Key(k),
                Some(Ok(Event::Key(_))) => continue,
                Some(Ok(Event::Resize(_, _))) => Msg::Resize,
                // Mouse / focus / paste events are ignored. This TUI is
                // keyboard-driven; resize is handled above.
                Some(Ok(_)) => continue,
                Some(Err(e)) => Msg::InputError(e),
                None => {
                    // EventStream ended (stdin closed). Not recoverable.
                    break;
                }
            },

            res = rx.recv(), if !station_closed => match res {
                Ok(ev) => Msg::Station(Box::new(ev)),
                Err(broadcast::error::RecvError::Lagged(n)) => Msg::StationLagged(n),
                Err(broadcast::error::RecvError::Closed) => {
                    station_closed = true;
                    Msg::StationClosed
                }
            },

            res = ui_rx.recv() => match res {
                Some(req) => Msg::UiRequest(req),
                None => Msg::UiChannelClosed,
            },

            // Inbound presence from other participants (dashboard
            // tabs). Same shape as Station events, different source so
            // the managed publisher doesn't pick it up and re-publish.
            Some(ev) = presence_rx.recv() => Msg::Station(Box::new(ev)),

            _ = tick.tick() => Msg::Tick,
        };

        // Handle presence side-effects before handing to `update` so
        // the focus/draft publish sees the *new* UI state rather than
        // the pre-keypress snapshot. `sync_presence` inspects the
        // current focused component + its string value and decides
        // whether to publish.
        let presence_action = derive_presence_action(&app, &msg);

        if update(&mut app, msg, &cancel).await {
            break;
        }

        match presence_action {
            PresenceAction::None => {}
            PresenceAction::ClearFocus => {
                presence.set_focus(None, None).await;
            }
            PresenceAction::SetFocus => {
                let (req, comp) = current_focus(&app);
                presence.set_focus(req.as_deref(), comp.as_deref()).await;
            }
            PresenceAction::UpdateDraft => {
                if let Some(value) = current_draft_value(&app) {
                    presence.update_draft(value);
                }
            }
        }
        presence.tick().await;
    }

    Ok(())
}

/// Classify each TUI message for presence side-effects. Keeps the
/// publish-routing out of `update`'s body so the reducer stays a pure
/// function of (state, event) without picking up an async
/// publish-side-effect tail.
enum PresenceAction {
    None,
    /// Component focus changed (Tab/BackTab/UI request appeared).
    SetFocus,
    /// Any focus we had is gone (UI dismissed, run complete, etc.).
    ClearFocus,
    /// Focused component's value changed (keystroke on text/number/
    /// textarea input). Publish after tick debounce.
    UpdateDraft,
}

fn derive_presence_action(app: &TuiState, msg: &Msg) -> PresenceAction {
    match msg {
        Msg::Key(k) => match k.code {
            KeyCode::Tab | KeyCode::BackTab => PresenceAction::SetFocus,
            KeyCode::Enter | KeyCode::Esc => {
                // These dismiss or submit the UI; defer to clear focus
                // on the next frame if the UI actually cleared.
                PresenceAction::ClearFocus
            }
            _ => {
                // Any text-bearing keystroke while focused on a text
                // component should refresh the draft. Filter non-text
                // focuses so slider arrows don't bleed into draft
                // publishes.
                if focused_is_text_input(app) {
                    PresenceAction::UpdateDraft
                } else {
                    PresenceAction::None
                }
            }
        },
        Msg::UiRequest(_) => PresenceAction::SetFocus,
        Msg::StationClosed => PresenceAction::ClearFocus,
        _ => PresenceAction::None,
    }
}

fn focused_is_text_input(app: &TuiState) -> bool {
    let Some(ui) = app.active_ui.as_ref() else {
        return false;
    };
    let Some((_comp, state)) = ui.focused_pair() else {
        return false;
    };
    matches!(
        state,
        ComponentState::Text(_) | ComponentState::Number(_) | ComponentState::Textarea(_)
    )
}

fn current_focus(app: &TuiState) -> (Option<String>, Option<String>) {
    let Some(ui) = app.active_ui.as_ref() else {
        return (None, None);
    };
    let request_id = Some(ui.request_id.clone());
    let component_key = ui.focused_pair().map(|(comp, _)| comp.key.clone());
    (request_id, component_key)
}

fn current_draft_value(app: &TuiState) -> Option<String> {
    let (_comp, state) = app.active_ui.as_ref()?.focused_pair()?;
    match state {
        ComponentState::Text(s) | ComponentState::Number(s) | ComponentState::Textarea(s) => {
            Some(s.clone())
        }
        _ => None,
    }
}

/// Single update entry point (TEA-style). All state mutation flows through
/// here so the draw loop sees a consistent snapshot each frame.
async fn update(app: &mut TuiState, msg: Msg, cancel: &super::cancel::CancelToken) -> bool {
    match msg {
        Msg::Tick => {
            // Display-only prompts (`requires_input: false`) used to
            // auto-continue 2 seconds after the request landed. That
            // raced phases that mutated the prompt for longer than 2s
            // via `ui.<key> = value`, dismissing the screen mid-stream
            // and freezing the operator on stale content. Match Studio
            // semantics: phase_complete is the only correct dismissal
            // anchor (see `state.rs` PhaseComplete arm).
            //
            // Tick is the right cadence for dropping stale presence
            // badges — the draw after this will re-render the UI panel
            // with any evicted entries gone, no extra frame.
            app.evict_stale_presence();
        }

        Msg::Key(key) => return handle_key(app, key, cancel).await,

        Msg::Resize => {
            // Explicit no-op: `terminal.draw()` at the top of the next
            // loop iteration reads the new viewport size.
        }

        Msg::InputError(e) => {
            crate::log::error(&format!("TUI input error: {e}"));
            return true;
        }

        Msg::Station(event) => {
            app.apply(*event);
        }

        Msg::StationLagged(n) => {
            app.lag_warning = Some(n);
        }

        Msg::StationClosed => {
            app.done = true;
            return false;
        }

        Msg::UiRequest(req) => app.set_ui_request(&req),

        Msg::UiChannelClosed => {
            // No more UI requests will arrive. Not fatal on its own — the
            // run may still complete via the broadcast channel.
        }
    }
    false
}

async fn handle_key(
    app: &mut TuiState,
    key: KeyEvent,
    cancel: &super::cancel::CancelToken,
) -> bool {
    let awaiting_input = matches!(
        app.active_ui.as_ref(),
        Some(ui) if ui.requires_input && !ui.submitted
    );

    // Global quit / auto-continue dismiss happen regardless of focus.
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
        // Ctrl-X = abort. First press calls `cancel()` (graceful);
        // second press escalates via `kill()` (force). Mirrors the web
        // operator-UI button morph. Allowed even while a UI prompt
        // is awaiting input — the operator should always be able to
        // bail out of a hung prompt.
        KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if app.stop_pressed {
                cancel.kill();
            } else {
                app.stop_pressed = true;
                cancel.cancel();
            }
            return false;
        }
        KeyCode::Char('q') if !awaiting_input => return true,
        _ => {}
    }

    if awaiting_input {
        handle_ui_key(app, key).await;
        return false;
    }

    // Display-only UI: any Enter dismisses it immediately (skip the timer).
    if let Some(ref ui) = app.active_ui {
        if !ui.requires_input && matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) {
            let request_id = ui.request_id.clone();
            let values = ui.collect_response();
            super::ui_response::send(&request_id, values).await;
            app.active_ui = None;
            app.promote_pending_ui();
            return false;
        }
    }

    if app.done {
        return true;
    }
    false
}

async fn handle_ui_key(app: &mut TuiState, key: KeyEvent) {
    let Some(ui) = app.active_ui.as_mut() else {
        return;
    };

    match key.code {
        KeyCode::Esc => {
            let request_id = ui.request_id.clone();
            super::ui_response::send_empty(&request_id).await;
            app.active_ui = None;
            app.promote_pending_ui();
        }
        KeyCode::Tab => ui.focus_next(),
        KeyCode::BackTab => ui.focus_prev(),
        KeyCode::Enter => {
            if ui.validate() {
                let request_id = ui.request_id.clone();
                let values = ui.collect_response();
                super::ui_response::send(&request_id, values).await;
                app.active_ui = None;
                app.promote_pending_ui();
            }
        }
        _ => dispatch_focused(ui, key),
    }
}

/// Dispatch keys to the per-component handler. Docs (Component pattern)
/// prescribe co-locating input handling with each component; we don't have
/// the trait machinery but the per-type fns give the same isolation.
fn dispatch_focused(ui: &mut ActiveUiRequest, key: KeyEvent) {
    let Some((comp, state)) = ui.focused_mut() else {
        return;
    };
    match state {
        ComponentState::Text(s) => handle_text_key(s, comp, key),
        ComponentState::Number(s) => handle_number_key(s, comp, key),
        ComponentState::Textarea(s) => handle_textarea_key(s, comp, key),
        ComponentState::Switch(b) => handle_switch_key(b, key),
        ComponentState::Slider(v) => handle_slider_key(v, comp, key),
        ComponentState::SingleChoice { value, cursor } => {
            handle_single_choice_key(value, cursor, comp, key)
        }
        ComponentState::MultiChoice { selected, cursor } => {
            handle_multi_choice_key(selected, cursor, comp, key)
        }
        ComponentState::Display => {}
    }
}

fn handle_text_key(s: &mut String, comp: &UiComponent, key: KeyEvent) {
    match key.code {
        KeyCode::Backspace => {
            s.pop();
        }
        KeyCode::Char(c) => {
            if let Some(max) = comp.max_length {
                if s.chars().count() >= max as usize {
                    return;
                }
            }
            s.push(c);
        }
        _ => {}
    }
}

fn handle_number_key(s: &mut String, comp: &UiComponent, key: KeyEvent) {
    match key.code {
        KeyCode::Backspace => {
            s.pop();
        }
        KeyCode::Char(c) => {
            // Accept a single leading '-', at most one '.', and digits only.
            if c == '-' {
                if !s.is_empty() {
                    return;
                }
            } else if c == '.' {
                if s.contains('.') {
                    return;
                }
            } else if !c.is_ascii_digit() {
                return;
            }
            if let Some(max) = comp.max_length {
                if s.chars().count() >= max as usize {
                    return;
                }
            }
            s.push(c);
        }
        _ => {}
    }
}

fn handle_textarea_key(s: &mut String, comp: &UiComponent, key: KeyEvent) {
    match key.code {
        KeyCode::Backspace => {
            s.pop();
        }
        KeyCode::Char(c) => {
            if let Some(max) = comp.max_length {
                if s.chars().count() >= max as usize {
                    return;
                }
            }
            s.push(c);
        }
        _ => {}
    }
}

fn handle_switch_key(b: &mut bool, key: KeyEvent) {
    if matches!(
        key.code,
        KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right
    ) {
        *b = !*b;
    }
}

fn handle_slider_key(v: &mut f64, comp: &UiComponent, key: KeyEvent) {
    let step = comp.step.unwrap_or(1.0);
    let min = comp.min.unwrap_or(0.0);
    let max = comp.max.unwrap_or(100.0);
    match key.code {
        KeyCode::Up | KeyCode::Right => *v = (*v + step).min(max),
        KeyCode::Down | KeyCode::Left => *v = (*v - step).max(min),
        _ => {}
    }
}

fn handle_single_choice_key(
    value: &mut Option<String>,
    cursor: &mut usize,
    comp: &UiComponent,
    key: KeyEvent,
) {
    let Some(options) = comp.options.as_ref() else {
        return;
    };
    if options.is_empty() {
        return;
    }
    match key.code {
        KeyCode::Up => {
            if *cursor > 0 {
                *cursor -= 1;
            }
        }
        KeyCode::Down => {
            if *cursor + 1 < options.len() {
                *cursor += 1;
            }
        }
        KeyCode::Char(' ') | KeyCode::Enter => {
            // Enter is intercepted upstream for submit; we never see it here
            // unless the dispatcher routes it through. Space selects.
            if let Some(opt) = options.get(*cursor) {
                *value = Some(opt.value.clone());
            }
        }
        _ => {}
    }
}

fn handle_multi_choice_key(
    selected: &mut Vec<String>,
    cursor: &mut usize,
    comp: &UiComponent,
    key: KeyEvent,
) {
    let Some(options) = comp.options.as_ref() else {
        return;
    };
    if options.is_empty() {
        return;
    }
    match key.code {
        KeyCode::Up => {
            if *cursor > 0 {
                *cursor -= 1;
            }
        }
        KeyCode::Down => {
            if *cursor + 1 < options.len() {
                *cursor += 1;
            }
        }
        KeyCode::Char(' ') => {
            if let Some(opt) = options.get(*cursor) {
                if let Some(pos) = selected.iter().position(|v| v == &opt.value) {
                    selected.remove(pos);
                } else {
                    selected.push(opt.value.clone());
                }
            }
        }
        _ => {}
    }
}
