//! The drainage TUI. Three tabs, in priority order: Drift (is my exchange rate
//! getting worse over time?), Attribution (which model/activity drains the limit
//! fastest?), and Budget (how much of each window is left, and for how long?).
//! Reloads from disk every few seconds so it tracks live usage.

use crate::analysis::Window;
use crate::data::Dataset as Data;
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

struct App {
    data: Data,
    tab: usize,
    last_reload: Instant,
    loaded_ago: Instant,
}

pub fn run() -> Result<()> {
    let data = Data::load(Weights::default())?;
    let mut app = App {
        data,
        tab: 0,
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

/// The busiest subscription account — what the single-provider panels focus on.
fn primary(app: &App) -> Provider {
    app.data
        .providers()
        .first()
        .cloned()
        .unwrap_or(Provider::Anthropic)
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
        Span::raw(format!("  {p} 5h:{five:.0}%  7d:{week:.0}%")),
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
        Span::styled("r", Style::new().fg(Color::Cyan)),
        Span::raw(" reload  "),
        Span::styled("q", Style::new().fg(Color::Cyan)),
        Span::raw(format!(" quit   ·   updated {ago}s ago, reloads live")),
    ]);
    f.render_widget(Paragraph::new(help).style(Style::new().fg(Color::Gray)), area);
}

fn draw_drift(f: &mut Frame, app: &App, area: Rect) {
    let p = primary(app);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(6), Constraint::Min(0)])
        .split(area);

    // ---- Summary: recent vs older median, per window ----
    let mut lines = vec![Line::from(Span::styled(
        format!("Is my {p} rate getting worse?  (higher %/Mtok = less usage per token = worse)"),
        Style::new().fg(Color::Gray),
    ))];
    for (w, name) in [(Window::FiveHour, "5h"), (Window::SevenDay, "7d")] {
        match app.data.drift_summary(&p, w) {
            Some((recent, older, change)) => {
                let (arrow, color, verdict) = if change > 2.0 {
                    ("▲", Color::Red, "worse")
                } else if change < -2.0 {
                    ("▼", Color::Green, "better")
                } else {
                    ("≈", Color::Yellow, "stable")
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("  {name}: "), Style::new().bold()),
                    Span::raw(format!("now {recent:.2} %/Mtok  vs  {older:.2} earlier   ")),
                    Span::styled(
                        format!("{arrow} {change:+.0}% {verdict}"),
                        Style::new().fg(color).bold(),
                    ),
                ]));
            }
            None => {
                let med = app.data.median_rate(&p, w);
                lines.push(Line::from(vec![
                    Span::styled(format!("  {name}: "), Style::new().bold()),
                    match med {
                        Some(m) => Span::raw(format!(
                            "{m:.2} %/Mtok  (need ≥2 days of data to show drift)"
                        )),
                        None => Span::styled(
                            "collecting… keep using Claude Code",
                            Style::new().fg(Color::DarkGray),
                        ),
                    },
                ]));
            }
        }
    }
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" drift ")),
        rows[0],
    );

    // ---- Chart ----
    let s5 = app.data.drift_series(&p, Window::FiveHour);
    let s7 = app.data.drift_series(&p, Window::SevenDay);
    if s5.len() + s7.len() < 2 {
        f.render_widget(
            Paragraph::new("Not enough snapshots yet to chart drift.\nThe collector is live — this fills in as you use Claude Code.")
                .block(Block::default().borders(Borders::ALL).title(" exchange-rate over time "))
                .style(Style::new().fg(Color::DarkGray)),
            rows[1],
        );
        return;
    }

    let all_x: Vec<f64> = s5.iter().chain(s7.iter()).map(|p| p.0).collect();
    let all_y: Vec<f64> = s5.iter().chain(s7.iter()).map(|p| p.1).collect();
    let xmin = all_x.iter().cloned().fold(f64::INFINITY, f64::min);
    let xmax = all_x.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let ymax = all_y.iter().cloned().fold(0.0_f64, f64::max).max(1.0) * 1.2;
    let xmax = if xmax <= xmin { xmin + 1.0 } else { xmax };

    let datasets = vec![
        ChartDataset::default()
            .name("5h")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(Color::Cyan))
            .data(&s5),
        ChartDataset::default()
            .name("7d")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(Color::Magenta))
            .data(&s7),
    ];
    let x_labels = vec![
        Span::raw(crate::analysis::day_key(xmin as i64)),
        Span::raw(crate::analysis::day_key(xmax as i64)),
    ];
    let y_labels = vec![
        Span::raw("0"),
        Span::raw(format!("{:.0}", ymax / 2.0)),
        Span::raw(format!("{ymax:.0}")),
    ];
    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" exchange-rate over time  (cyan 5h · magenta 7d) "),
        )
        .x_axis(
            Axis::default()
                .style(Style::new().fg(Color::DarkGray))
                .bounds([xmin, xmax])
                .labels(x_labels),
        )
        .y_axis(
            Axis::default()
                .title("%/Mtok")
                .style(Style::new().fg(Color::DarkGray))
                .bounds([0.0, ymax])
                .labels(y_labels),
        );
    f.render_widget(chart, rows[1]);
}

fn draw_attr(f: &mut Frame, app: &App, area: Rect) {
    let rows = app.data.by_model();
    let max_rate = rows
        .iter()
        .filter_map(|r| r.rate_5h)
        .fold(0.0_f64, f64::max);

    let header = Row::new(vec!["harness", "model", "weighted", "output", "calls", "5h %/Mtok"])
        .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    let body: Vec<Row> = rows
        .iter()
        .map(|r| {
            let rate = match r.rate_5h {
                Some(v) => format!("{v:.2}"),
                None => "—".into(),
            };
            // Highlight the fastest drainer.
            let style = if r.rate_5h.map(|v| v >= max_rate && max_rate > 0.0).unwrap_or(false) {
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            Row::new(vec![
                Cell::from(r.harness.to_string()),
                Cell::from(r.model.clone()),
                Cell::from(human(r.weighted)),
                Cell::from(human(r.output as f64)),
                Cell::from(r.calls.to_string()),
                Cell::from(rate),
            ])
            .style(style)
        })
        .collect();

    let table = Table::new(
        body,
        [
            Constraint::Length(12),
            Constraint::Min(20),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(10),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" what drains the limit fastest  (red = highest %/Mtok) "),
    );
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

    // Projection: remaining weighted tokens until the 7d window is full.
    let mut lines = vec![];
    match app.data.median_rate(&p, Window::SevenDay) {
        Some(rate) if rate > 0.0 => {
            let remaining_pct = (100.0 - week).max(0.0);
            let remaining_tok = remaining_pct / rate * 1_000_000.0;
            lines.push(Line::from(vec![
                Span::raw("At your current 7d rate ("),
                Span::styled(format!("{rate:.2} %/Mtok"), Style::new().fg(Color::Cyan)),
                Span::raw("), about "),
                Span::styled(human(remaining_tok), Style::new().fg(Color::Green).bold()),
                Span::raw(" weighted tokens remain this week."),
            ]));
        }
        _ => lines.push(Line::from(Span::styled(
            "Projection unlocks once an exchange rate has been measured.",
            Style::new().fg(Color::DarkGray),
        ))),
    }
    // Other accounts (providers) we also track utilization for.
    let others: Vec<String> = app
        .data
        .providers()
        .into_iter()
        .filter(|q| q != &p)
        .filter_map(|q| {
            app.data.latest_snap(&q).map(|s| {
                format!(
                    "{q} 5h:{:.0}% 7d:{:.0}%",
                    s.five_pct.unwrap_or(0.0),
                    s.week_pct.unwrap_or(0.0)
                )
            })
        })
        .collect();
    if !others.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("other accounts: {}", others.join("   ")),
            Style::new().fg(Color::Gray),
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

    /// Render every tab headlessly to catch layout/bounds panics on real data.
    #[test]
    fn renders_all_tabs() {
        let data = Data::load(Weights::default()).expect("load");
        let mut app = App {
            data,
            tab: 0,
            last_reload: Instant::now(),
            loaded_ago: Instant::now(),
        };
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("term");
        for t in 0..3 {
            app.tab = t;
            terminal.draw(|f| draw(f, &app)).expect("draw");
        }
    }
}
