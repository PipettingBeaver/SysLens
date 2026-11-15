use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Gauge, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use crate::app::{App, Modal, StatusKind, Toast};
use crate::proc::{
    category, is_compositor_unit, read_cmdline, Category, Level, ProcRow, Snapshot, UnitStat,
};
use crate::sysd::Action;
use crate::util::human_bytes;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let narrow = area.width < 100;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(if narrow { 8 } else { 4 }),
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(frame, chunks[0], app);
    draw_cards(frame, chunks[1], app, narrow);
    draw_warning(frame, chunks[2], app);
    draw_units(frame, chunks[3], app, narrow);
    draw_footer(frame, chunks[4], app, narrow);

    if let Some(toast) = &app.toast {
        if toast.until > std::time::Instant::now() {
            draw_toast(frame, area, toast);
        }
    }

    match &app.modal {
        Modal::None => {}
        Modal::Help => draw_help(frame, area),
        Modal::Menu { unit, index } => draw_menu(frame, area, app, unit, *index),
        Modal::Confirm { action, unit } => draw_confirm(frame, area, app, *action, unit),
        Modal::ConfirmAll { action, units } => draw_confirm_all(frame, area, *action, units),
        Modal::Processes {
            unit,
            index,
            offset,
        } => draw_processes(frame, area, app, unit, *index, *offset),
        Modal::Emergency { index } => draw_emergency(frame, area, app, *index),
    }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let snapshot = &app.snapshot;
    let guard = app.guard_label();
    let line = Line::from(vec![
        Span::styled(
            concat!("SysLens ", env!("CARGO_PKG_VERSION"), " "),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{}  ", snapshot.host),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            snapshot.clock.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("   refresh {:.1}s", app.interval),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("   {guard}"),
            Style::default().fg(guard_color(&guard)),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_cards(frame: &mut Frame, area: Rect, app: &App, narrow: bool) {
    let snapshot = &app.snapshot;
    let mut cards: Vec<(&str, f64, String, Level)> = Vec::new();

    let ram = &snapshot.ram;
    let ram_level = if ram.available >= 3 * crate::proc::GIB {
        Level::Ok
    } else if ram.available >= 1536 * 1024 * 1024 {
        Level::Warn
    } else {
        Level::Crit
    };
    cards.push((
        "MEMORY",
        ram.percent / 100.0,
        format!(
            "{}/{} · {} free",
            compact(ram.used),
            compact(ram.total),
            compact(ram.available)
        ),
        ram_level,
    ));

    let swap = &snapshot.swap;
    let swap_level = if swap.percent < 70.0 {
        Level::Ok
    } else if swap.percent < 85.0 {
        Level::Warn
    } else {
        Level::Crit
    };
    let zram = snapshot
        .zram
        .map(|zram| format!(" z{}→{}", compact(zram.data), compact(zram.compr)))
        .unwrap_or_default();
    cards.push((
        "SWAP / ZRAM",
        swap.percent / 100.0,
        format!(
            "{}/{} {:.0}%{}",
            compact(swap.used),
            compact(swap.total),
            swap.percent,
            zram
        ),
        swap_level,
    ));

    let reserve = snapshot.reserve_mb;
    match &snapshot.gpu {
        Some(gpu) => {
            let level = if gpu.free >= reserve * 3 / 2 {
                Level::Ok
            } else if gpu.free >= reserve {
                Level::Warn
            } else {
                Level::Crit
            };
            cards.push((
                "GPU VRAM",
                ratio(gpu.used, gpu.total),
                format!("{}/{}M free {} r{}", gpu.used, gpu.total, gpu.free, reserve),
                level,
            ));
            let util = gpu.util.unwrap_or(0);
            let util_text = text_or_na(gpu.util, "%");
            let temp_text = text_or_na(gpu.temp, "C");
            cards.push((
                "GPU / CPU",
                util as f64 / 100.0,
                format!(
                    "GPU {} {} · CPU {:.0}% L{:.2}",
                    util_text, temp_text, snapshot.cpu_pct, snapshot.load[0]
                ),
                if util < 90 { Level::Ok } else { Level::Warn },
            ));
        }
        None => {
            cards.push((
                "GPU VRAM",
                0.0,
                "no GPU VRAM telemetry".to_string(),
                Level::Info,
            ));
            cards.push((
                "CPU / LOAD",
                snapshot.cpu_pct / 100.0,
                format!(
                    "CPU {:.0}% L{:.2} {:.2} {:.2}",
                    snapshot.cpu_pct, snapshot.load[0], snapshot.load[1], snapshot.load[2]
                ),
                Level::Ok,
            ));
        }
    }

    let layout = if narrow {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(4), Constraint::Length(4)])
            .split(area);
        vec![
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
                .split(rows[0]),
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
                .split(rows[1]),
        ]
    } else {
        vec![Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Ratio(1, 4),
                Constraint::Ratio(1, 4),
                Constraint::Ratio(1, 4),
                Constraint::Ratio(1, 4),
            ])
            .split(area)]
    };

    let mut index = 0;
    for row in layout {
        for slot in row.iter() {
            if let Some((title, ratio, value, level)) = cards.get(index).cloned() {
                draw_card(frame, *slot, title, ratio, &value, level);
            }
            index += 1;
        }
    }
}

fn draw_card(frame: &mut Frame, area: Rect, title: &str, ratio: f64, value: &str, level: Level) {
    let color = level_color(level);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Line::from(Span::styled(
            format!(" {title} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(inner);
    frame.render_widget(
        Gauge::default()
            .ratio(ratio.clamp(0.0, 1.0))
            .gauge_style(Style::default().fg(color)),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            value.to_string(),
            Style::default().fg(Color::Gray),
        ))),
        rows[1],
    );
}

fn draw_warning(frame: &mut Frame, area: Rect, app: &App) {
    let Some(first) = app.snapshot.warnings.first() else {
        return;
    };
    let color = level_color(first.level);
    let mut spans = vec![
        Span::styled(
            format!("[{}] ", first.level.label()),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(first.text.clone(), Style::default().fg(color)),
    ];
    if app.snapshot.warnings.len() > 1 {
        spans.push(Span::styled(
            format!("  (+{} more)", app.snapshot.warnings.len() - 1),
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_units(frame: &mut Frame, area: Rect, app: &mut App, narrow: bool) {
    let visible = area.height.saturating_sub(3) as usize;
    app.ensure_visible(visible);

    if app.filtered.is_empty() {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Line::from(Span::styled(
                " Units ",
                Style::default().add_modifier(Modifier::BOLD),
            )));
        let text = Paragraph::new(Line::from(Span::styled(
            "No units matched. Press x to show every unit.",
            Style::default().fg(Color::DarkGray),
        )))
        .block(block)
        .alignment(Alignment::Center);
        frame.render_widget(text, area);
        return;
    }

    let widths = if narrow {
        vec![
            Constraint::Min(16),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(5),
            Constraint::Length(12),
        ]
    } else {
        vec![
            Constraint::Min(20),
            Constraint::Length(5),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(6),
            Constraint::Length(18),
        ]
    };

    let header_cells: Vec<&str> = if narrow {
        vec!["UNIT", "RSS", "VRAM", "CPU%", "FLAGS"]
    } else {
        vec!["UNIT", "PROCS", "RSS", "VRAM", "CPU%", "FLAGS"]
    };
    let header = Row::new(header_cells.into_iter().map(Cell::from).collect::<Vec<_>>()).style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    let reserve = app.snapshot.reserve_mb;
    let self_unit = app.snapshot.self_unit.clone();
    let mut rows = Vec::new();
    for unit in app.visible_units(visible) {
        let selected = app
            .selected_unit()
            .map(|current| current.unit == unit.unit)
            .unwrap_or(false);
        rows.push(unit_row(unit, selected, narrow, reserve, &self_unit));
    }

    let title = format!(
        " Units ({}){} ",
        app.filtered.len(),
        if app.show_all { " · all" } else { "" }
    );
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(1)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Line::from(Span::styled(
                    title,
                    Style::default().add_modifier(Modifier::BOLD),
                ))),
        );
    frame.render_widget(table, area);
}

fn unit_row(
    unit: &UnitStat,
    selected: bool,
    narrow: bool,
    reserve: u64,
    self_unit: &str,
) -> Row<'static> {
    let flags = flags_cell(unit, reserve, self_unit);
    let vram = if unit.vram == 0 {
        "-".to_string()
    } else {
        format!("{} MiB", unit.vram)
    };
    let category = category(unit, self_unit);
    let display = if unit.display.is_empty() {
        unit.unit.clone()
    } else {
        unit.display.clone()
    };
    let name = Cell::from(Line::from(Span::styled(
        display,
        Style::default().fg(category_color(category)),
    )));
    let mut cells = vec![
        name,
        Cell::from(human_bytes(unit.rss)),
        Cell::from(vram),
        Cell::from(format!("{:.1}", unit.cpu)),
    ];
    if !narrow {
        cells.insert(1, Cell::from(unit.procs.to_string()));
    }
    cells.push(Cell::from(flags));

    let style = if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };
    Row::new(cells).style(style)
}

fn flags_cell(unit: &UnitStat, reserve: u64, self_unit: &str) -> Line<'static> {
    let mut flags: Vec<(&'static str, Color)> = Vec::new();
    if unit.stopped {
        flags.push(("STOPPED", Color::DarkGray));
    }
    if unit.frozen {
        flags.push(("FROZEN", Color::Yellow));
    }
    match category(unit, self_unit) {
        Category::Model => flags.push(("MODEL", Color::Magenta)),
        Category::Desktop => flags.push((
            if is_compositor_unit(&unit.unit) {
                "COMP"
            } else {
                "DESKTOP"
            },
            Color::LightBlue,
        )),
        Category::Container => flags.push(("CONTAINER", Color::Green)),
        Category::System => flags.push(("SYSTEM", Color::Yellow)),
        Category::SelfUnit => flags.push(("SELF", Color::LightCyan)),
        Category::Kernel => flags.push(("KERNEL", Color::DarkGray)),
        Category::User => {}
    }
    if !unit.user
        && !unit.stopped
        && !matches!(
            category(unit, self_unit),
            Category::System | Category::Kernel
        )
    {
        flags.push(("SYSTEM", Color::Gray));
    }
    if unit.vram >= reserve && reserve > 0 {
        flags.push(("HEAVY", Color::Red));
    }
    let mut spans = Vec::new();
    for (index, (label, color)) in flags.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(*label, Style::default().fg(*color)));
    }
    Line::from(spans)
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App, narrow: bool) {
    let hints = if narrow {
        "j/k select · p processes · Enter actions · s stop · r start · q quit"
    } else {
        "j/k select · p processes · Enter actions · s stop · r start · f freeze · t thaw · ! emergency · ? help · q quit"
    };
    let mut spans = Vec::new();
    if let Some(count) = app.snapshot.gpu_alloc_failures {
        if count > 0 {
            spans.push(Span::styled(
                format!("GPU alloc {count}  ·  "),
                Style::default().fg(Color::Yellow),
            ));
        }
    }
    if !app.status.is_empty() {
        let color = match app.status_kind {
            StatusKind::Info => Color::Cyan,
            StatusKind::Ok => Color::Green,
            StatusKind::Err => Color::Red,
        };
        spans.push(Span::styled(app.status.clone(), Style::default().fg(color)));
        spans.push(Span::styled("  ·  ", Style::default().fg(Color::DarkGray)));
    }
    spans.push(Span::styled(hints, Style::default().fg(Color::DarkGray)));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn display_for(app: &App, unit: &str) -> String {
    app.find_unit(unit)
        .map(|row| {
            if row.display.is_empty() {
                unit.to_string()
            } else {
                row.display.clone()
            }
        })
        .unwrap_or_else(|| unit.to_string())
}

fn draw_menu(frame: &mut Frame, area: Rect, app: &App, unit: &str, index: usize) {
    let popup = centered(area, 58, 9);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Line::from(Span::styled(
            format!(" Actions · {} ", display_for(app, unit)),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines = Vec::new();
    for (position, action) in Action::ALL.iter().enumerate() {
        let selected = position == index;
        let style = if selected {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(
            format!(" {:<14} {}", action.label(), action.hint()),
            style,
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " \u{2191}/\u{2193} choose · Enter run · Esc cancel",
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_confirm(frame: &mut Frame, area: Rect, app: &App, action: Action, unit: &str) {
    let unit = display_for(app, unit);
    let popup = centered(area, 68, 9);
    frame.render_widget(Clear, popup);
    let color = if action == Action::Kill {
        Color::Red
    } else {
        Color::Yellow
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(Line::from(Span::styled(
            format!(" Confirm {} ", action.label().to_lowercase()),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let body = match action {
        Action::Kill => format!(
            "Force-kill {unit}?\nSIGKILL is sent to every process in the unit. Unsaved work is lost."
        ),
        Action::Stop => format!(
            "Suspend {unit}?\nThe unit is stopped and its RAM/VRAM is freed. Start it again with r."
        ),
        _ => format!("Run {} on {unit}?", action.label()),
    };
    let text = vec![
        Line::from(Span::styled(body, Style::default().fg(color))),
        Line::from(""),
        Line::from(Span::styled(
            "Enter/y confirm · n/Esc cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), inner);
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let popup = centered(area, 76, 23);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Line::from(Span::styled(
            " SysLens keys ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let lines = vec![
        Line::from("j / \u{2193}      next unit"),
        Line::from("k / \u{2191}      previous unit"),
        Line::from("g / G      first / last unit"),
        Line::from("PgUp/PgDn  page through units"),
        Line::from("Enter / m  actions menu"),
        Line::from("s          suspend (stop, frees RAM/VRAM)"),
        Line::from("r          resume (start)"),
        Line::from("f / t      freeze / thaw (pause in memory)"),
        Line::from("K          force kill (SIGKILL)"),
        Line::from("p          processes for the selected unit"),
        Line::from("!          emergency panel (guard tools)"),
        Line::from("x          show all / hide idle units"),
        Line::from("?          close help"),
        Line::from("q          quit"),
        Line::from(""),
        Line::from(Span::styled(
            "Colors: magenta model · blue desktop · green container · yellow system · cyan self · yellow frozen.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "Guard freezes the largest model near the reserve; if still critical after the grace period it stops it. t thaws and cancels; ! disables the guard.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "Destructive actions need confirmation. Actions target systemd --user units; system model units need --allow-system (root or passwordless sudo). Compositor, desktop, session, kernel, and background system units stay protected.",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn text_or_na(value: Option<u64>, suffix: &str) -> String {
    match value {
        Some(value) => format!("{value}{suffix}"),
        None => "n/a".to_string(),
    }
}

fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64
    }
}

fn level_color(level: Level) -> Color {
    match level {
        Level::Crit => Color::Red,
        Level::Warn => Color::Yellow,
        Level::Info => Color::Cyan,
        Level::Ok => Color::Green,
    }
}

fn category_color(category: Category) -> Color {
    match category {
        Category::Model => Color::Magenta,
        Category::Desktop => Color::LightBlue,
        Category::Container => Color::Green,
        Category::System => Color::Yellow,
        Category::SelfUnit => Color::LightCyan,
        Category::Kernel => Color::DarkGray,
        Category::User => Color::Reset,
    }
}

fn guard_color(label: &str) -> Color {
    if label.contains("GUARD") || label.contains("stop in") {
        Color::Red
    } else if label.contains("paused") || label.contains("frozen") {
        Color::Yellow
    } else if label.contains("off") {
        Color::DarkGray
    } else {
        Color::Green
    }
}

fn draw_toast(frame: &mut Frame, area: Rect, toast: &Toast) {
    let width = area.width.saturating_sub(4).min(64);
    if width < 24 || area.height < 8 {
        return;
    }
    let height = 6.min(area.height.saturating_sub(4));
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width + 2),
        y: area.y + 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let color = level_color(toast.level);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(Line::from(Span::styled(
            " SysLens alert ",
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            toast.text.clone(),
            Style::default().fg(color),
        )))
        .wrap(Wrap { trim: true }),
        inner,
    );
}

fn draw_confirm_all(frame: &mut Frame, area: Rect, action: Action, units: &[String]) {
    let height = (units.len() as u16 + 7).min(14);
    let popup = centered(area, 72, height);
    frame.render_widget(Clear, popup);
    let color = Color::Yellow;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(Line::from(Span::styled(
            format!(
                " Confirm {} {} units ",
                action.label().to_lowercase(),
                units.len()
            ),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines = vec![Line::from(Span::styled(
        format!(
            "{} {} model units? This affects every process in those units.",
            action.label(),
            units.len()
        ),
        Style::default().fg(color),
    ))];
    for unit in units.iter().take(6) {
        lines.push(Line::from(Span::styled(
            format!("  • {unit}"),
            Style::default().fg(color),
        )));
    }
    if units.len() > 6 {
        lines.push(Line::from(Span::styled(
            format!("  … and {} more", units.len() - 6),
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter/y confirm · n/Esc cancel",
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn draw_processes(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    unit: &str,
    index: usize,
    offset: usize,
) {
    let table_unit = app.find_unit(unit);
    let display = table_unit
        .map(|row| row.display.clone())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| unit.to_string());
    let origin = table_unit
        .map(|row| row.origin.clone())
        .unwrap_or_else(|| unit.to_string());

    let mut rows: Vec<&ProcRow> = app
        .snapshot
        .procs
        .iter()
        .filter(|proc_row| proc_row.unit == unit)
        .collect();
    rows.sort_by_key(|p| std::cmp::Reverse((p.vram, p.rss)));

    let width = area.width.saturating_sub(4).clamp(40, 110);
    let popup = centered(area, width, 20);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Line::from(Span::styled(
            format!(" Processes · {display} · {} ", rows.len()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(13),
            Constraint::Length(3),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            origin,
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[0],
    );

    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "No active processes for this unit.",
                Style::default().fg(Color::DarkGray),
            )))
            .alignment(Alignment::Center),
            chunks[1],
        );
    } else {
        let header = Row::new(vec![
            Cell::from("PID"),
            Cell::from("RSS"),
            Cell::from("VRAM"),
            Cell::from("CPU%"),
            Cell::from("COMMAND"),
        ])
        .style(
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );

        let visible = chunks[1].height.saturating_sub(1) as usize;
        let start = offset.min(rows.len());
        let end = (start + visible).min(rows.len());
        let mut table_rows = Vec::new();
        for (position, proc_row) in rows[start..end].iter().enumerate() {
            let selected = start + position == index;
            let command = {
                let command = read_cmdline(proc_row.pid);
                if command.is_empty() {
                    proc_row.comm.clone()
                } else {
                    command
                }
            };
            let vram = if proc_row.vram == 0 {
                "-".to_string()
            } else {
                format!("{} MiB", proc_row.vram)
            };
            let cells = vec![
                Cell::from(proc_row.pid.to_string()),
                Cell::from(human_bytes(proc_row.rss)),
                Cell::from(vram),
                Cell::from(format!("{:.1}", proc_row.cpu)),
                Cell::from(command),
            ];
            let style = if selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            table_rows.push(Row::new(cells).style(style));
        }

        let table = Table::new(
            table_rows,
            [
                Constraint::Length(7),
                Constraint::Length(9),
                Constraint::Length(9),
                Constraint::Length(6),
                Constraint::Min(30),
            ],
        )
        .header(header)
        .column_spacing(1);
        frame.render_widget(table, chunks[1]);
    }

    let selected_command = rows.get(index).map(|proc_row| {
        let command = read_cmdline(proc_row.pid);
        if command.is_empty() {
            proc_row.comm.clone()
        } else {
            command
        }
    });
    let footer = vec![
        Line::from(Span::styled(
            selected_command
                .map(|command| format!("cmd: {command}"))
                .unwrap_or_else(|| "No process selected.".to_string()),
            Style::default().fg(Color::Gray),
        )),
        Line::from(Span::styled(
            "j/k scroll · PgUp/PgDn page · Esc/p close",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(Paragraph::new(footer), chunks[2]);
}

fn draw_emergency(frame: &mut Frame, area: Rect, app: &App, index: usize) {
    let popup = centered(area, 66, 13);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red))
        .title(Line::from(Span::styled(
            " Emergency ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines = Vec::new();
    for (position, label) in app.emergency_labels().iter().enumerate() {
        let selected = position == index;
        let style = if selected {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(
            format!(" {}) {label}", position + 1),
            style,
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " 1-8 or ↑/↓ choose · Enter run · Esc close",
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn compact(bytes: u64) -> String {
    let text = human_bytes(bytes);
    if let Some(stripped) = text.strip_suffix('B') {
        stripped.to_string()
    } else {
        text
    }
}

fn fit(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut short: String = text.chars().take(width.saturating_sub(1)).collect();
    short.push('…');
    short
}

fn ansi_category(category: Category) -> &'static str {
    match category {
        Category::Model => "35",
        Category::Desktop => "34",
        Category::Container => "32",
        Category::System => "33",
        Category::SelfUnit => "36",
        Category::Kernel => "90",
        Category::User => "",
    }
}

fn ansi_level(level: Level) -> &'static str {
    match level {
        Level::Crit => "31;1",
        Level::Warn => "33",
        Level::Info => "36",
        Level::Ok => "32",
    }
}

fn paint(text: &str, code: &str, color: bool) -> String {
    if color && !code.is_empty() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn render_plain(snapshot: &Snapshot, color: bool) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "SysLens {} snapshot {}  {}  ({} cores)\n",
        env!("CARGO_PKG_VERSION"),
        snapshot.clock,
        snapshot.host,
        snapshot.ncpu
    ));
    out.push_str(&format!(
        "CPU   {:5.1}%  load {:.2} {:.2} {:.2}\n",
        snapshot.cpu_pct, snapshot.load[0], snapshot.load[1], snapshot.load[2]
    ));
    out.push_str(&format!(
        "RAM   {} / {}  available {}\n",
        human_bytes(snapshot.ram.used),
        human_bytes(snapshot.ram.total),
        human_bytes(snapshot.ram.available)
    ));
    let mut swap_line = format!(
        "SWAP  {} / {} ({:.0}%)",
        human_bytes(snapshot.swap.used),
        human_bytes(snapshot.swap.total),
        snapshot.swap.percent
    );
    if let Some(zram) = snapshot.zram {
        swap_line.push_str(&format!(
            "  zram {} -> {}",
            human_bytes(zram.data),
            human_bytes(zram.compr)
        ));
    }
    out.push_str(&swap_line);
    out.push('\n');
    match &snapshot.gpu {
        Some(gpu) => out.push_str(&format!(
            "GPU   {}  VRAM {} / {} MiB (free {}, reserve {})  util {}  {}  unaccounted {} MiB\n",
            gpu.name,
            gpu.used,
            gpu.total,
            gpu.free,
            snapshot.reserve_mb,
            text_or_na(gpu.util, "%"),
            text_or_na(gpu.temp, "C"),
            snapshot.gpu_unattributed
        )),
        None => out.push_str("GPU   not detected\n"),
    }
    if let Some(count) = snapshot.gpu_alloc_failures {
        out.push_str(&format!("GPU alloc failures (15m) {count}\n"));
    }

    out.push('\n');
    out.push_str(&format!(
        "{:<32} {:>5} {:>9} {:>9} {:>7}  FLAGS\n",
        "UNIT", "PROCS", "RSS", "VRAM", "CPU%"
    ));
    for unit in snapshot.units.iter().take(20) {
        let category = category(unit, &snapshot.self_unit);
        let mut flags = Vec::new();
        if unit.stopped {
            flags.push("STOPPED");
        }
        if unit.frozen {
            flags.push("FROZEN");
        }
        match category {
            Category::Model => flags.push("MODEL"),
            Category::Desktop => flags.push(if is_compositor_unit(&unit.unit) {
                "COMP"
            } else {
                "DESKTOP"
            }),
            Category::Container => flags.push("CONTAINER"),
            Category::System => flags.push("SYSTEM"),
            Category::SelfUnit => flags.push("SELF"),
            Category::Kernel => flags.push("KERNEL"),
            Category::User => {}
        }
        if !unit.user
            && !unit.stopped
            && !matches!(
                category,
                Category::System | Category::Kernel | Category::Container
            )
        {
            flags.push("SYSTEM");
        }
        let vram = if unit.vram == 0 {
            "-".to_string()
        } else {
            format!("{} MiB", unit.vram)
        };
        let display = if unit.display.is_empty() {
            unit.unit.as_str()
        } else {
            unit.display.as_str()
        };
        let name = format!("{:<32}", fit(display, 32));
        let flags = flags.join(", ");
        out.push_str(&format!(
            "{} {:>5} {:>9} {:>9} {:>7.1}  {}\n",
            paint(&name, ansi_category(category), color),
            unit.procs,
            human_bytes(unit.rss),
            vram,
            unit.cpu,
            paint(&flags, ansi_category(category), color)
        ));
    }

    out.push_str("\nWarnings\n");
    for warning in &snapshot.warnings {
        let text = format!("[{}] {}", warning.level.label(), warning.text);
        out.push_str(&format!(
            "  {}\n",
            paint(&text, ansi_level(warning.level), color)
        ));
    }
    out
}
