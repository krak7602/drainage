//! The drainage TUI. Three tabs, in priority order: Drift (is my per-model
//! exchange rate getting worse over time?), Attribution (which model drains the
//! limit fastest?), and Budget (how much of each window is left, and for how
//! long?). Reloads from disk every few seconds so it tracks live usage.

use crate::analysis::Window;
use crate::data::{Dataset as Data, Method};
use crate::model::{Provider, Weights};
use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Cell, Chart, Dataset as ChartDataset, Gauge, GraphType, Paragraph, Row,
    Table, Tabs,
};
use ratatui::Frame;
use std::time::{Duration, Instant};

const RELOAD_EVERY: Duration = Duration::from_secs(3);
/// Distinct colors for per-model chart lines / labels.
const PALETTE: [Color; 6] = [
    Color::Cyan,
    Color::Magenta,
    Color::Yellow,
    Color::Green,
    Color::Blue,
    Color::LightRed,
];
const MAX_MODELS: usize = 5;

/// A model's drift line for the chart: (short label, color, per-day points).
type ModelSeries = (String, Color, Vec<(f64, f64)>);

struct App {
    data: Data,
    tab: usize,
    window: Window,
    method: Method,
    last_reload: Instant,
    loaded_ago: Instant,
}

pub fn run() -> Result<()> {
    let data = Data::load(Weights::default())?;
    let mut app = App {
        data,
        tab: 0,
        // 5h has many more reset-epochs than 7d today, so its levels estimate is
        // the strongest signal; levels+Kalman is the most accurate estimator.
        window: Window::FiveHour,
        method: Method::Levels,
        last_reload: Instant::now(),
        loaded_ago: Instant::now(),
    };

    let mut terminal = ratatui::init();
    let res = loop {
        if let Err(e) = terminal.draw(|f| draw(f, &app)) {
            break Err(e.into());
        }
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                        KeyCode::Char('1') => app.tab = 0,
                        KeyCode::Char('2') => app.tab = 1,
                        KeyCode::Char('3') => app.tab = 2,
                        KeyCode::Tab | KeyCode::Right => app.tab = (app.tab + 1) % 3,
                        KeyCode::Left => app.tab = (app.tab + 2) % 3,
                        KeyCode::Char('w') => app.window = app.window.other(),
                        KeyCode::Char('m') => app.method = app.method.toggle(),
                        KeyCode::Char('r') => reload(&mut app),
                        _ => {}
                    }
                }
            }
        }
        if app.last_reload.elapsed() >= RELOAD_EVERY {
            reload(&mut app);
        }
    };
    ratatui::restore();
    res
}

fn reload(app: &mut App) {
    if let Ok(d) = Data::load(Weights::default()) {
        app.data = d;
        app.loaded_ago = Instant::now();
    }
    app.last_reload = Instant::now();
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

fn primary(app: &App) -> Provider {
    app.data
        .providers()
        .first()
        .cloned()
        .unwrap_or(Provider::Anthropic)
}

/// Shorten a model id for display: strip the `claude-` prefix and any trailing
/// `-YYYYMMDD` date segment.
fn short_model(m: &str) -> String {
    let s = m.strip_prefix("claude-").unwrap_or(m);
    if let Some(idx) = s.rfind('-') {
        let tail = &s[idx + 1..];
        if tail.len() >= 6 && tail.chars().all(|c| c.is_ascii_digit()) {
            return s[..idx].to_string();
        }
    }
    s.to_string()
}

fn human(n: f64) -> String {
    if n >= 1e9 {
        format!("{:.2}B", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.2}M", n / 1e6)
    } else if n >= 1e3 {
        format!("{:.1}K", n / 1e3)
    } else {
        format!("{n:.0}")
    }
}

fn dur(secs: i64) -> String {
    if secs <= 0 {
        return "now".into();
    }
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    if h > 24 {
        format!("{}d {}h", h / 24, h % 24)
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_header(f, app, chunks[0]);
    match app.tab {
        0 => draw_drift(f, app, chunks[1]),
        1 => draw_attr(f, app, chunks[1]),
        _ => draw_budget(f, app, chunks[1]),
    }
    draw_footer(f, app, chunks[2]);
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let p = primary(app);
    let latest = app.data.latest_snap(&p);
    let (five, week) = latest
        .map(|s| (s.five_pct.unwrap_or(0.0), s.week_pct.unwrap_or(0.0)))
        .unwrap_or((0.0, 0.0));
    let title = Line::from(vec![
        Span::styled(" drainage ", Style::new().bold().fg(Color::Black).bg(Color::Cyan)),
        Span::raw(format!(
            "  {p} 5h:{five:.0}%  7d:{week:.0}%  ·  window: {}  ·  method: {}",
            app.window.label(),
            app.method.label()
        )),
    ]);
    let tabs = Tabs::new(vec!["1 Drift", "2 Attribution", "3 Budget"])
        .select(app.tab)
        .style(Style::new().fg(Color::DarkGray))
        .highlight_style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(tabs, area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let ago = app.loaded_ago.elapsed().as_secs();
    let help = Line::from(vec![
        Span::styled("1/2/3", Style::new().fg(Color::Cyan)),
        Span::raw(" tabs  "),
        Span::styled("w", Style::new().fg(Color::Cyan)),
        Span::raw(" 5h/7d  "),
        Span::styled("m", Style::new().fg(Color::Cyan)),
        Span::raw(" method  "),
        Span::styled("r", Style::new().fg(Color::Cyan)),
        Span::raw(" reload  "),
        Span::styled("q", Style::new().fg(Color::Cyan)),
        Span::raw(format!(" quit   ·   updated {ago}s ago")),
    ]);
    f.render_widget(Paragraph::new(help).style(Style::new().fg(Color::Gray)), area);
}

fn draw_drift(f: &mut Frame, app: &App, area: Rect) {
    let p = primary(app);
    let win = app.window;
    let models: Vec<String> = app.data.models(&p).into_iter().take(MAX_MODELS).collect();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(0)])
        .split(area);

    // ---- Per-model summary ----
    let mut lines = vec![Line::from(Span::styled(
        format!("Is my {p} {} rate getting worse?  (per model · higher %/Mtok = worse)", win.label()),
        Style::new().fg(Color::Gray),
    ))];
    let mut any = false;
    for (idx, model) in models.iter().enumerate() {
        let color = PALETTE[idx % PALETTE.len()];
        let short = short_model(model);
        match app.data.model_drift_summary(&p, model, win, app.method) {
            Some((recent, older, change)) => {
                any = true;
                let (arrow, vcolor, verdict) = if change > 2.0 {
                    ("▲", Color::Red, "worse")
                } else if change < -2.0 {
                    ("▼", Color::Green, "better")
                } else {
                    ("≈", Color::Yellow, "stable")
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("  {short:14} "), Style::new().fg(color).bold()),
                    Span::raw(format!("now {recent:.2}  vs {older:.2}   ")),
                    Span::styled(format!("{arrow} {change:+.0}% {verdict}"), Style::new().fg(vcolor).bold()),
                ]));
            }
            None => {
                if let Some((rate, n)) = app.data.model_rate(&p, model, win, app.method) {
                    any = true;
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {short:14} "), Style::new().fg(color).bold()),
                        Span::raw(format!("{rate:.2} %/Mtok  (n={n}, need ≥2 days for drift)")),
                    ]));
                }
            }
        }
    }
    if !any {
        lines.push(Line::from(Span::styled(
            "  collecting… keep using your agents; per-model rates fill in here.",
            Style::new().fg(Color::DarkGray),
        )));
    }
    let (attr, mixed) = app.data.attribution_coverage(&p, win);
    if attr + mixed > 0.0 {
        let frac = mixed / (attr + mixed) * 100.0;
        lines.push(Line::from(Span::styled(
            format!("  unattributed: {frac:.0}% of spend is in mixed-model intervals (priced later via NNLS)"),
            Style::new().fg(Color::DarkGray),
        )));
    }
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" drift ")),
        rows[0],
    );

    // ---- Per-model chart ----
    let series: Vec<ModelSeries> = models
        .iter()
        .enumerate()
        .map(|(idx, m)| {
            (
                short_model(m),
                PALETTE[idx % PALETTE.len()],
                app.data.model_drift_series(&p, m, win, app.method),
            )
        })
        .filter(|(_, _, s)| !s.is_empty())
        .collect();
    let total_pts: usize = series.iter().map(|(_, _, s)| s.len()).sum();
    if total_pts < 2 {
        f.render_widget(
            Paragraph::new("Not enough per-model snapshots yet to chart drift.\nThe collector is live — this fills in as you use your agents.")
                .block(Block::default().borders(Borders::ALL).title(" exchange-rate over time "))
                .style(Style::new().fg(Color::DarkGray)),
            rows[1],
        );
        return;
    }

    let xs: Vec<f64> = series.iter().flat_map(|(_, _, s)| s.iter().map(|p| p.0)).collect();
    let ys: Vec<f64> = series.iter().flat_map(|(_, _, s)| s.iter().map(|p| p.1)).collect();
    let xmin = xs.iter().cloned().fold(f64::INFINITY, f64::min);
    let mut xmax = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    if xmax <= xmin {
        xmax = xmin + 1.0;
    }
    let ymax = ys.iter().cloned().fold(0.0_f64, f64::max).max(1.0) * 1.2;

    let datasets: Vec<ChartDataset> = series
        .iter()
        .map(|(name, color, s)| {
            ChartDataset::default()
                .name(name.clone())
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::new().fg(*color))
                .data(s)
        })
        .collect();
    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" exchange-rate over time · {} (%/Mtok, per model) ", win.label())),
        )
        .x_axis(
            Axis::default()
                .style(Style::new().fg(Color::DarkGray))
                .bounds([xmin, xmax])
                .labels(vec![
                    Span::raw(crate::analysis::day_key(xmin as i64)),
                    Span::raw(crate::analysis::day_key(xmax as i64)),
                ]),
        )
        .y_axis(
            Axis::default()
                .title("%/Mtok")
                .style(Style::new().fg(Color::DarkGray))
                .bounds([0.0, ymax])
                .labels(vec![
                    Span::raw("0"),
                    Span::raw(format!("{:.0}", ymax / 2.0)),
                    Span::raw(format!("{ymax:.0}")),
                ]),
        );
    f.render_widget(chart, rows[1]);
}

fn draw_attr(f: &mut Frame, app: &App, area: Rect) {
    let win = app.window;
    let rows = app.data.by_model();
    // Look up each model's current-window rate; find the max for highlighting.
    let rate_of =
        |r: &crate::data::ModelRow| app.data.model_rate(&r.provider, &r.model, win, app.method).map(|(v, _)| v);
    let max_rate = rows.iter().filter_map(rate_of).fold(0.0_f64, f64::max);

    let header = Row::new(vec!["harness", "model", "weighted", "output", "calls", "%/Mtok"])
        .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    let body: Vec<Row> = rows
        .iter()
        .map(|r| {
            let rate = rate_of(r);
            let rate_s = rate.map(|v| format!("{v:.2}")).unwrap_or_else(|| "—".into());
            let style = if rate.map(|v| v >= max_rate && max_rate > 0.0).unwrap_or(false) {
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            Row::new(vec![
                Cell::from(r.harness.to_string()),
                Cell::from(short_model(&r.model)),
                Cell::from(human(r.weighted)),
                Cell::from(human(r.output as f64)),
                Cell::from(r.calls.to_string()),
                Cell::from(rate_s),
            ])
            .style(style)
        })
        .collect();

    let table = Table::new(
        body,
        [
            Constraint::Length(12),
            Constraint::Min(16),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(9),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(format!(
        " what drains the {} limit fastest  (red = highest %/Mtok) ",
        win.label()
    )));
    f.render_widget(table, area);
}

fn draw_budget(f: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .split(area);

    let p = primary(app);
    let snap = app.data.latest_snap(&p);
    let (five, week, five_reset, week_reset) = snap
        .map(|s| {
            (
                s.five_pct.unwrap_or(0.0),
                s.week_pct.unwrap_or(0.0),
                s.five_reset.unwrap_or(0),
                s.week_reset.unwrap_or(0),
            )
        })
        .unwrap_or((0.0, 0.0, 0, 0));
    let now = now_ts();

    let gauge = |pct: f64, reset: i64, label: &str| {
        let color = if pct >= 85.0 {
            Color::Red
        } else if pct >= 60.0 {
            Color::Yellow
        } else {
            Color::Green
        };
        Gauge::default()
            .block(Block::default().borders(Borders::ALL).title(format!(
                " {label}  (resets in {}) ",
                dur(reset - now)
            )))
            .gauge_style(Style::new().fg(color))
            .ratio((pct / 100.0).clamp(0.0, 1.0))
            .label(format!("{pct:.1}%"))
    };
    f.render_widget(gauge(five, five_reset, &format!("{p} · 5-hour window")), cols[0]);
    f.render_widget(gauge(week, week_reset, &format!("{p} · 7-day window")), cols[1]);

    // Per-model projection: how many more tokens of each model until the 7d
    // window is full, at that model's own measured rate.
    let mut lines = vec![Line::from(Span::styled(
        "Weekly headroom, per model (at each model's own measured rate):",
        Style::new().fg(Color::Gray),
    ))];
    let remaining_pct = (100.0 - week).max(0.0);
    let mut shown = 0;
    for (idx, model) in app.data.models(&p).into_iter().enumerate() {
        if shown >= 3 {
            break;
        }
        if let Some((rate, _)) = app.data.model_rate(&p, &model, Window::SevenDay, app.method) {
            if rate > 0.0 {
                let tok = remaining_pct / rate * 1_000_000.0;
                lines.push(Line::from(vec![
                    Span::styled(format!("  {:14} ", short_model(&model)), Style::new().fg(PALETTE[idx % PALETTE.len()]).bold()),
                    Span::raw("~"),
                    Span::styled(human(tok), Style::new().fg(Color::Green).bold()),
                    Span::raw(format!(" more {} tokens  (rate {rate:.1} %/Mtok)", short_model(&model))),
                ]));
                shown += 1;
            }
        }
    }
    if shown == 0 {
        lines.push(Line::from(Span::styled(
            "  projection unlocks once a per-model rate has been measured.",
            Style::new().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(
            "snapshots  claude {} · codex {} · omp {}     token events  claude {} · codex {} · omp {}",
            app.data.n_claude_snaps,
            app.data.n_codex_snaps,
            app.data.n_omp_snaps,
            app.data.n_claude,
            app.data.n_codex,
            app.data.n_omp
        ),
        Style::new().fg(Color::Gray),
    )));
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" projection ")),
        cols[2],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn renders_all_tabs() {
        let data = Data::load(Weights::default()).expect("load");
        let mut app = App {
            data,
            tab: 0,
            window: Window::SevenDay,
            method: Method::Single,
            last_reload: Instant::now(),
            loaded_ago: Instant::now(),
        };
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("term");
        for method in [Method::Single, Method::Nnls, Method::Levels] {
            app.method = method;
            for w in [Window::FiveHour, Window::SevenDay] {
                app.window = w;
                for t in 0..3 {
                    app.tab = t;
                    terminal.draw(|f| draw(f, &app)).expect("draw");
                }
            }
        }
    }
}
