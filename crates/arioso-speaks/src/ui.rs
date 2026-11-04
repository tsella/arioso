//! Drawing, and the keyboard.
//!
//! The palette is the transcriber's own: petrol, tan, plum and moss, with the muted greys a
//! terminal already has. One accent carries the live state; everything else is quiet, so the
//! waveform is the only thing that moves.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::app::{Action, App, Focus, TextBuffer};
use crate::catalog;
use crate::dither;
use crate::save::Format;

/// The project's petrol.
const PETROL: Color = Color::Indexed(31);
/// The project's tan.
const TAN: Color = Color::Indexed(137);
/// The project's plum.
const PLUM: Color = Color::Indexed(97);
/// The project's moss.
const MOSS: Color = Color::Indexed(65);
/// Primary text.
const INK: Color = Color::Indexed(252);
/// Secondary text: sizes, licences, hints.
const MUTED: Color = Color::Indexed(245);
/// Rules and the empty half of a bar.
const FAINT: Color = Color::Indexed(238);
/// The highlighted line.
const CREAM: Color = Color::Indexed(223);

/// How wide the wordmark is, including the two columns of margin before it.
const MARK_WIDTH: u16 = 32;

/// Below this width the mark is dropped and the signal takes the row.
///
/// A forty-three column mark in a sixty column window leaves a strip for everything else.
const MARK_FLOOR: u16 = 70;

/// Draw one frame.
pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let rows = Layout::vertical([
        Constraint::Length(6),
        Constraint::Min(6),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(area);

    draw_header(frame, rows[0], app);
    draw_body(frame, rows[1], app);
    draw_actions(frame, rows[2], app);
    draw_hints(frame, rows[3], app);

    if app.save_prompt {
        draw_save_prompt(frame, area);
    }
}

/// The wordmark, the waveform, and the line under them.
fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);

    // The mark on the left and the live signal on the right, sharing the rows. A window
    // too narrow for both keeps the signal: it is the part that changes.
    if area.width < MARK_FLOOR {
        draw_waveform(frame, rows[0], app);
    } else {
        let columns =
            Layout::horizontal([Constraint::Length(MARK_WIDTH), Constraint::Min(8)]).split(rows[0]);
        draw_wordmark(frame, columns[0]);
        draw_waveform(frame, columns[1], app);
    }

    // The rule carries the facts about the voice, right-aligned where the eye looks last.
    let voice = app.current_voice();
    let facts = match voice {
        Some(voice) => {
            let speaker = match voice.speakers.len() {
                0 | 1 => String::new(),
                count => format!(
                    " · speaker {}/{} {}",
                    app.sid + 1,
                    count,
                    voice
                        .speakers
                        .get(app.sid as usize)
                        .cloned()
                        .unwrap_or_default()
                ),
            };
            format!(
                "{}{} · {} · {} ",
                voice.id,
                speaker,
                catalog::size(voice.bytes),
                voice.licence_label()
            )
        }
        None => "no voice in this catalog ".to_owned(),
    };
    let rule_width = rows[1].width as usize;
    let facts_width = facts.chars().count();
    let rule: String = "─".repeat(rule_width.saturating_sub(facts_width + 2));
    let line = Line::from(vec![
        Span::styled(format!("  {rule} "), Style::default().fg(FAINT)),
        Span::styled(
            facts,
            Style::default().fg(if voice.is_some_and(|voice| voice.licence_needs_a_look()) {
                TAN
            } else {
                MUTED
            }),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), rows[1]);

    // The tagline, under the rule, in the quietest ink the terminal has.
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled("local speech, in the terminal", Style::default().fg(FAINT)),
            Span::styled(
                app.log_file
                    .as_ref()
                    .map(|path| format!("   log {}", path.display()))
                    .unwrap_or_default(),
                Style::default().fg(Color::Indexed(240)),
            ),
        ])),
        rows[2],
    );
}

/// The mark.
///
/// Letters made of block glyphs read as a fence at this size: an `a` and an `o` are both a
/// rectangle with a hole, and at three rows there is no room for the shapes that tell them
/// apart. The drawn form below keeps the strokes of the letters, so the word is read rather
/// than guessed at, and it is still ASCII, so it survives any terminal font.
///
/// The rows fade downwards, cream to petrol to a darker shade, so the mark looks lit from
/// above instead of stamped.
fn draw_wordmark(frame: &mut Frame, area: Rect) {
    // figlet's standard font, which already sets the letters against each other: the `r`'s
    // right bar is the `i`'s stem, and the `i`'s stem is the `o`'s left edge, so one stroke
    // serves two letters. This is that font's output, not a hand-kerning of it.
    //
    // It is four rows where the font uses five, because the `i`'s dot is set on the same row
    // as the top of the letters beside it. A header is not a poster.
    const MARK: [&str; 4] = [
        "  __ _ _ __(_) ___  ___  ___",
        " / _` | '__| |/ _ \\/ __|/ _ \\",
        "| (_| | |  | | (_) \\__ \\ (_) |",
        " \\__,_|_|  |_|\\___/|___/\\___/",
    ];
    let lines: Vec<Line> = MARK
        .iter()
        .enumerate()
        .map(|(row, text)| {
            let ink = match row {
                0 => CREAM,
                1 => PETROL,
                2 => Color::Indexed(24),
                _ => Color::Indexed(23),
            };
            Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(*text, Style::default().fg(ink)),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// The live signal, drawn from the audio that was rendered.
///
/// While something plays it is a three-second window that scrolls, with a playhead down the
/// middle. When nothing has been rendered yet it is a standby ripple: an idle screen with no
/// motion reads as a hung program, and the ripple is labelled in the code as decoration so
/// nobody later mistakes it for a level.
fn draw_waveform(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width as usize;
    if width == 0 {
        return;
    }
    let live = app.player.playing();
    let levels = if app.audio.is_none() && !live {
        standby(width, app.frame)
    } else {
        app.player.levels(width)
    };
    let grid = dither::waveform_levels(&levels, area.height.max(1) as usize);
    let playhead = if live { Some(width / 2) } else { None };

    let lines: Vec<Line> = grid
        .iter()
        .enumerate()
        .map(|(row, cells)| {
            let mut spans = Vec::with_capacity(cells.len() + 1);
            for (col, level) in cells.iter().enumerate() {
                let glyph = dither::cell(*level, row, col);
                let ink = if playhead == Some(col) {
                    CREAM
                } else {
                    waveform_ink(*level, live || app.audio.is_some())
                };
                spans.push(Span::styled(glyph, Style::default().fg(ink)));
            }
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// A peak is warm, the body is the accent, and a still envelope is grey.
fn waveform_ink(level: f32, has_audio: bool) -> Color {
    if !has_audio {
        return MUTED;
    }
    match level {
        level if level > 0.92 => CREAM,
        level if level > 0.6 => PLUM,
        level if level > 0.0 => PETROL,
        _ => FAINT,
    }
}

/// A slow travelling ripple for a window with nothing to say yet.
///
/// This is decoration, not a signal: it never appears once audio exists, and its amplitude
/// is kept low so it reads as an idle screen rather than as a level meter.
fn standby(width: usize, frame: u64) -> Vec<f32> {
    let phase = frame as f32 * 0.09;
    (0..width)
        .map(|col| {
            let travel = col as f32 * 0.22 - phase;
            0.16 + 0.14 * travel.sin().abs()
        })
        .collect()
}

/// The voice list, the text, and the phonemes.
fn draw_body(frame: &mut Frame, area: Rect, app: &App) {
    let columns = Layout::horizontal([Constraint::Length(38), Constraint::Min(20)]).split(area);
    draw_voices(frame, columns[0], app);
    let right = Layout::vertical([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(columns[1]);
    draw_text(frame, right[0], app);
    draw_phonemes(frame, right[1], app);
}

/// The engine tabs and the filterable list.
fn draw_voices(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Voices;
    let block = Block::bordered()
        .border_style(Style::default().fg(if focused { PETROL } else { FAINT }))
        .title(Line::from(vec![
            Span::styled(" engine ", Style::default().fg(MUTED)),
            Span::styled(
                if focused { "· filter: " } else { "· " },
                Style::default().fg(MUTED),
            ),
            Span::styled(app.filter.clone(), Style::default().fg(INK)),
        ]));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(inner);

    // Tabs: the engine that is showing is the one in the accent.
    let mut tabs = Vec::new();
    for engine in catalog::ENGINES {
        let active = engine == app.engine;
        tabs.push(Span::styled(
            format!(" {engine} "),
            if active {
                Style::default().fg(Color::Indexed(16)).bg(PETROL)
            } else {
                Style::default().fg(MUTED)
            },
        ));
        tabs.push(Span::styled(" ", Style::default()));
    }
    frame.render_widget(Paragraph::new(Line::from(tabs)), rows[0]);

    let list_height = rows[1].height as usize;
    // Centred rather than pinned to the bottom: the voices either side of the selection are
    // what tells a person where they are in a list of two hundred.
    let first = app
        .selected
        .saturating_sub(list_height / 2)
        .min(app.visible.len().saturating_sub(list_height));
    let lines: Vec<Line> = app
        .visible
        .iter()
        .skip(first)
        .take(list_height)
        .enumerate()
        .map(|(offset, index)| {
            let voice = &app.voices[*index];
            let chosen = first + offset == app.selected;
            let installed = voice.installed;
            let style = if chosen {
                Style::default().bg(Color::Indexed(236)).fg(CREAM)
            } else if installed {
                Style::default().fg(INK)
            } else {
                Style::default().fg(MUTED)
            };
            let mark = if chosen {
                Span::styled("▸ ", Style::default().fg(PETROL))
            } else {
                Span::styled("  ", Style::default())
            };
            let name = Span::styled(voice.display_name(), style);
            let detail = Span::styled(
                format!(
                    "  {} {}{}",
                    catalog::size(voice.bytes),
                    voice.licence_chip(),
                    if voice.installed { "" } else { " ↓" }
                ),
                if chosen {
                    style
                } else {
                    Style::default().fg(if voice.licence_needs_a_look() {
                        TAN
                    } else {
                        FAINT
                    })
                },
            );
            Line::from(vec![mark, name, detail])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), rows[1]);
}

/// The text to say.
fn draw_text(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Text;
    let block = Block::bordered()
        .border_style(Style::default().fg(if focused { PETROL } else { FAINT }))
        .title(Line::from(vec![
            Span::styled(" text ", Style::default().fg(MUTED)),
            Span::styled(
                format!("· {} characters ", app.text.text().chars().count()),
                Style::default().fg(FAINT),
            ),
        ]));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width.max(1) as usize;
    let lines = wrapped_lines(&app.text, width);
    let visible = inner.height as usize;
    // Keep the cursor's line in view.
    let cursor_line = lines
        .iter()
        .position(|(_, range)| range.contains(&app.text.cursor))
        .unwrap_or(lines.len().saturating_sub(1));
    let first = cursor_line.saturating_sub(visible.saturating_sub(1));

    let rendered: Vec<Line> = lines
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .map(|(number, (text, range))| {
            let mut spans = vec![Span::styled(text.clone(), Style::default().fg(INK))];
            if focused && number == cursor_line {
                let column = app.text.cursor.saturating_sub(range.start);
                spans = with_caret(text, column);
            }
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(rendered), inner);
}

/// The phonemes, which are editable and are what gets spoken.
fn draw_phonemes(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Phonemes;
    let edited = app.phonemes_edited;
    let block = Block::bordered()
        .border_style(Style::default().fg(if focused || edited { PLUM } else { FAINT }))
        .title(Line::from(vec![
            Span::styled(" phonemes ", Style::default().fg(MUTED)),
            Span::styled(
                if edited {
                    "· edited · spoken as written "
                } else {
                    "· generated "
                },
                Style::default().fg(if edited { PLUM } else { FAINT }),
            ),
        ]));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = if app.phonemes_pending && app.phonemes.is_empty() {
        "…".to_owned()
    } else {
        app.phonemes.clone()
    };
    let style = Style::default().fg(if edited { CREAM } else { MOSS });
    let line = if focused {
        let cursor = app.phonemes.chars().count().min(text.chars().count());
        Line::from(with_caret(&text, cursor))
    } else {
        Line::from(Span::styled(text, style))
    };
    frame.render_widget(Paragraph::new(line).wrap(Wrap { trim: false }), inner);
}

/// The action row, or the download bar while one runs.
fn draw_actions(frame: &mut Frame, area: Rect, app: &App) {
    let inner = Rect {
        x: area.x + 2,
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    };
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(inner);

    if let Some(fraction) = app.download_fraction() {
        let (id, received, total) = app.download.clone().unwrap_or_default();
        let bar_width = rows[0].width.saturating_sub(30) as usize;
        let mut spans = vec![Span::styled(
            format!("  {id}  "),
            Style::default().fg(CREAM),
        )];
        let cells = dither::bar_levels(fraction, bar_width.max(1), app.frame as usize);
        let head = cells.iter().rposition(|level| *level > 0.0);
        spans.extend(cells.iter().enumerate().map(|(col, level)| {
            let ink = if Some(col) == head { CREAM } else { PETROL };
            Span::styled(dither::cell(*level, 1, col), Style::default().fg(ink))
        }));
        spans.push(Span::styled(
            format!(
                "  {} / {}  {:.0}%",
                catalog::size(received),
                catalog::size(total),
                fraction * 100.0
            ),
            Style::default().fg(MUTED),
        ));
        frame.render_widget(Paragraph::new(Line::from(spans)), rows[0]);
        return;
    }

    let mut spans = vec![Span::styled("  ", Style::default())];
    for (index, (_action, label, enabled)) in app.actions().iter().enumerate() {
        let chosen = app.focus == Focus::Actions && index == app.action;
        let style = if !enabled {
            Style::default().fg(FAINT)
        } else if chosen {
            Style::default().fg(Color::Indexed(16)).bg(CREAM)
        } else {
            Style::default().fg(INK)
        };
        spans.push(Span::styled(format!(" {label} "), style));
        spans.push(Span::styled("  ", Style::default()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), rows[0]);

    let status = match &app.error {
        Some(error) => Span::styled(error.clone(), Style::default().fg(TAN)),
        None => Span::styled(app.status.clone(), Style::default().fg(MUTED)),
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  ", Style::default()),
            status,
        ])),
        rows[1],
    );
}

/// The key hints.
fn draw_hints(frame: &mut Frame, area: Rect, app: &App) {
    let hint = match app.focus {
        Focus::Voices => "tab panes · ↑↓ voices · type to filter · enter select · ctrl-d download",
        Focus::Text => "tab panes · type to edit · ctrl-s save · ctrl-enter say · ctrl-q quit",
        Focus::Phonemes => "tab panes · edit the phonemes · ctrl-r reset · ctrl-enter say",
        Focus::Actions => "tab panes · ←→ choose · enter run · ctrl-q quit",
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            format!("  {hint}"),
            Style::default().fg(FAINT),
        )])),
        area,
    );
}

/// The format chooser.
fn draw_save_prompt(frame: &mut Frame, area: Rect) {
    let width = 44.min(area.width);
    let height = 5.min(area.height);
    let prompt = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let block = Block::bordered()
        .border_style(Style::default().fg(PLUM))
        .title(Span::styled(" save as ", Style::default().fg(MUTED)));
    let inner = block.inner(prompt);
    frame.render_widget(ratatui::widgets::Clear, prompt);
    frame.render_widget(block, prompt);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(" w ", Style::default().fg(Color::Indexed(16)).bg(CREAM)),
                Span::styled("  WAV, uncompressed", Style::default().fg(INK)),
            ]),
            Line::from(vec![
                Span::styled(" f ", Style::default().fg(Color::Indexed(16)).bg(CREAM)),
                Span::styled("  FLAC, lossless", Style::default().fg(INK)),
            ]),
            Line::from(Span::styled(" esc cancels", Style::default().fg(FAINT))),
        ]),
        inner,
    );
}

/// The lines of a buffer, wrapped at `width`, each with the character range it covers.
fn wrapped_lines(buffer: &TextBuffer, width: usize) -> Vec<(String, std::ops::Range<usize>)> {
    let chars: Vec<char> = buffer.text().chars().collect();
    let mut lines = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let end = (start + width).min(chars.len());
        // Break at the last space in the line, so words are not cut in half, unless there
        // is no space to break at.
        let break_at = chars[start..end]
            .iter()
            .rposition(|character| *character == ' ')
            .map(|offset| start + offset + 1)
            .filter(|at| *at > start);
        let stop = if end == chars.len() {
            end
        } else {
            break_at.unwrap_or(end)
        };
        let text: String = chars[start..stop].iter().collect();
        lines.push((text, start..stop));
        if stop == start {
            break;
        }
        start = stop;
    }
    if lines.is_empty() {
        lines.push((String::new(), 0..0));
    }
    lines
}

/// The line with the caret drawn at `column`.
fn with_caret(text: &str, column: usize) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let at = column.min(chars.len());
    let before: String = chars[..at].iter().collect();
    let under: String = chars
        .get(at)
        .map(|c| c.to_string())
        .unwrap_or(" ".to_owned());
    let after: String = chars
        .get(at + 1..)
        .map(|rest| rest.iter().collect())
        .unwrap_or_default();
    vec![
        Span::styled(before, Style::default().fg(INK)),
        Span::styled(under, Style::default().bg(CREAM).fg(Color::Indexed(16))),
        Span::styled(after, Style::default().fg(INK)),
    ]
}

/// What the keyboard did.
pub enum Outcome {
    /// Nothing that needs a redraw.
    Idle,
    /// Something changed.
    Changed,
    /// The app should close.
    Quit,
}

/// Handle one key.
pub fn handle_key(app: &mut App, key: KeyEvent) -> Outcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if app.save_prompt {
        match key.code {
            KeyCode::Char('w') => {
                app.save(Format::Wav);
                return Outcome::Changed;
            }
            KeyCode::Char('f') => {
                app.save(Format::Flac);
                return Outcome::Changed;
            }
            KeyCode::Esc => {
                app.save_prompt = false;
                return Outcome::Changed;
            }
            _ => return Outcome::Idle,
        }
    }

    if ctrl {
        match key.code {
            KeyCode::Char('q') | KeyCode::Char('c') => return Outcome::Quit,
            KeyCode::Char('s') => {
                app.save_prompt = true;
                return Outcome::Changed;
            }
            KeyCode::Char('r') => {
                app.reset_phonemes();
                return Outcome::Changed;
            }
            KeyCode::Char('d') => {
                if let Some(voice) = app.current_voice() {
                    let id = voice.id.clone();
                    app.start_download(&id);
                }
                return Outcome::Changed;
            }
            KeyCode::Enter => {
                app.say();
                return Outcome::Changed;
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Tab => {
            app.focus = app.focus.next();
            return Outcome::Changed;
        }
        KeyCode::BackTab => {
            // Backwards through the same order.
            for _ in 0..3 {
                app.focus = app.focus.next();
            }
            return Outcome::Changed;
        }
        KeyCode::Esc => return Outcome::Quit,
        _ => {}
    }

    match app.focus {
        Focus::Voices => handle_voices(app, key),
        Focus::Text => handle_text(app, key),
        Focus::Phonemes => handle_phonemes(app, key),
        Focus::Actions => handle_actions(app, key),
    }
}

fn handle_voices(app: &mut App, key: KeyEvent) -> Outcome {
    match key.code {
        // The tabs are the first row of the pane, so left and right move between them and
        // up and down move down the list.
        KeyCode::Left | KeyCode::Right => {
            let at = catalog::ENGINES
                .iter()
                .position(|engine| *engine == app.engine)
                .unwrap_or(0);
            let next = if matches!(key.code, KeyCode::Left) {
                (at + catalog::ENGINES.len() - 1) % catalog::ENGINES.len()
            } else {
                (at + 1) % catalog::ENGINES.len()
            };
            let engine = catalog::ENGINES[next].to_owned();
            app.set_engine(&engine);
            Outcome::Changed
        }
        KeyCode::Up => {
            app.selected = app.selected.saturating_sub(1);
            Outcome::Changed
        }
        KeyCode::Down => {
            if app.selected + 1 < app.visible.len() {
                app.selected += 1;
            }
            Outcome::Changed
        }
        KeyCode::Enter => {
            app.select_voice();
            Outcome::Changed
        }
        KeyCode::Char('+') | KeyCode::Char('=') => {
            app.next_speaker();
            Outcome::Changed
        }
        KeyCode::Char('-') | KeyCode::Char('_') => {
            app.previous_speaker();
            Outcome::Changed
        }
        KeyCode::Backspace => {
            app.filter.pop();
            app.visible = catalog::matching(&app.voices, &app.filter);
            app.selected = 0;
            Outcome::Changed
        }
        KeyCode::Char(character) => {
            app.filter.push(character);
            app.visible = catalog::matching(&app.voices, &app.filter);
            app.selected = 0;
            Outcome::Changed
        }
        _ => Outcome::Idle,
    }
}

fn handle_text(app: &mut App, key: KeyEvent) -> Outcome {
    match key.code {
        KeyCode::Char(character) => {
            app.text.insert(character);
            app.text_changed();
            Outcome::Changed
        }
        KeyCode::Enter => {
            app.text.insert('\n');
            app.text_changed();
            Outcome::Changed
        }
        KeyCode::Backspace => {
            app.text.backspace();
            app.text_changed();
            Outcome::Changed
        }
        KeyCode::Delete => {
            app.text.delete();
            app.text_changed();
            Outcome::Changed
        }
        KeyCode::Left => {
            app.text.left();
            Outcome::Changed
        }
        KeyCode::Right => {
            app.text.right();
            Outcome::Changed
        }
        KeyCode::Home => {
            app.text.home();
            Outcome::Changed
        }
        KeyCode::End => {
            app.text.end();
            Outcome::Changed
        }
        _ => Outcome::Idle,
    }
}

fn handle_phonemes(app: &mut App, key: KeyEvent) -> Outcome {
    match key.code {
        KeyCode::Char(character) => {
            app.phonemes.push(character);
            app.phonemes_edited = true;
            Outcome::Changed
        }
        KeyCode::Backspace => {
            app.phonemes.pop();
            app.phonemes_edited = true;
            Outcome::Changed
        }
        _ => Outcome::Idle,
    }
}

fn handle_actions(app: &mut App, key: KeyEvent) -> Outcome {
    let count = app.actions().len();
    match key.code {
        KeyCode::Left => {
            app.action = app.action.saturating_sub(1);
            Outcome::Changed
        }
        KeyCode::Right => {
            app.action = (app.action + 1).min(count.saturating_sub(1));
            Outcome::Changed
        }
        KeyCode::Enter => {
            match app.actions().get(app.action).map(|(action, _, _)| *action) {
                Some(Action::Say) => app.say(),
                Some(Action::Save) => app.save_prompt = true,
                Some(Action::Reset) => app.reset_phonemes(),
                None => {}
            }
            Outcome::Changed
        }
        _ => Outcome::Idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_breaks_at_spaces() {
        let buffer = TextBuffer::new("hello world again");
        let lines = wrapped_lines(&buffer, 12);
        assert_eq!(lines[0].0, "hello world ");
        assert_eq!(lines[1].0, "again");
    }

    #[test]
    fn a_word_longer_than_the_line_is_cut() {
        let buffer = TextBuffer::new("abcdefghij");
        let lines = wrapped_lines(&buffer, 4);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].0, "abcd");
    }

    #[test]
    fn the_caret_is_a_character_wide() {
        let spans = with_caret("abc", 1);
        assert_eq!(spans[0].content, "a");
        assert_eq!(spans[1].content, "b");
        assert_eq!(spans[2].content, "c");
    }
}
