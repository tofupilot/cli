//! Frame rendering for the TUI: draws phases, logs, operator prompts, and remote
//! presence into the ratatui terminal backend.

use execution_engine::ui::{ComponentType, ComponentValue, UiComponent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap},
    Frame,
};
use ratatui_image::StatefulImage;
use station_protocol::RunMeasurement;

use super::state::{ActiveUiRequest, ComponentState, PhaseStatus, TuiState};
use super::ImageCache;

// Status colors lifted from `apps/studio/app/lib/measurements/outcome-utils.ts`
// so the TUI matches Studio/dashboard semantics. Tailwind-500 shades.
mod palette {
    use ratatui::style::Color;
    pub const PASS: Color = Color::Rgb(132, 204, 22); // lime-500
    pub const FAIL: Color = Color::Rgb(236, 72, 153); // pink-500
    pub const ERROR: Color = Color::Rgb(239, 68, 68); // red-500
    pub const RUNNING: Color = Color::Rgb(59, 130, 246); // blue-500
    pub const MUTED: Color = Color::Rgb(113, 113, 122); // zinc-500
    pub const SUBTLE: Color = Color::Rgb(63, 63, 70); // zinc-700 — pending segment track
    pub const WARN: Color = Color::Rgb(249, 115, 22); // orange-500
    pub const TIMEOUT: Color = Color::Rgb(249, 115, 22); // orange-500 — same hue as web TIMEOUT
    pub const ABORTED: Color = Color::Rgb(234, 179, 8); // yellow-500 — same hue as web ABORTED
}

pub fn draw(f: &mut Frame, state: &TuiState, image_cache: &mut ImageCache) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(2), // brand (cap + body)
            Constraint::Length(1), // gap under header
            Constraint::Min(4),    // content
            Constraint::Length(3), // progress (label row + gap + segment strip)
            Constraint::Length(1), // footer
        ])
        .split(f.area());

    draw_header(f, state, chunks[0]);

    if state.run_error.is_some() {
        // A crash diagnostic supersedes everything: the run never
        // produced phases, so the table would be empty and confusing.
        // Render the error blurb in the main panel instead.
        draw_crash_panel(f, state, chunks[2]);
    } else if state.active_ui.is_some() {
        draw_ui_panel(f, state, chunks[2], image_cache);
    } else {
        draw_phase_list(f, state, chunks[2]);
    }

    draw_progress(f, state, chunks[3]);
    draw_footer(f, state, chunks[4]);
}

fn draw_header(f: &mut Frame, state: &TuiState, area: Rect) {
    // Same brand mark as the station banner: cap on top (╭ ✈︎ ╮), body
    // on bottom ([•ᴗ•]). See `show_banner` in station/mod.rs.
    let blue = Style::default().fg(Color::Rgb(59, 130, 246));
    let gold = Style::default().fg(Color::Rgb(234, 179, 8));

    // Match station banner spacing: cap is " ╭ ✈︎ ╮", body is " [•ᴗ•]".
    // That keeps the plane visually centered over the ᴗ glyph.
    let cap = Line::from(vec![
        Span::styled(" \u{256d} ", blue),
        Span::styled("\u{2708}\u{fe0e}", gold),
        Span::styled(" \u{256e}", blue),
    ]);
    let body = Line::from(vec![
        Span::raw(" [\u{2022}\u{1d17}\u{2022}]  "),
        Span::styled(
            &state.procedure_id,
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]);

    f.render_widget(Paragraph::new(vec![cap, body]), area);
}

fn draw_phase_list(f: &mut Frame, state: &TuiState, area: Rect) {
    // Columns mirror the web phase/measurement table in
    // `apps/web/components/streaming/phases/phase-measurement-table.tsx`:
    //   Time | Name | Outcome | Value/Unit | Validators
    // Phase rows carry the time; indented measurement rows under each
    // phase leave Time empty.
    let header = Row::new(vec![
        Cell::from(" Time"),
        Cell::from("Name"),
        Cell::from("Outcome"),
        Cell::from("Value / Unit"),
        Cell::from("Validators"),
    ])
    .style(Style::default().fg(palette::MUTED))
    .height(1);

    let mut rows: Vec<Row> = Vec::new();
    for phase in &state.phases {
        rows.push(phase_row(state, phase));
        for m in &phase.measurements {
            rows.push(measurement_row(m));
        }
    }

    let widths = [
        Constraint::Length(9),  // Time " mm:ss.s"
        Constraint::Min(18),    // Name
        Constraint::Length(8),  // Outcome
        Constraint::Length(16), // Value / Unit
        Constraint::Min(18),    // Validators
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(2)
        .block(Block::default().borders(Borders::TOP).title("Phases"));
    f.render_widget(table, area);
}

fn phase_row<'a>(state: &TuiState, phase: &'a super::state::PhaseState) -> Row<'a> {
    let outcome_text = match phase.status {
        PhaseStatus::Pending => "PENDING",
        PhaseStatus::Running => "RUNNING",
        PhaseStatus::Pass => "PASS",
        PhaseStatus::Fail => "FAIL",
        PhaseStatus::Skip => "SKIP",
        PhaseStatus::Error => "ERROR",
        PhaseStatus::Timeout => "TIMEOUT",
        PhaseStatus::Aborted => "ABORTED",
    };
    let outcome_color = match phase.status {
        PhaseStatus::Pending => palette::MUTED,
        PhaseStatus::Running => palette::RUNNING,
        PhaseStatus::Pass => palette::PASS,
        PhaseStatus::Fail => palette::FAIL,
        PhaseStatus::Skip => palette::MUTED,
        PhaseStatus::Error => palette::ERROR,
        PhaseStatus::Timeout => palette::TIMEOUT,
        PhaseStatus::Aborted => palette::ABORTED,
    };

    let time_str = phase
        .started_at
        .zip(state.run_started_at)
        .map(|(phase_t, run_t)| format_offset(phase_t.saturating_duration_since(run_t)))
        .unwrap_or_default();

    Row::new(vec![
        Cell::from(Span::styled(time_str, Style::default().fg(palette::MUTED))),
        Cell::from(Span::styled(
            phase.name.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Cell::from(Span::styled(
            outcome_text,
            Style::default().fg(outcome_color),
        )),
        Cell::from(""),
        Cell::from(""),
    ])
}

fn measurement_row(m: &RunMeasurement) -> Row<'_> {
    let (outcome_color, _icon) = match m.outcome.as_str() {
        "PASS" => (palette::PASS, "✓"),
        "FAIL" => (palette::FAIL, "✗"),
        _ => (palette::MUTED, "·"),
    };

    let value_span = format_measurement_value(m);

    let validator_spans = build_validator_spans(m);

    Row::new(vec![
        Cell::from(""),
        Cell::from(Span::raw(format!("  {}", m.name))),
        Cell::from(Span::styled(
            m.outcome.as_str(),
            Style::default().fg(outcome_color),
        )),
        Cell::from(value_span),
        Cell::from(Line::from(validator_spans)),
    ])
}

fn format_measurement_value(m: &RunMeasurement) -> Span<'_> {
    let val = m.measured_value.as_ref();
    let txt = match val {
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        Some(_) => "…".to_string(),
        None => String::new(),
    };
    if txt.is_empty() {
        return Span::raw("");
    }
    let suffix = m
        .units
        .as_deref()
        .map(|u| format!(" {u}"))
        .unwrap_or_default();
    Span::styled(
        format!("{txt}{suffix}"),
        Style::default().fg(palette::MUTED),
    )
}

/// Mirror web's `ValidatorBadge` palette in terminal paint:
///   * pink+bold when the validator failed decisively,
///   * muted gray for passing / indicative / unknown.
fn build_validator_spans(m: &RunMeasurement) -> Vec<Span<'static>> {
    if m.validators.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<Span<'static>> = Vec::new();
    for (i, v) in m.validators.iter().enumerate() {
        if i > 0 {
            out.push(Span::raw("  "));
        }
        let decisive = v.is_decisive.unwrap_or(true);
        let failing = v.outcome == "FAIL";
        let style = if failing && decisive {
            Style::default().fg(palette::FAIL)
        } else if !decisive {
            Style::default()
                .fg(palette::MUTED)
                .add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(palette::MUTED)
        };
        out.push(Span::styled(v.expression.clone(), style));
    }
    out
}

fn format_offset(d: std::time::Duration) -> String {
    // `mm:ss.s` — compact enough for a narrow column, matching the web
    // table's relative time column.
    let secs = d.as_secs();
    let mins = secs / 60;
    let s = secs % 60;
    let tenths = d.subsec_millis() / 100;
    format!(" {mins:02}:{s:02}.{tenths}")
}

fn draw_crash_panel(f: &mut Frame, state: &TuiState, area: Rect) {
    // We only land here when `state.run_error` is Some, but keep the
    // unwrap_or in case something flips between draw and event apply.
    let err = match state.run_error.as_ref() {
        Some(e) => e,
        None => return,
    };
    let kind_label = err.kind.replace('_', " ").to_uppercase();
    let mut lines: Vec<Line<'_>> = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("  {kind_label}"),
        Style::default()
            .fg(palette::ERROR)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    for raw_line in err.message.lines() {
        // Indent each line so the body sits inside the framed block.
        lines.push(Line::from(format!("  {raw_line}")));
    }
    if err.message.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no diagnostic provided)",
            Style::default().fg(palette::MUTED),
        )));
    }
    let block = Block::default().borders(Borders::TOP).title(Span::styled(
        "Run failed",
        Style::default()
            .fg(palette::ERROR)
            .add_modifier(Modifier::BOLD),
    ));
    // `trim: false` preserves leading indentation we added so wrapped
    // continuation lines line up with the original. Without wrap the
    // long single-line YAML diagnostics get clipped to the panel width.
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

// Mirrors the web app's `OperatorProgressBar`: top row shows elapsed
// time on the left and percent complete on the right; bottom row is
// a one-character-tall segmented strip with one segment per phase
// coloured by the phase's status. Width-equal segments so progression
// reads at a glance.
fn draw_progress(f: &mut Frame, state: &TuiState, area: Rect) {
    let total = state.total_count().max(1);
    let completed = state.completed_count();
    let pct = (completed as f64 / total as f64 * 100.0).round() as u32;

    // Elapsed: anchor on `run_started_at`. Freezes at `run_ended_at`
    // when the run reaches a terminal state — matches the web ticker's
    // freeze-on-endedAt behaviour.
    let elapsed_ms = state
        .run_started_at
        .map(|t| {
            let end = state.run_ended_at.unwrap_or_else(std::time::Instant::now);
            end.saturating_duration_since(t).as_millis() as u64
        })
        .unwrap_or(0);
    let elapsed = format_elapsed(elapsed_ms);

    // 1-cell horizontal padding mirrors the web app's `px-8` block —
    // segments and label don't sit flush against the edges. Vertical
    // layout: label / gap / segments, also matching the web's
    // `mb-2` between the label row and the strip.
    let padded = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area)[1];
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // label
            Constraint::Length(1), // gap
            Constraint::Length(1), // segments
        ])
        .split(padded);

    // Label row: time on left, percent on right. Mono-styled to match
    // the web app's `font-mono tabular-nums` block.
    let label = Line::from(vec![
        Span::styled(elapsed.main.clone(), Style::default().fg(palette::MUTED)),
        Span::styled(
            format!(".{}", elapsed.cs),
            Style::default()
                .fg(palette::MUTED)
                .add_modifier(Modifier::DIM),
        ),
    ]);
    // `X / Y` in dim muted (same shade as the timer's centiseconds)
    // sits to the left of the percent so the operator can see both
    // the count and the percentage at a glance.
    let pct_line = Line::from(vec![
        Span::styled(
            format!("{completed}/{total} "),
            Style::default()
                .fg(palette::MUTED)
                .add_modifier(Modifier::DIM),
        ),
        Span::styled(format!("{pct}%"), Style::default().fg(palette::MUTED)),
    ])
    .alignment(ratatui::layout::Alignment::Right);
    f.render_widget(Paragraph::new(label), rows[0]);
    f.render_widget(Paragraph::new(pct_line), rows[0]);

    // Segments row. Constraint::Ratio interleaved with Length(1) gaps
    // drifts because the gaps consume cells before the ratio
    // divides what's left — phases end up with visibly different
    // widths. Manual integer split: floor(usable / n) per segment,
    // 1-cell gaps between, leftover cells parked at the end so all
    // phases get IDENTICAL widths.
    if state.phases.is_empty() {
        return;
    }
    let seg_count = state.phases.len() as u16;
    let total = rows[2].width;
    let gaps = seg_count.saturating_sub(1);
    let usable = total.saturating_sub(gaps);
    let seg_width = usable / seg_count;
    let leftover = usable.saturating_sub(seg_width * seg_count);

    let mut constraints: Vec<Constraint> = Vec::with_capacity((seg_count * 2) as usize);
    for i in 0..seg_count {
        constraints.push(Constraint::Length(seg_width));
        if i + 1 < seg_count {
            constraints.push(Constraint::Length(1));
        }
    }
    if leftover > 0 {
        constraints.push(Constraint::Length(leftover));
    }
    let cells = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(rows[2]);
    for (i, phase) in state.phases.iter().enumerate() {
        let color = match phase.status {
            super::state::PhaseStatus::Pass => palette::PASS,
            super::state::PhaseStatus::Fail => palette::FAIL,
            super::state::PhaseStatus::Error => palette::ERROR,
            super::state::PhaseStatus::Timeout => palette::TIMEOUT,
            super::state::PhaseStatus::Aborted => palette::ABORTED,
            super::state::PhaseStatus::Skip => palette::MUTED,
            super::state::PhaseStatus::Running => palette::RUNNING,
            super::state::PhaseStatus::Pending => palette::SUBTLE,
        };
        // Cells are [seg0, gap, seg1, gap, …, segN-1, (leftover)]
        // so the i-th phase lives at `cells[i * 2]`.
        let area = cells[i * 2];
        // Solid block char so the segment renders as a filled bar
        // even on terminals that strip background colour.
        let bar = "█".repeat(area.width as usize);
        let widget = Paragraph::new(Span::styled(bar, Style::default().fg(color)));
        f.render_widget(widget, area);
    }
}

struct ElapsedParts {
    main: String,
    cs: String,
}

fn format_elapsed(ms: u64) -> ElapsedParts {
    let total_sec = ms / 1000;
    let cs = ((ms % 1000) / 10) as u32;
    let h = total_sec / 3600;
    let m = (total_sec % 3600) / 60;
    let s = total_sec % 60;
    let main = if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    };
    ElapsedParts {
        main,
        cs: format!("{cs:02}"),
    }
}

fn draw_footer(f: &mut Frame, state: &TuiState, area: Rect) {
    let footer_text = if let Some(n) = state.lag_warning {
        Line::from(Span::styled(
            format!("  ⚠ dropped {n} event(s) — consumer fell behind"),
            Style::default().fg(palette::WARN),
        ))
    } else if let Some(ref ui) = state.active_ui {
        if ui.submitted {
            Line::from(Span::styled(
                "  Submitted. Waiting for engine...",
                Style::default().fg(palette::PASS),
            ))
        } else {
            Line::from(shortcut_hints(ui))
        }
    } else if let Some(ref outcome) = state.outcome {
        let color = match outcome.as_str() {
            super::super::outcomes::PASS => palette::PASS,
            super::super::outcomes::ERROR => palette::ERROR,
            super::super::outcomes::TIMEOUT => palette::TIMEOUT,
            super::super::outcomes::ABORTED => palette::ABORTED,
            _ => palette::FAIL,
        };
        Line::from(Span::styled(
            format!("  {outcome}"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ))
    } else {
        // Run in flight, no UI prompt — surface the abort shortcut.
        // Morphs after the first press: Stop on first Ctrl-X, Kill on
        // the second (matches the web button's progressive escalation).
        Line::from(run_shortcut_hints(state.stop_pressed))
    };
    f.render_widget(Paragraph::new(footer_text), area);
}

/// Footer hints surfaced while the run is in flight (no UI prompt).
/// First press of Ctrl-X publishes `Stop`; the hint then morphs to
/// `Kill` so the operator sees that a second press will escalate to
/// the force path. Style mirrors `shortcut_hints`: primary action
/// blue+bold, descriptor muted.
fn run_shortcut_hints(stop_pressed: bool) -> Vec<Span<'static>> {
    let primary = Style::default()
        .fg(palette::RUNNING)
        .add_modifier(Modifier::BOLD);
    let danger = Style::default()
        .fg(palette::FAIL)
        .add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(palette::MUTED);
    if stop_pressed {
        vec![
            Span::raw("  "),
            Span::styled("Ctrl-X", danger),
            Span::styled(" force kill   ", muted),
            Span::styled("Stopping...", muted),
        ]
    } else {
        vec![
            Span::raw("  "),
            Span::styled("Ctrl-X", primary),
            Span::styled(" stop   ", muted),
            Span::styled("Ctrl-C", muted),
            Span::styled(" quit", muted),
        ]
    }
}

/// Build the footer hint row for an active UI. Order: Space > Tab > Enter > Esc.
/// The shortcut matching the next meaningful action is highlighted blue;
/// others are muted. Shortcuts that don't apply (Tab with one field, Space
/// on text-only forms) are omitted.
fn shortcut_hints(ui: &ActiveUiRequest) -> Vec<Span<'static>> {
    let primary = Style::default()
        .fg(palette::RUNNING)
        .add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(palette::MUTED);
    let mut spans: Vec<Span<'static>> = vec![Span::raw("  ")];

    if !ui.requires_input {
        spans.push(Span::styled("Enter", primary));
        spans.push(Span::styled(" continue   ", muted));
        spans.push(Span::styled("(auto in 2s)", muted));
        return spans;
    }

    let focused_space_action = ui
        .components
        .get(ui.focused_index)
        .and_then(space_action_label);
    let inputs: Vec<usize> = ui.input_indices().collect();
    let multi_field = inputs.len() > 1;
    // Is focus on the last input? If yes, there's no "next field" to move
    // to — Tab would wrap back to the start, which isn't the natural next
    // action; keep Enter primary in that case.
    let focused_is_last = inputs.last().copied() == Some(ui.focused_index);
    let focused_filled = ui
        .states
        .get(ui.focused_index)
        .map(has_value)
        .unwrap_or(false);

    // Priority: Space (if a choice/switch is focused and not yet acted on) →
    // Tab (user filled the current field and there's a next one) → Enter.
    let space_primary = focused_space_action.is_some() && !focused_filled;
    let tab_primary = !space_primary && multi_field && !focused_is_last && focused_filled;
    let enter_primary = !space_primary && !tab_primary;

    let mut first = true;
    let mut push = |spans: &mut Vec<Span<'static>>, label: Span<'static>, desc: Span<'static>| {
        if !first {
            spans.push(Span::raw("   "));
        }
        first = false;
        spans.push(label);
        spans.push(desc);
    };

    if let Some(hint) = focused_arrow_hint(ui) {
        push(
            &mut spans,
            Span::styled(hint.keys, muted),
            Span::styled(format!(" {}", hint.desc), muted),
        );
    }
    if let Some(label) = focused_space_action {
        push(
            &mut spans,
            Span::styled("Space", if space_primary { primary } else { muted }),
            Span::styled(format!(" {label}"), muted),
        );
    }
    if multi_field {
        push(
            &mut spans,
            Span::styled("Tab", if tab_primary { primary } else { muted }),
            Span::styled(" next field", muted),
        );
    }
    push(
        &mut spans,
        Span::styled("Enter", if enter_primary { primary } else { muted }),
        Span::styled(" submit", muted),
    );
    push(
        &mut spans,
        Span::styled("Esc", muted),
        Span::styled(" cancel", muted),
    );

    spans
}

/// Has the user meaningfully acted on this field? Used to decide when Tab
/// becomes the primary shortcut (after the current field is filled). Switch
/// and Slider always report true — their defaults are a valid value — so
/// Tab takes over immediately once the user tabs onto them.
fn has_value(state: &ComponentState) -> bool {
    match state {
        ComponentState::Text(s) | ComponentState::Number(s) | ComponentState::Textarea(s) => {
            !s.trim().is_empty()
        }
        ComponentState::SingleChoice { value, .. } => value.is_some(),
        ComponentState::MultiChoice { selected, .. } => !selected.is_empty(),
        ComponentState::Switch(_) | ComponentState::Slider(_) => true,
        ComponentState::Display => false,
    }
}

struct ArrowHint {
    keys: &'static str,
    desc: &'static str,
}

/// Surface arrow-key hints for the focused component. Slider reacts to
/// both axes; radios/checklists use ↑↓ only; switch uses ←→ only.
fn focused_arrow_hint(ui: &ActiveUiRequest) -> Option<ArrowHint> {
    let comp = ui.components.get(ui.focused_index)?;
    match comp.component_type {
        ComponentType::Slider => Some(ArrowHint {
            keys: "←→",
            desc: "adjust",
        }),
        ComponentType::Switch => Some(ArrowHint {
            keys: "←→",
            desc: "toggle",
        }),
        ComponentType::Radio
        | ComponentType::Select
        | ComponentType::Checklist
        | ComponentType::Multiselect => Some(ArrowHint {
            keys: "↑↓",
            desc: "move",
        }),
        _ => None,
    }
}

/// If the given component reacts to Space, return what Space does there.
/// Used to decide whether to surface the Space shortcut in the footer.
fn space_action_label(comp: &UiComponent) -> Option<&'static str> {
    match comp.component_type {
        ComponentType::Switch => Some("toggle"),
        ComponentType::Radio | ComponentType::Select => Some("select"),
        ComponentType::Checklist | ComponentType::Multiselect => Some("toggle"),
        _ => None,
    }
}

/// Option lists render image markers when any option carries an image
/// (the web operator UI shows these as an image-card grid).
fn options_have_images(comp: &UiComponent) -> bool {
    comp.options
        .as_ref()
        .is_some_and(|opts| opts.iter().any(|o| o.image.is_some()))
}

fn component_height(comp: &UiComponent) -> u16 {
    match comp.component_type {
        ComponentType::Text => 3,
        ComponentType::Image => 10,
        ComponentType::Progress => 3,
        // Base 3 (label+value line + error line + trailing gap); reserve one
        // extra row when a description line is present so it stacks under the
        // label like operator-ui's Field (label -> description -> input -> error).
        ComponentType::TextInput | ComponentType::NumberInput => {
            3 + comp.description.is_some() as u16
        }
        ComponentType::Textarea => 5,
        ComponentType::Switch => 2,
        ComponentType::Slider => 2,
        ComponentType::Radio
        | ComponentType::Select
        | ComponentType::Checklist
        | ComponentType::Multiselect => {
            let opt_count = comp.options.as_ref().map(|o| o.len()).unwrap_or(0);
            (opt_count as u16 + 2).min(12)
        }
    }
}

fn draw_ui_panel(f: &mut Frame, state: &TuiState, area: Rect, image_cache: &mut ImageCache) {
    let Some(ui) = state.active_ui.as_ref() else {
        return;
    };

    let phase_name = state
        .phases
        .iter()
        .find(|p| p.key == ui.phase_key)
        .map(|p| p.name.as_str())
        .unwrap_or(&ui.phase_key);

    let title = format!("Operator UI - {phase_name}");
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // Reserve the top row(s) for `<name> is editing…` indicators — one
    // line per remote user (capped) so a flood of viewers can't starve
    // the input area.
    let presence_lines = collect_presence_lines(state, ui);
    let (presence_area, body_area) = if presence_lines.is_empty() {
        (Rect::default(), inner)
    } else {
        let reserve = (presence_lines.len() as u16).min(inner.height.saturating_sub(1));
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(reserve), Constraint::Min(0)])
            .split(inner);
        (split[0], split[1])
    };
    if !presence_lines.is_empty() && presence_area.height > 0 {
        f.render_widget(Paragraph::new(presence_lines), presence_area);
    }

    if body_area.height == 0 || body_area.width == 0 {
        return;
    }

    let constraints: Vec<Constraint> = ui
        .components
        .iter()
        .map(|c| Constraint::Length(component_height(c)))
        .collect();

    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(body_area);

    for (idx, (comp, cstate)) in ui.components.iter().zip(ui.states.iter()).enumerate() {
        if idx >= areas.len() {
            break;
        }
        let comp_area = areas[idx];
        if comp_area.height == 0 {
            continue;
        }
        let is_focused = ui.focused_index == idx;
        draw_component(f, ui, comp, cstate, is_focused, comp_area, image_cache);
    }
}

/// Build at most `MAX_PRESENCE_LINES` one-liners for remote users focused
/// on this UI request: `<name> is editing…`. Deliberately minimal — no
/// color, no component key, no draft value. "editing" works for any
/// input type (switch, slider, text) since focus broadcasts regardless
/// of whether values stream. Overflow collapses into `+N more editing…`
/// so a flood of viewers can't starve the input area.
const MAX_PRESENCE_LINES: usize = 3;

fn collect_presence_lines<'a>(state: &'a TuiState, ui: &'a ActiveUiRequest) -> Vec<Line<'a>> {
    let mut users: Vec<&super::state::PresenceState> = state
        .presence
        .values()
        .filter(|p| {
            p.focus_request_id
                .as_deref()
                .map(|r| r == ui.request_id)
                .unwrap_or(false)
        })
        .collect();
    users.sort_by_key(|a| a.seq);

    let total = users.len();
    let style = Style::default().fg(palette::MUTED);
    let mut lines: Vec<Line<'a>> = users
        .iter()
        .take(MAX_PRESENCE_LINES)
        .map(|user| {
            Line::from(Span::styled(
                format!("{} is editing…", user.display_name),
                style,
            ))
        })
        .collect();
    if total > MAX_PRESENCE_LINES {
        let extra = total - MAX_PRESENCE_LINES;
        lines.push(Line::from(Span::styled(
            format!("+{extra} more editing…"),
            style,
        )));
    }
    lines
}

fn draw_component(
    f: &mut Frame,
    ui: &ActiveUiRequest,
    comp: &UiComponent,
    state: &ComponentState,
    is_focused: bool,
    area: Rect,
    image_cache: &mut ImageCache,
) {
    let label = comp.label.as_deref().unwrap_or(&comp.key);
    let focus_marker = if is_focused { ">" } else { " " };
    // Only style the label when the row is focused — unstyled spans pick
    // up the terminal's default foreground, which respects user themes.
    let label_style = if is_focused {
        Style::default()
            .fg(palette::RUNNING)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    match (&comp.component_type, state) {
        (ComponentType::Text, _) => draw_text_display(f, comp, area),
        (ComponentType::Image, _) => draw_image_display(f, comp, area, image_cache),
        (ComponentType::Progress, _) => draw_progress_display(f, comp, label, area),

        (ComponentType::TextInput | ComponentType::NumberInput, ComponentState::Text(s))
        | (ComponentType::TextInput | ComponentType::NumberInput, ComponentState::Number(s)) => {
            draw_text_input(
                f,
                ui,
                comp,
                s,
                label,
                label_style,
                focus_marker,
                is_focused,
                area,
            )
        }

        (ComponentType::Textarea, ComponentState::Textarea(s)) => draw_textarea(
            f,
            ui,
            comp,
            s,
            label,
            label_style,
            focus_marker,
            is_focused,
            area,
        ),

        (ComponentType::Switch, ComponentState::Switch(on)) => {
            let toggle = if *on {
                Span::styled(
                    "[ON]",
                    Style::default()
                        .fg(palette::PASS)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled("[OFF]", Style::default().fg(palette::MUTED))
            };
            let p = Paragraph::new(Line::from(vec![
                Span::styled(format!("{focus_marker} {label}: "), label_style),
                toggle,
            ]));
            f.render_widget(p, area);
        }

        (ComponentType::Slider, ComponentState::Slider(v)) => {
            draw_slider(f, comp, *v, label, label_style, focus_marker, area)
        }

        (
            ComponentType::Radio | ComponentType::Select,
            ComponentState::SingleChoice { value, cursor },
        ) => {
            let show_image = options_have_images(comp);
            let lines = option_list_lines(
                comp,
                label,
                label_style,
                focus_marker,
                is_focused,
                *cursor,
                value.as_deref().into_iter().collect(),
                ui.errors.get(&comp.key).map(String::as_str),
                false,
                show_image,
            );
            f.render_widget(Paragraph::new(lines), area);
        }

        (
            ComponentType::Checklist | ComponentType::Multiselect,
            ComponentState::MultiChoice { selected, cursor },
        ) => {
            let show_image = options_have_images(comp);
            let lines = option_list_lines(
                comp,
                label,
                label_style,
                focus_marker,
                is_focused,
                *cursor,
                selected.iter().map(String::as_str).collect(),
                ui.errors.get(&comp.key).map(String::as_str),
                true,
                show_image,
            );
            f.render_widget(Paragraph::new(lines), area);
        }

        // State/type mismatch should be impossible — init always produces
        // matched variants. Render nothing rather than panic.
        _ => {}
    }
}

fn draw_text_display(f: &mut Frame, comp: &UiComponent, area: Rect) {
    let text = comp
        .value
        .as_ref()
        .or(comp.default_value.as_ref())
        .map(|v| match v {
            ComponentValue::String(s) => s.clone(),
            ComponentValue::Number(n) => n.to_string(),
            ComponentValue::Boolean(b) => b.to_string(),
            ComponentValue::Array(a) => a.join(", "),
        })
        .unwrap_or_default();
    let p =
        Paragraph::new(Line::from(Span::raw(text))).wrap(ratatui::widgets::Wrap { trim: false });
    f.render_widget(p, area);
}

fn draw_image_display(f: &mut Frame, comp: &UiComponent, area: Rect, image_cache: &mut ImageCache) {
    let path_str = comp
        .value
        .as_ref()
        .or(comp.default_value.as_ref())
        .and_then(|v| match v {
            ComponentValue::String(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();
    if path_str.is_empty() {
        return;
    }
    if let Some(protocol) = image_cache.get_or_load(&path_str) {
        let img_widget = StatefulImage::default();
        f.render_stateful_widget(img_widget, area, protocol);
    } else {
        let p = Paragraph::new(Line::from(Span::styled(
            format!("[Image: {path_str}]"),
            Style::default().fg(palette::MUTED),
        )));
        f.render_widget(p, area);
    }
}

fn draw_progress_display(f: &mut Frame, comp: &UiComponent, label: &str, area: Rect) {
    let pct = comp
        .value
        .as_ref()
        .or(comp.default_value.as_ref())
        .map(|v| match v {
            ComponentValue::Number(n) => *n,
            ComponentValue::String(s) => s.parse().unwrap_or(0.0),
            _ => 0.0,
        })
        .unwrap_or(0.0);
    let max = comp.max.unwrap_or(100.0);
    let ratio = (pct / max).clamp(0.0, 1.0);
    let bar_w = (area.width as usize).saturating_sub(4);
    let filled = (ratio * bar_w as f64) as usize;
    let empty = bar_w.saturating_sub(filled);
    let lines = vec![
        Line::from(Span::raw(format!("{label}: "))),
        Line::from(vec![
            Span::styled("█".repeat(filled), Style::default().fg(palette::RUNNING)),
            Span::styled("░".repeat(empty), Style::default().fg(palette::MUTED)),
            Span::raw(format!(" {pct:.0}%")),
        ]),
    ];
    f.render_widget(Paragraph::new(lines), area);
}

#[allow(clippy::too_many_arguments)]
fn draw_text_input(
    f: &mut Frame,
    ui: &ActiveUiRequest,
    comp: &UiComponent,
    value: &str,
    label: &str,
    label_style: Style,
    focus_marker: &str,
    is_focused: bool,
    area: Rect,
) {
    let display_val = if value.is_empty() {
        comp.placeholder.as_deref().unwrap_or("").to_string()
    } else {
        value.to_string()
    };
    // Placeholder is muted; typed value uses the terminal's default fg.
    let val_style = if value.is_empty() {
        Style::default().fg(palette::MUTED)
    } else {
        Style::default()
    };
    let cursor = if is_focused { "_" } else { "" };
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{focus_marker} {label}: "), label_style),
        Span::styled(display_val, val_style),
        Span::styled(cursor.to_string(), Style::default().fg(palette::RUNNING)),
    ])];
    // Helper text below the input, mirroring operator-ui's FieldDescription.
    // Indented to align under the label, muted to read as a hint not a value.
    if let Some(desc) = comp.description.as_deref() {
        lines.push(Line::from(Span::styled(
            format!("    {desc}"),
            Style::default().fg(palette::MUTED),
        )));
    }
    if let Some(err) = ui.errors.get(&comp.key) {
        lines.push(Line::from(Span::styled(
            format!("    ! {err}"),
            Style::default().fg(palette::ERROR),
        )));
    }
    f.render_widget(Paragraph::new(lines), area);
}

#[allow(clippy::too_many_arguments)]
fn draw_textarea(
    f: &mut Frame,
    ui: &ActiveUiRequest,
    comp: &UiComponent,
    value: &str,
    label: &str,
    label_style: Style,
    focus_marker: &str,
    is_focused: bool,
    area: Rect,
) {
    let mut lines = vec![Line::from(Span::styled(
        format!("{focus_marker} {label}:"),
        label_style,
    ))];
    for text_line in value.lines() {
        lines.push(Line::from(Span::raw(format!("  {text_line}"))));
    }
    if is_focused {
        lines.push(Line::from(Span::styled(
            "  _",
            Style::default().fg(palette::RUNNING),
        )));
    }
    if let Some(err) = ui.errors.get(&comp.key) {
        lines.push(Line::from(Span::styled(
            format!("    ! {err}"),
            Style::default().fg(palette::ERROR),
        )));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_slider(
    f: &mut Frame,
    comp: &UiComponent,
    current: f64,
    label: &str,
    label_style: Style,
    focus_marker: &str,
    area: Rect,
) {
    let min = comp.min.unwrap_or(0.0);
    let max = comp.max.unwrap_or(100.0);
    let ratio = if max > min {
        ((current - min) / (max - min)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let bar_w = (area.width as usize).saturating_sub(20);
    let filled = (ratio * bar_w as f64) as usize;
    let empty = bar_w.saturating_sub(filled);
    let p = Paragraph::new(Line::from(vec![
        Span::styled(format!("{focus_marker} {label}: "), label_style),
        Span::styled("█".repeat(filled), Style::default().fg(palette::RUNNING)),
        Span::styled("░".repeat(empty), Style::default().fg(palette::MUTED)),
        Span::raw(format!(" {current:.1}")),
    ]));
    f.render_widget(p, area);
}

#[allow(clippy::too_many_arguments)]
fn option_list_lines<'a>(
    comp: &'a UiComponent,
    label: &'a str,
    label_style: Style,
    focus_marker: &'a str,
    is_focused: bool,
    cursor: usize,
    selected_values: Vec<&'a str>,
    error: Option<&'a str>,
    multi: bool,
    show_image: bool,
) -> Vec<Line<'a>> {
    let mut lines = vec![Line::from(Span::styled(
        format!("{focus_marker} {label}:"),
        label_style,
    ))];
    if let Some(options) = comp.options.as_ref() {
        for (oi, opt) in options.iter().enumerate() {
            let active = selected_values.contains(&opt.value.as_str());
            let cursor_marker = if is_focused && oi == cursor { ">" } else { " " };
            let marker = match (multi, active) {
                (true, true) => "[✓]",
                (true, false) => "[ ]",
                (false, true) => "(●)",
                (false, false) => "( )",
            };
            // Focused cursor row wins; otherwise selected rows show in the
            // pass/success color. Unfocused/unselected rows inherit the
            // terminal default.
            let opt_style = if is_focused && oi == cursor {
                Style::default()
                    .fg(palette::RUNNING)
                    .add_modifier(Modifier::BOLD)
            } else if active {
                Style::default().fg(palette::PASS)
            } else {
                Style::default()
            };
            let display = if show_image {
                if let Some(ref img) = opt.image {
                    format!("{} [{}]", opt.label, img)
                } else {
                    opt.label.clone()
                }
            } else {
                opt.label.clone()
            };
            lines.push(Line::from(Span::styled(
                format!("  {cursor_marker} {marker} {display}"),
                opt_style,
            )));
        }
    }
    if let Some(err) = error {
        lines.push(Line::from(Span::styled(
            format!("    ! {err}"),
            Style::default().fg(palette::ERROR),
        )));
    }
    lines
}
