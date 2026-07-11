use nvim_rs::Value;
use ratatui::layout::Position;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;
use std::collections::HashMap;

pub(super) fn draw_grid(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    grid: &Grid,
) {
    let _ = terminal.draw(|frame| {
        let area = frame.area();
        let w = area.width as usize;
        let mut lines: Vec<Line> = Vec::with_capacity(area.height as usize);
        for row in grid.cells.iter().take(area.height as usize) {
            lines.push(render_row(grid, row, w));
        }
        frame.render_widget(Paragraph::new(Text::from(lines)), area);
        let cx = (grid.cursor.0 as u16).min(area.width.saturating_sub(1));
        let cy = (grid.cursor.1 as u16).min(area.height.saturating_sub(1));
        frame.set_cursor_position(Position::new(area.x + cx, area.y + cy));
    });
}

fn render_row(grid: &Grid, row: &[GCell], width: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur: Option<Style> = None;
    for cell in row.iter().take(width) {
        let style = resolve_style(grid, cell.hl);
        if cur != Some(style) {
            if !buf.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut buf),
                    cur.unwrap_or_default(),
                ));
            }
            cur = Some(style);
        }
        buf.push(cell.ch);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, cur.unwrap_or_default()));
    }
    Line::from(spans)
}

fn resolve_style(grid: &Grid, hl: u16) -> Style {
    let a = grid.hl.get(&hl).copied().unwrap_or_default();
    let mut s = Style::default();
    if let Some(fg) = a.fg.or(grid.def_fg) {
        s = s.fg(rgb(fg));
    }
    if let Some(bg) = a.bg {
        if Some(bg) != grid.def_bg {
            s = s.bg(rgb(bg));
        }
    }
    if a.bold {
        s = s.add_modifier(Modifier::BOLD);
    }
    if a.italic {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if a.underline {
        s = s.add_modifier(Modifier::UNDERLINED);
    }
    if a.reverse {
        s = s.add_modifier(Modifier::REVERSED);
    }
    s
}

fn rgb(v: u32) -> Color {
    Color::Rgb(
        ((v >> 16) & 0xff) as u8,
        ((v >> 8) & 0xff) as u8,
        (v & 0xff) as u8,
    )
}

#[derive(Clone, Copy)]
pub(super) struct GCell {
    pub(super) ch: char,
    pub(super) hl: u16,
}

impl Default for GCell {
    fn default() -> Self {
        GCell { ch: ' ', hl: 0 }
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
pub(super) struct Attr {
    pub(super) fg: Option<u32>,
    pub(super) bg: Option<u32>,
    pub(super) bold: bool,
    pub(super) italic: bool,
    pub(super) underline: bool,
    pub(super) reverse: bool,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Shape {
    Block,
    Bar,
    Underline,
}

pub(super) struct Grid {
    pub(super) w: usize,
    pub(super) h: usize,
    pub(super) cells: Vec<Vec<GCell>>,
    pub(super) cursor: (usize, usize),
    pub(super) hl: HashMap<u16, Attr>,
    pub(super) def_fg: Option<u32>,
    pub(super) def_bg: Option<u32>,
    pub(super) shape: Shape,
}

impl Grid {
    pub(super) fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            cells: vec![vec![GCell::default(); w]; h],
            cursor: (0, 0),
            hl: HashMap::new(),
            def_fg: None,
            def_bg: None,
            shape: Shape::Block,
        }
    }
    pub(super) fn resize(&mut self, w: usize, h: usize) {
        self.w = w;
        self.h = h;
        self.cells = vec![vec![GCell::default(); w]; h];
        self.cursor = (0, 0);
    }
    fn clear(&mut self) {
        for row in &mut self.cells {
            for c in row.iter_mut() {
                *c = GCell::default();
            }
        }
    }
}

pub(super) fn apply_redraw(grid: &mut Grid, batch: &[Value]) {
    for group in batch {
        let Some(items) = group.as_array() else {
            continue;
        };
        let Some(name) = items.first().and_then(|v| v.as_str()) else {
            continue;
        };
        for params in &items[1..] {
            let Some(p) = params.as_array() else { continue };
            match name {
                "grid_resize" => {
                    if let (Some(w), Some(h)) = (uget(p, 1), uget(p, 2)) {
                        grid.resize(w as usize, h as usize);
                    }
                }
                "grid_clear" => grid.clear(),
                "grid_cursor_goto" => {
                    if let (Some(r), Some(c)) = (uget(p, 1), uget(p, 2)) {
                        grid.cursor = (c as usize, r as usize);
                    }
                }
                "grid_line" => apply_grid_line(grid, p),
                "grid_scroll" => apply_grid_scroll(grid, p),
                "default_colors_set" => {
                    grid.def_fg = uget(p, 0).map(|v| v as u32);
                    grid.def_bg = uget(p, 1).map(|v| v as u32);
                }
                "hl_attr_define" => {
                    if let Some((id, attr)) = parse_hl(p) {
                        grid.hl.insert(id, attr);
                    }
                }
                "mode_change" => {
                    if let Some(mode) = p.first().and_then(|v| v.as_str()) {
                        grid.shape = if mode.contains("insert") {
                            Shape::Bar
                        } else if mode.contains("replace") {
                            Shape::Underline
                        } else {
                            Shape::Block
                        };
                    }
                }
                _ => {}
            }
        }
    }
}

fn parse_hl(p: &[Value]) -> Option<(u16, Attr)> {
    let id = uget(p, 0)? as u16;
    let m = p.get(1)?;
    let b = |key: &str| map_get(m, key).and_then(|v| v.as_bool()).unwrap_or(false);
    let attr = Attr {
        fg: map_get(m, "foreground")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        bg: map_get(m, "background")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        bold: b("bold"),
        italic: b("italic"),
        underline: b("underline") || b("undercurl") || b("underdouble"),
        reverse: b("reverse"),
    };
    Some((id, attr))
}

pub(super) fn map_get<'a>(m: &'a Value, key: &str) -> Option<&'a Value> {
    if let Value::Map(entries) = m {
        for (k, v) in entries {
            if k.as_str() == Some(key) {
                return Some(v);
            }
        }
    }
    None
}

fn apply_grid_line(grid: &mut Grid, p: &[Value]) {
    let (Some(row), Some(col_start)) = (uget(p, 1), uget(p, 2)) else {
        return;
    };
    let (row, mut col) = (row as usize, col_start as usize);
    if row >= grid.h {
        return;
    }
    let Some(cells) = p.get(3).and_then(|v| v.as_array()) else {
        return;
    };
    let mut last_hl: u16 = 0;
    for cell in cells {
        let Some(c) = cell.as_array() else { continue };
        let text = c.first().and_then(|v| v.as_str()).unwrap_or(" ");
        if let Some(h) = c.get(1).and_then(|v| v.as_u64()) {
            last_hl = h as u16;
        }
        let remaining = grid.w.saturating_sub(col) as u64;
        let repeat = c
            .get(2)
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1)
            .min(remaining);
        let ch = text.chars().next().unwrap_or(' ');
        for _ in 0..repeat {
            grid.cells[row][col] = GCell { ch, hl: last_hl };
            col += 1;
        }
    }
}

fn apply_grid_scroll(grid: &mut Grid, p: &[Value]) {
    let (Some(top), Some(bot), Some(left), Some(right)) =
        (uget(p, 1), uget(p, 2), uget(p, 3), uget(p, 4))
    else {
        return;
    };
    let rows = p.get(5).and_then(|v| v.as_i64()).unwrap_or(0);
    let (top, bot, left, right) = (
        (top as usize).min(grid.h),
        (bot as usize).min(grid.h),
        (left as usize).min(grid.w),
        (right as usize).min(grid.w),
    );
    if rows > 0 {
        let r = rows.unsigned_abs() as usize;
        for dst in top..bot.saturating_sub(r) {
            if dst + r >= grid.h {
                break;
            }
            for col in left..right {
                grid.cells[dst][col] = grid.cells[dst + r][col];
            }
        }
        for dst in bot.saturating_sub(r)..bot {
            for col in left..right {
                grid.cells[dst][col] = GCell::default();
            }
        }
    } else if rows < 0 {
        let r = rows.unsigned_abs() as usize;
        for dst in (top + r..bot).rev() {
            if dst < r {
                continue;
            }
            for col in left..right {
                grid.cells[dst][col] = grid.cells[dst - r][col];
            }
        }
        for dst in top..(top + r).min(grid.h) {
            for col in left..right {
                grid.cells[dst][col] = GCell::default();
            }
        }
    }
}

fn uget(p: &[Value], i: usize) -> Option<u64> {
    p.get(i).and_then(|v| v.as_u64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_line_writes_text() {
        let mut g = Grid::new(10, 3);
        let p = vec![
            Value::from(1),
            Value::from(0u64),
            Value::from(0u64),
            Value::Array(vec![
                Value::Array(vec![Value::from("H")]),
                Value::Array(vec![Value::from("i")]),
            ]),
        ];
        apply_grid_line(&mut g, &p);
        assert_eq!(g.cells[0][0].ch, 'H');
        assert_eq!(g.cells[0][1].ch, 'i');
    }

    #[test]
    fn hl_attr_and_default_colors_parse() {
        let mut g = Grid::new(4, 1);
        apply_redraw(
            &mut g,
            &[Value::Array(vec![
                Value::from("default_colors_set"),
                Value::Array(vec![
                    Value::from(0xeeeeeeu64),
                    Value::from(0x111111u64),
                    Value::from(0),
                ]),
            ])],
        );
        assert_eq!(g.def_fg, Some(0xeeeeee));
        assert_eq!(g.def_bg, Some(0x111111));
        let attrmap = Value::Map(vec![
            (Value::from("foreground"), Value::from(0xff0000u64)),
            (Value::from("bold"), Value::from(true)),
        ]);
        apply_redraw(
            &mut g,
            &[Value::Array(vec![
                Value::from("hl_attr_define"),
                Value::Array(vec![
                    Value::from(7u64),
                    attrmap,
                    Value::Map(vec![]),
                    Value::Array(vec![]),
                ]),
            ])],
        );
        let a = g.hl.get(&7).copied().unwrap();
        assert_eq!(a.fg, Some(0xff0000));
        assert!(a.bold);
    }

    #[test]
    fn mode_change_sets_cursor_shape() {
        let mut g = Grid::new(4, 1);
        apply_redraw(
            &mut g,
            &[Value::Array(vec![
                Value::from("mode_change"),
                Value::Array(vec![Value::from("insert"), Value::from(1)]),
            ])],
        );
        assert!(matches!(g.shape, Shape::Bar));
        apply_redraw(
            &mut g,
            &[Value::Array(vec![
                Value::from("mode_change"),
                Value::Array(vec![Value::from("normal"), Value::from(0)]),
            ])],
        );
        assert!(matches!(g.shape, Shape::Block));
    }

    #[test]
    fn default_background_is_transparent() {
        let mut g = Grid::new(2, 1);
        g.def_bg = Some(0x112233);
        assert!(
            resolve_style(&g, 0).bg.is_none(),
            "default bg must be transparent"
        );
        g.hl.insert(
            1,
            Attr {
                bg: Some(0x112233),
                ..Default::default()
            },
        );
        assert!(
            resolve_style(&g, 1).bg.is_none(),
            "a bg equal to the default must stay transparent"
        );
        g.hl.insert(
            2,
            Attr {
                bg: Some(0xff0000),
                ..Default::default()
            },
        );
        assert!(
            resolve_style(&g, 2).bg.is_some(),
            "a distinct highlight bg must be painted"
        );
    }
}
