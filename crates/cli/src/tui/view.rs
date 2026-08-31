//! Drawing. Everything has to survive a resize and stay readable at 80x24.

use appimg_core::list::{Health, InstalledApp};
use appimg_core::{install, update, MAIN_CATEGORIES};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use super::app::{App, Mode};
use super::form::Field;
use super::theme;
use crate::ui::human_size;

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    match &app.mode {
        Mode::Browse(browser) => draw_browser(frame, layout[0], browser),
        Mode::Form(form) => draw_form(frame, layout[0], form),
        Mode::Preview(form) => draw_preview(frame, layout[0], app, &form.request),
        _ => draw_list(frame, layout[0], app),
    }

    draw_status(frame, layout[1], app);
    draw_keys(frame, layout[2], app);

    match &app.mode {
        Mode::Details => draw_details(frame, area, app),
        Mode::Help => draw_help(frame, area),
        Mode::Confirm { question, .. } => draw_confirm(frame, area, question),
        _ => {}
    }
}

fn draw_list(frame: &mut Frame, area: Rect, app: &App) {
    let header =
        Row::new(vec!["NAME", "VERSION", "SIZE", "CATEGORY", "STATUS"]).style(theme::title());

    let rows: Vec<Row> = app
        .visible
        .iter()
        .enumerate()
        .filter_map(|(position, index)| app.apps.get(*index).map(|app| (position, app)))
        .map(|(position, installed)| {
            let row = Row::new(vec![
                installed.name.clone(),
                installed.version.clone().unwrap_or_else(|| "-".to_string()),
                installed.size_bytes.map(human_size).unwrap_or_else(|| "-".to_string()),
                installed.categories.first().cloned().unwrap_or_else(|| "-".to_string()),
                status_of(installed),
            ]);
            if position == app.selected {
                row.style(theme::selected())
            } else {
                row.style(theme::base())
            }
        })
        .collect();

    if rows.is_empty() {
        let message = if app.apps.is_empty() {
            "Nothing installed yet. Press i to install an AppImage."
        } else {
            "No application matches the search."
        };
        frame.render_widget(
            Paragraph::new(message).style(theme::dim()).block(list_block(app)),
            area,
        );
        return;
    }

    let widths = [
        Constraint::Percentage(34),
        Constraint::Length(10),
        Constraint::Length(9),
        Constraint::Percentage(20),
        Constraint::Percentage(26),
    ];
    frame.render_widget(Table::new(rows, widths).header(header).block(list_block(app)), area);
}

fn list_block(app: &App) -> Block<'static> {
    let title = if app.filter.is_empty() {
        format!(" appimg — {} installed ", app.apps.len())
    } else {
        format!(" appimg — search: {} ", app.filter)
    };
    Block::default().borders(Borders::ALL).border_style(theme::border()).title(title)
}

fn status_of(app: &InstalledApp) -> String {
    match app.health {
        Health::Ok => update::source_for(app).describe(),
        Health::MissingBinary => "broken: no binary".to_string(),
        Health::Incomplete => "broken: entry".to_string(),
    }
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let text = match (&app.mode, &app.status) {
        (Mode::Search, _) => format!("/{}", app.filter),
        (_, Some(status)) => status.clone(),
        (_, None) => match app.selected_app() {
            Some(selected) => selected.appimage_path.to_string_lossy().into_owned(),
            None => String::new(),
        },
    };
    frame.render_widget(Paragraph::new(text).style(theme::dim()), area);
}

fn draw_keys(frame: &mut Frame, area: Rect, app: &App) {
    let keys = match app.mode {
        Mode::List => {
            "q quit  ? help  i install  u/U update  d remove  e edit  / search  enter details"
        }
        Mode::Search => "type to filter  enter keep  esc clear",
        Mode::Details | Mode::Help => "any key closes",
        Mode::Browse(_) => "j/k move  enter open  h parent  esc back",
        Mode::Form(_) => "tab next field  space toggle  enter preview  esc cancel",
        Mode::Preview(_) => "enter install  e back to the form  esc cancel",
        Mode::Confirm { .. } => "y confirm  any other key cancels",
    };
    frame.render_widget(Paragraph::new(keys).style(theme::dim()), area);
}

fn draw_browser(frame: &mut Frame, area: Rect, browser: &super::browser::Browser) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border())
        .title(format!(" {} ", browser.directory.display()));

    if let Some(error) = &browser.error {
        frame.render_widget(Paragraph::new(error.clone()).block(block), area);
        return;
    }
    if browser.entries.is_empty() {
        frame.render_widget(
            Paragraph::new("No directories and no AppImages here.")
                .style(theme::dim())
                .block(block),
            area,
        );
        return;
    }

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let height = inner.height as usize;
    let first = browser.selected.saturating_sub(height.saturating_sub(1));
    let lines: Vec<Line> = browser
        .entries
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(index, entry)| {
            let style = if index == browser.selected {
                theme::selected()
            } else if entry.is_dir {
                theme::accent()
            } else {
                theme::base()
            };
            Line::from(Span::styled(entry.label.clone(), style))
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_form(frame: &mut Frame, area: Rect, form: &super::form::InstallForm) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border())
        .title(format!(" Install {} ", form.source().display()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    for field in Field::ALL {
        let focused = field == form.field;
        let value = match field {
            Field::Categories => {
                if form.request.categories.is_empty() {
                    "(none, Utility is used)".to_string()
                } else {
                    form.request.categories.join(", ")
                }
            }
            other => form.text(other),
        };
        let marker = if focused { ">" } else { " " };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} {:<11}", field.label()), theme::title()),
            Span::styled(value, if focused { theme::accent() } else { theme::base() }),
        ]));
    }

    if form.field == Field::Categories {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("left/right move, space toggles:", theme::dim())));
        lines.extend(category_lines(form, inner.width as usize));
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Arguments are passed to the AppImage. Icon takes a path, empty uses the embedded one.",
            theme::dim(),
        )));
        let source = if form.info.extract_root().is_some() {
            "Prefilled from the desktop entry inside the AppImage."
        } else {
            "The AppImage did not extract, these values are guesses."
        };
        lines.push(Line::from(Span::styled(source, theme::dim())));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn category_lines(form: &super::form::InstallForm, width: usize) -> Vec<Line<'static>> {
    let per_line = (width / 16).max(1);
    let mut lines = Vec::new();
    let mut spans: Vec<Span> = Vec::new();

    for (index, category) in MAIN_CATEGORIES.iter().enumerate() {
        let mark = if form.is_selected(category) { "[x]" } else { "[ ]" };
        let style = if index == form.category_cursor { theme::selected() } else { theme::base() };
        spans.push(Span::styled(format!("{mark} {category:<11} "), style));
        if spans.len() == per_line {
            lines.push(Line::from(std::mem::take(&mut spans)));
        }
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}

fn draw_preview(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    request: &appimg_core::install::InstallRequest,
) {
    let block =
        Block::default().borders(Borders::ALL).border_style(theme::border()).title(" Preview ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = match install::plan(&app.paths, request) {
        Ok(plan) => {
            let mut text = plan.desktop_entry.to_string();
            if plan.already_installed {
                text.push_str("\nThis replaces the installed version of the same slug.");
            }
            text
        }
        Err(error) => format!("This cannot be installed: {error}"),
    };
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
}

fn draw_details(frame: &mut Frame, area: Rect, app: &App) {
    let Some(selected) = app.selected_app() else {
        return;
    };

    let mut lines = vec![
        field_line("Name", &selected.name),
        field_line("Slug", &selected.slug),
        field_line("Version", selected.version.as_deref().unwrap_or("-")),
        field_line("Comment", selected.comment.as_deref().unwrap_or("-")),
        field_line("Categories", &selected.categories.join(", ")),
        field_line("Source", selected.origin.as_deref().unwrap_or("-")),
        field_line("Update", &update::source_for(selected).describe()),
        field_line("Installed", selected.installed_at.as_deref().unwrap_or("-")),
        field_line("Binary", &selected.appimage_path.to_string_lossy()),
        field_line("Entry", &selected.desktop_entry_path.to_string_lossy()),
    ];
    if selected.is_broken() {
        lines.push(Line::from(Span::styled(
            "The AppImage this entry points at is gone. Press d to clean it up.",
            theme::warning(),
        )));
    }

    popup(frame, area, " Details ", lines, 80, 60);
}

fn field_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<12}"), theme::title()),
        Span::raw(value.to_string()),
    ])
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let lines: Vec<Line> = [
        ("j/k, arrows", "move"),
        ("g/G", "first/last"),
        ("enter", "details"),
        ("i", "install an AppImage"),
        ("u / U", "update the selection / everything"),
        ("e", "edit the desktop entry in $EDITOR"),
        ("d", "remove, after a confirmation"),
        ("/", "search, esc clears it"),
        ("r", "reload the list"),
        ("q", "quit"),
    ]
    .iter()
    .map(|(keys, what)| field_line(keys, what))
    .collect();

    popup(frame, area, " Keys ", lines, 60, 60);
}

fn draw_confirm(frame: &mut Frame, area: Rect, question: &str) {
    let lines = vec![
        Line::from(question.to_string()),
        Line::from(""),
        Line::from(Span::styled("y confirms, any other key cancels", theme::dim())),
    ];
    popup(frame, area, " Confirm ", lines, 60, 30);
}

fn popup(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    lines: Vec<Line<'static>>,
    width_percent: u16,
    height_percent: u16,
) {
    let rect = centered(area, width_percent, height_percent);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border())
        .title(title.to_string())
        .title_alignment(Alignment::Left);

    frame.render_widget(Clear, rect);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// A centered rectangle that never grows past the terminal, so 80x24 works.
fn centered(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let width = (area.width * width_percent / 100).min(area.width).max(1);
    let height = (area.height * height_percent / 100).min(area.height).max(1);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}
