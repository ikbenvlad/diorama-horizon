//! The waypoint list and editor on the fullscreen map, apart from their drawing: the editor's
//! fields as the player types into them, the order of the list, and where every box and button
//! goes, so drawing and clicking agree. Rectangles are `[x, y, width, height]` in GUI pixels.

use crate::share::MAX_NAME;
use crate::waypoint::{MAX_INITIALS, PALETTE, Waypoint};

pub type Rect = [f32; 4];

pub fn inside(r: Rect, p: [f32; 2]) -> bool {
    p[0] >= r[0] && p[0] < r[0] + r[2] && p[1] >= r[1] && p[1] < r[1] + r[3]
}

/// The editor's text fields.
pub const FIELDS: [&str; 5] = ["Name", "Letters", "X", "Y", "Z"];
const NAME: usize = 0;
const LETTERS: usize = 1;
/// Digits a coordinate takes, its minus sign apart.
const COORD_DIGITS: usize = 8;

/// A waypoint being edited: what is typed, before it is saved.
#[derive(Clone, Debug, PartialEq)]
pub struct Form {
    /// The waypoint's index in the list.
    pub index: usize,
    pub fields: [String; 5],
    /// The field typed into.
    pub focus: usize,
    pub color: u32,
    pub visible: bool,
    /// Why the last save was refused.
    pub error: Option<&'static str>,
}

impl Form {
    pub fn of(index: usize, w: &Waypoint) -> Form {
        Form {
            index,
            fields: [
                w.name.clone(),
                w.initials.clone(),
                w.x.to_string(),
                w.y.to_string(),
                w.z.to_string(),
            ],
            focus: NAME,
            color: w.color,
            visible: w.visible,
            error: None,
        }
    }

    /// Types `c` into the focused field, if that field takes it.
    pub fn type_char(&mut self, c: char) -> bool {
        let field = &mut self.fields[self.focus];
        let len = field.chars().count();
        let takes = match self.focus {
            NAME => !c.is_control() && c != '§' && len < MAX_NAME,
            LETTERS => c.is_alphanumeric() && len < MAX_INITIALS,
            _ => {
                let digits = field.chars().filter(char::is_ascii_digit).count();
                c.is_ascii_digit() && digits < COORD_DIGITS || c == '-' && field.is_empty()
            }
        };
        if takes {
            field.push(c);
            self.error = None;
        }
        takes
    }

    pub fn backspace(&mut self) {
        self.fields[self.focus].pop();
        self.error = None;
    }

    /// The next field, or the one before.
    pub fn tab(&mut self, back: bool) {
        let n = FIELDS.len();
        self.focus = (self.focus + if back { n - 1 } else { 1 }) % n;
    }

    /// Writes the fields into `w`; refused, with the reason, when a field doesn't read.
    pub fn apply(&self, w: &mut Waypoint) -> Result<(), &'static str> {
        let name = self.fields[NAME].trim();
        if name.is_empty() {
            return Err("The name is empty");
        }
        let coord = |i: usize| self.fields[i].trim().parse::<i32>();
        let (Ok(x), Ok(y), Ok(z)) = (coord(2), coord(3), coord(4)) else {
            return Err("X, Y and Z must be whole numbers");
        };
        if y.abs() > 20_000 || x.abs() > 30_000_000 || z.abs() > 30_000_000 {
            return Err("That's outside the world");
        }
        w.name = name.to_owned();
        w.initials = self.fields[LETTERS].trim().to_owned();
        (w.x, w.y, w.z) = (x, y, z);
        w.color = self.color;
        w.visible = self.visible;
        Ok(())
    }
}

/// The editor's buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Visible,
    Share,
    Teleport,
    Delete,
    Save,
    Cancel,
}

impl Button {
    pub fn label(self, visible: bool) -> &'static str {
        match self {
            Button::Visible if visible => "Shown",
            Button::Visible => "Hidden",
            Button::Share => "Share in chat",
            Button::Teleport => "Teleport",
            Button::Delete => "Delete",
            Button::Save => "Save",
            Button::Cancel => "Cancel",
        }
    }
}

/// What a click in the editor landed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorHit {
    Field(usize),
    Swatch(usize),
    Button(Button),
}

pub const BUTTON_H: f32 = 14.0;
const FIELD_H: f32 = 13.0;
const EDITOR_W: f32 = 240.0;
const EDITOR_H: f32 = 182.0;

pub struct EditorLayout {
    pub panel: Rect,
    /// The title's place.
    pub title: [f32; 2],
    /// Each field's label place and box.
    pub labels: [[f32; 2]; 5],
    pub fields: [Rect; 5],
    pub swatches: [Rect; 16],
    pub buttons: [(Button, Rect); 6],
    /// Where the reason a save was refused goes.
    pub error: [f32; 2],
}

/// The editor, in the middle of a GUI `size` across.
pub fn editor_layout(size: [f32; 2]) -> EditorLayout {
    let (x, y) = (
        ((size[0] - EDITOR_W) / 2.0).floor(),
        ((size[1] - EDITOR_H) / 2.0).floor().max(0.0),
    );
    let field_x = x + 52.0;
    let row = |i: f32| y + 22.0 + i * 18.0;
    let coord_w = 52.0;
    let coord = |i: f32| {
        [
            field_x + 14.0 + i * (coord_w + 16.0),
            row(2.0),
            coord_w,
            FIELD_H,
        ]
    };
    let fields = [
        [field_x, row(0.0), EDITOR_W - 60.0, FIELD_H],
        [field_x, row(1.0), 50.0, FIELD_H],
        coord(0.0),
        coord(1.0),
        coord(2.0),
    ];
    let labels = [
        [x + 8.0, row(0.0) + 3.0],
        [x + 8.0, row(1.0) + 3.0],
        [coord(0.0)[0] - 10.0, row(2.0) + 3.0],
        [coord(1.0)[0] - 10.0, row(2.0) + 3.0],
        [coord(2.0)[0] - 10.0, row(2.0) + 3.0],
    ];
    let swatch_y = row(3.0) + 4.0;
    let swatches = std::array::from_fn(|i| [x + 8.0 + i as f32 * 14.0, swatch_y, 12.0, 12.0]);
    let half = (EDITOR_W - 24.0) / 2.0;
    let button = |col: f32, line: f32| {
        [
            x + 8.0 + col * (half + 8.0),
            swatch_y + 22.0 + line * (BUTTON_H + 6.0),
            half,
            BUTTON_H,
        ]
    };
    EditorLayout {
        panel: [x, y, EDITOR_W, EDITOR_H],
        title: [x + 8.0, y + 7.0],
        labels,
        fields,
        swatches,
        buttons: [
            (Button::Visible, button(0.0, 0.0)),
            (Button::Share, button(1.0, 0.0)),
            (Button::Teleport, button(0.0, 1.0)),
            (Button::Delete, button(1.0, 1.0)),
            (Button::Save, button(0.0, 2.0)),
            (Button::Cancel, button(1.0, 2.0)),
        ],
        error: [x + 8.0, swatch_y + 22.0 + 3.0 * (BUTTON_H + 6.0)],
    }
}

impl EditorLayout {
    pub fn hit(&self, p: [f32; 2]) -> Option<EditorHit> {
        if let Some(i) = self.fields.iter().position(|r| inside(*r, p)) {
            return Some(EditorHit::Field(i));
        }
        if let Some(i) = self.swatches.iter().position(|r| inside(*r, p)) {
            return Some(EditorHit::Swatch(i));
        }
        self.buttons
            .iter()
            .find(|(_, r)| inside(*r, p))
            .map(|(b, _)| EditorHit::Button(*b))
    }
}

/// The swatch colours, the palette's.
pub fn swatch(i: usize) -> u32 {
    PALETTE[i]
}

/// The list's order: the waypoints in `dimension` nearest `player` first, then the other
/// dimensions' by dimension and name.
pub fn list_order(all: &[Waypoint], dimension: &str, player: [f64; 3]) -> Vec<usize> {
    let mut here: Vec<(f64, usize)> = Vec::new();
    let mut away: Vec<(&str, &str, usize)> = Vec::new();
    for (i, w) in all.iter().enumerate() {
        if w.dimension == dimension {
            here.push((w.distance_to(player), i));
        } else {
            away.push((&w.dimension, &w.name, i));
        }
    }
    here.sort_by(|a, b| a.0.total_cmp(&b.0));
    away.sort();
    here.into_iter()
        .map(|(_, i)| i)
        .chain(away.into_iter().map(|(_, _, i)| i))
        .collect()
}

/// What a click in the list landed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListHit {
    Open(usize),
    Toggle(usize),
    Add,
    Done,
}

pub const ROW_H: f32 = 14.0;
const LIST_W: f32 = 300.0;
const TOGGLE_W: f32 = 40.0;

pub struct ListLayout {
    pub panel: Rect,
    pub title: [f32; 2],
    /// Each row shown: the waypoint's index, the row and its show/hide toggle.
    pub rows: Vec<(usize, Rect, Rect)>,
    /// The first row shown, after clamping.
    pub scroll: usize,
    /// The most `scroll` can be.
    pub scroll_max: usize,
    pub add: Rect,
    pub done: Rect,
}

/// The list of `order` (waypoint indices) from row `scroll`, in the middle of a GUI `size`.
pub fn list_layout(size: [f32; 2], order: &[usize], scroll: usize) -> ListLayout {
    let fits = (((size[1] - 80.0) / ROW_H).floor() as usize).max(3);
    let shown = order.len().clamp(1, fits);
    let height = 22.0 + shown as f32 * ROW_H + 8.0 + BUTTON_H + 8.0;
    let (x, y) = (
        ((size[0] - LIST_W) / 2.0).floor(),
        ((size[1] - height) / 2.0).floor().max(0.0),
    );
    let scroll_max = order.len().saturating_sub(fits);
    let scroll = scroll.min(scroll_max);
    let rows = order
        .iter()
        .skip(scroll)
        .take(fits)
        .enumerate()
        .map(|(n, &i)| {
            let ry = y + 22.0 + n as f32 * ROW_H;
            (
                i,
                [x + 6.0, ry, LIST_W - 12.0 - TOGGLE_W - 4.0, ROW_H - 1.0],
                [x + LIST_W - 6.0 - TOGGLE_W, ry, TOGGLE_W, ROW_H - 1.0],
            )
        })
        .collect();
    let bottom = y + height - 8.0 - BUTTON_H;
    ListLayout {
        panel: [x, y, LIST_W, height],
        title: [x + 8.0, y + 7.0],
        rows,
        scroll,
        scroll_max,
        add: [x + 6.0, bottom, 110.0, BUTTON_H],
        done: [x + LIST_W - 6.0 - 80.0, bottom, 80.0, BUTTON_H],
    }
}

impl ListLayout {
    pub fn hit(&self, p: [f32; 2]) -> Option<ListHit> {
        if inside(self.add, p) {
            return Some(ListHit::Add);
        }
        if inside(self.done, p) {
            return Some(ListHit::Done);
        }
        self.rows.iter().find_map(|(i, row, toggle)| {
            if inside(*toggle, p) {
                Some(ListHit::Toggle(*i))
            } else {
                inside(*row, p).then_some(ListHit::Open(*i))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wp(name: &str, at: [i32; 3], dim: &str) -> Waypoint {
        Waypoint::new(name, at, dim, PALETTE[0])
    }

    #[test]
    fn fields_take_what_they_should() {
        let mut f = Form::of(0, &wp("Home", [1, 64, -2], "d"));
        assert!(f.type_char('!'));
        f.tab(false);
        assert!(!f.type_char(' '));
        for c in "ABCDE".chars() {
            f.type_char(c);
        }
        assert_eq!(f.fields[1], "ABCD");
        f.tab(false);
        f.fields[2].clear();
        assert!(f.type_char('-'));
        assert!(!f.type_char('-'));
        assert!(!f.type_char('x'));
        f.type_char('5');
        f.tab(true);
        f.tab(true);
        assert_eq!(f.focus, 0);
        let mut w = wp("Home", [1, 64, -2], "d");
        f.apply(&mut w).unwrap();
        assert_eq!(
            (w.name.as_str(), w.initials.as_str(), w.x),
            ("Home!", "ABCD", -5)
        );
        f.fields[3] = "-".into();
        assert!(f.apply(&mut w).is_err());
        f.fields[3] = "70".into();
        f.fields[0] = "  ".into();
        assert_eq!(f.apply(&mut w), Err("The name is empty"));
    }

    #[test]
    fn the_list_puts_near_ones_first() {
        let all = vec![
            wp("far", [100, 0, 0], "a"),
            wp("other", [0, 0, 0], "b"),
            wp("near", [1, 0, 0], "a"),
        ];
        assert_eq!(list_order(&all, "a", [0.0, 0.0, 0.0]), vec![2, 0, 1]);
    }

    #[test]
    fn layouts_hit_what_they_draw() {
        let e = editor_layout([427.0, 240.0]);
        for (i, r) in e.fields.iter().enumerate() {
            assert_eq!(e.hit([r[0] + 1.0, r[1] + 1.0]), Some(EditorHit::Field(i)));
        }
        for (b, r) in e.buttons {
            assert_eq!(
                e.hit([r[0] + r[2] - 1.0, r[1] + r[3] - 1.0]),
                Some(EditorHit::Button(b))
            );
            assert!(
                r[1] + r[3] <= e.panel[1] + e.panel[3],
                "{b:?} inside the panel"
            );
        }
        assert_eq!(
            e.hit([e.swatches[15][0] + 1.0, e.swatches[15][1] + 1.0]),
            Some(EditorHit::Swatch(15))
        );
        assert!(e.swatches[15][0] + 12.0 <= e.panel[0] + e.panel[2]);

        let order: Vec<usize> = (0..40).collect();
        let l = list_layout([427.0, 240.0], &order, 100);
        assert_eq!(l.scroll, l.scroll_max);
        assert!(l.panel[1] + l.panel[3] <= 240.0);
        let (first, row, toggle) = l.rows[0];
        assert_eq!(first, l.scroll);
        assert_eq!(
            l.hit([row[0] + 1.0, row[1] + 1.0]),
            Some(ListHit::Open(first))
        );
        assert_eq!(
            l.hit([toggle[0] + 1.0, toggle[1] + 1.0]),
            Some(ListHit::Toggle(first))
        );
        assert_eq!(l.hit([l.add[0] + 1.0, l.add[1] + 1.0]), Some(ListHit::Add));
        assert_eq!(list_layout([427.0, 240.0], &[], 0).rows.len(), 0);
    }
}
