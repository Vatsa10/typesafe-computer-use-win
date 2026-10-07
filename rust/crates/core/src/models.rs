//! The data carried between perception, decision and action.
//!
//! A port of `typesafe_computer_use_win/models.py`. The coordinate rules here are the ones that
//! cost the most to learn: a capture is one display, items live in that display's pixels, and the
//! display's origin on the virtual desktop has to be subtracted going in and added coming out.

use serde::Serialize;

/// x1, y1, x2, y2 in capture pixels.
pub type PixelBox = (f64, f64, f64, f64);

pub const TEXT_ROLES: [&str; 4] = ["AXTextField", "AXTextArea", "AXSearchField", "AXComboBox"];

/// A control's role as one human word, which is how the classifier is told what it is.
pub fn role_word(role: &str) -> &'static str {
    match role {
        "AXButton" | "AXMenuButton" => "button",
        "AXCell" | "AXRow" => "cell",
        "AXCheckBox" => "checkbox",
        "AXComboBox" | "AXSearchField" | "AXTextArea" | "AXTextField" => "field",
        "AXDockItem" => "taskbar item",
        "AXImage" => "image",
        "AXLink" => "link",
        "AXMenuBarItem" => "menu",
        "AXPopUpButton" => "popup",
        "AXRadioButton" => "radio",
        "AXSlider" => "slider",
        "AXTab" => "tab",
        _ => "other",
    }
}

/// One thing that can be clicked: text plus its box on the capture.
///
/// `source` is "ocr" for a text block, "ax" for a control the app declared, "ax+ocr" when both
/// found the same thing. `role` is a human word and is empty for text-only items.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Item {
    pub index: usize,
    pub text: String,
    pub ocr_confidence: f64,
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
    pub role: String,
    pub source: String,
}

impl Item {
    pub fn center(&self) -> (f64, f64) {
        ((self.x1 + self.x2) / 2.0, (self.y1 + self.y2) / 2.0)
    }

    pub fn from_ax(&self) -> bool {
        self.source == "ax" || self.source == "ax+ocr"
    }
}

/// The focused element, in virtual-desktop points.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Field {
    pub role: String,
    pub label: String,
    pub placeholder: String,
    pub value: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Field {
    pub fn is_text(&self) -> bool {
        TEXT_ROLES.contains(&self.role.as_str())
    }

    pub fn summary(&self) -> serde_json::Value {
        let mut value = self.value.clone();
        value.truncate(200);
        serde_json::json!({
            "role": self.role,
            "label": self.label,
            "placeholder": self.placeholder,
            "current_value": value,
        })
    }
}

/// Raised when the user triggers an escape hatch: the corner, the abort hotkey, a spoken "stop".
#[derive(Debug, Clone, PartialEq)]
pub struct Abort(pub String);

impl std::fmt::Display for Abort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for Abort {}

/// Where the mouse is, and what it is over. People point at what they are asking about, so "this",
/// "here" and "that" mean whatever is under it.
#[derive(Debug, Clone, PartialEq)]
pub struct PointerInfo {
    /// Virtual-desktop physical pixels, like every window rectangle. May be negative.
    pub x: f64,
    pub y: f64,
    /// The display the mouse is on.
    pub monitor: usize,
    /// Seconds since the last mouse or keyboard input, when Windows would say.
    pub idle_seconds: Option<f64>,
    /// The control under the mouse, never one of Pointer's own windows.
    pub under: Option<platform::uia::Under>,
    /// The top window (app, title) on the mouse's display, filled only when that display is not
    /// the one being read and the mouse moved recently: the user is looking there.
    pub display_window: Option<(String, String)>,
}

/// Everything captured about the machine at one instant.
///
/// Not `Send`: accessibility handles are COM pointers bound to the thread that fetched them, so a
/// screen and the step that acts on it belong to one thread. That is the design, not an accident.
pub struct Screen {
    /// The capture, BGRA, top-down rows.
    pub bgra: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Capture pixels per point. 1.0 on a DPI-aware process, which this one always is.
    pub scale: f64,
    pub app: String,
    pub field: Option<Field>,
    pub url: Option<String>,
    pub pid: Option<u32>,
    /// The foreground window as x, y, w, h in virtual-desktop points.
    pub window: Option<(f64, f64, f64, f64)>,
    /// Where the captured display sits on the virtual desktop. On this machine one monitor starts
    /// at y=-1440 and another at x=2561, so this is routinely non-zero and sometimes negative.
    pub origin: (f64, f64),
    /// Which display this capture came from.
    pub monitor: usize,
    pub windows: Vec<platform::winlist::WindowInfo>,
    pub monitors: Vec<platform::display::Monitor>,
    /// Item index to the control behind it, when the item came from the accessibility tree.
    pub ax_refs: std::collections::HashMap<usize, platform::uia::PressHandle>,
    /// Labelled controls the app exposes but does not show: reachable by a press, never by a pixel.
    pub offscreen: Vec<platform::uia::Node>,
    /// The mouse, when it could be found.
    pub pointer: Option<PointerInfo>,
}

impl Screen {
    pub fn size_pt(&self) -> (f64, f64) {
        (
            self.width as f64 / self.scale,
            self.height as f64 / self.scale,
        )
    }

    /// Which ninth of the screen an item sits in, in words.
    pub fn region(&self, item: &Item) -> String {
        let (cx, cy) = item.center();
        let col = ["left", "center", "right"][((3.0 * cx / self.width as f64) as usize).min(2)];
        let row = ["top", "middle", "bottom"][((3.0 * cy / self.height as f64) as usize).min(2)];
        format!("{row}-{col}")
    }

    /// Where to click, in the virtual-desktop coordinates synthetic input speaks.
    ///
    /// The item's pixels are relative to the captured display; adding the origin is what keeps a
    /// click meant for the third monitor from landing on the first. It is added exactly once —
    /// the item had it subtracted exactly once on the way in.
    pub fn to_points(&self, item: &Item) -> (f64, f64) {
        let (cx, cy) = item.center();
        (
            self.origin.0 + cx / self.scale,
            self.origin.1 + cy / self.scale,
        )
    }

    /// The mouse in this capture's pixels: the origin subtracted exactly once, as for items.
    pub fn pointer_px(&self) -> Option<(f64, f64)> {
        let p = self.pointer.as_ref()?;
        Some((
            (p.x - self.origin.0) * self.scale,
            (p.y - self.origin.1) * self.scale,
        ))
    }

    /// The item the mouse is over: the smallest one whose box holds the mouse, so a button inside
    /// a toolbar wins over the toolbar. None when the mouse is on another display.
    pub fn under_mouse(&self, items: &[Item]) -> Option<usize> {
        let (px, py) = self.pointer_px()?;
        items
            .iter()
            .filter(|it| it.x1 <= px && px <= it.x2 && it.y1 <= py && py <= it.y2)
            .min_by(|a, b| {
                let area = |it: &&Item| (it.x2 - it.x1) * (it.y2 - it.y1);
                area(a).total_cmp(&area(b))
            })
            .map(|it| it.index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(x1: f64, y1: f64, x2: f64, y2: f64) -> Item {
        Item {
            index: 0,
            text: "x".into(),
            ocr_confidence: 1.0,
            x1,
            y1,
            x2,
            y2,
            role: String::new(),
            source: "ocr".into(),
        }
    }

    fn screen(origin: (f64, f64)) -> Screen {
        Screen {
            bgra: Vec::new(),
            width: 2560,
            height: 1440,
            scale: 1.0,
            app: String::new(),
            field: None,
            url: None,
            pid: None,
            window: None,
            origin,
            monitor: 0,
            windows: Vec::new(),
            monitors: Vec::new(),
            ax_refs: Default::default(),
            offscreen: Vec::new(),
            pointer: None,
        }
    }

    fn at_mouse(x: f64, y: f64) -> Option<PointerInfo> {
        Some(PointerInfo {
            x,
            y,
            monitor: 1,
            idle_seconds: Some(1.0),
            under: None,
            display_window: None,
        })
    }

    #[test]
    fn the_item_under_the_mouse_has_the_origin_subtracted_once() {
        // The display above the primary: origin (1, -1440). The mouse at (51, -1430) is item
        // pixel (50, 10): inside the small button, inside the big toolbar too.
        let mut s = screen((1.0, -1440.0));
        s.pointer = at_mouse(51.0, -1430.0);
        let mut toolbar = item(0.0, 0.0, 400.0, 40.0);
        toolbar.index = 1;
        let mut button = item(40.0, 0.0, 60.0, 20.0);
        button.index = 2;
        let mut far = item(500.0, 500.0, 600.0, 600.0);
        far.index = 3;
        assert_eq!(
            s.under_mouse(&[toolbar.clone(), button.clone(), far.clone()]),
            Some(2)
        );
        assert_eq!(s.to_points(&button), (51.0, -1430.0));
        // Treating the virtual-desktop point as capture pixels would have missed every item.
        assert_eq!(s.under_mouse(&[far]), None);
        // Mouse on another display entirely: nothing here is under it.
        s.pointer = at_mouse(51.0, 300.0);
        assert_eq!(s.under_mouse(&[toolbar, button]), None);
        s.pointer = None;
        assert_eq!(s.pointer_px(), None);
    }

    #[test]
    fn a_click_adds_the_display_origin_once() {
        // The monitor above the primary on the development machine: origin (1, -1440).
        let at = screen((1.0, -1440.0)).to_points(&item(40.0, 0.0, 60.0, 20.0));
        assert_eq!(at, (51.0, -1430.0));
    }

    #[test]
    fn a_click_on_the_primary_is_unchanged() {
        assert_eq!(
            screen((0.0, 0.0)).to_points(&item(100.0, 100.0, 200.0, 200.0)),
            (150.0, 150.0)
        );
    }

    #[test]
    fn regions_split_the_capture_into_ninths() {
        let s = screen((0.0, 0.0));
        assert_eq!(s.region(&item(0.0, 0.0, 10.0, 10.0)), "top-left");
        assert_eq!(
            s.region(&item(1270.0, 710.0, 1290.0, 730.0)),
            "middle-center"
        );
        assert_eq!(
            s.region(&item(2550.0, 1430.0, 2560.0, 1440.0)),
            "bottom-right"
        );
    }

    #[test]
    fn role_words_match_the_python_table() {
        assert_eq!(role_word("AXButton"), "button");
        assert_eq!(role_word("AXTextField"), "field");
        assert_eq!(role_word("AXDockItem"), "taskbar item");
        assert_eq!(role_word("SomethingNew"), "other");
    }

    #[test]
    fn a_text_field_is_recognised_and_a_button_is_not() {
        let mut f = Field {
            role: "AXTextField".into(),
            label: String::new(),
            placeholder: String::new(),
            value: String::new(),
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        };
        assert!(f.is_text());
        f.role = "AXButton".into();
        assert!(!f.is_text());
    }
}
