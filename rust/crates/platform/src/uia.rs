//! The bounded accessibility walk, and the UI Automation adapter that feeds it.
//!
//! This is a port of `typesafe_computer_use_win/axwalk.py` plus the UIA section of
//! `typesafe_computer_use_win/windows.py`. The Python implementation is the reference and every
//! pruning rule below exists because it broke something real; the comment on each rule is the
//! incident, and deleting the comment loses the only record of why the code is shaped this way.
//!
//! The split is the same as the Python one and for the same reason: the pruning rules reach the
//! tree only through the [`AxTree`] trait, so they are pure, platform-free and tested against a
//! plain in-memory tree. That is what lets `tests/test_ax.py` exist without a desktop, and it is
//! why those rules are still correct years after the incidents that caused them.

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::hash::Hash;
use std::time::Instant;

use windows::core::Interface;
use windows::core::HRESULT;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, SAFEARRAY,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationExpandCollapsePattern,
    IUIAutomationInvokePattern, IUIAutomationSelectionItemPattern, IUIAutomationTogglePattern,
    IUIAutomationTreeWalker, IUIAutomationValuePattern, UIA_ButtonControlTypeId,
    UIA_CalendarControlTypeId, UIA_CheckBoxControlTypeId, UIA_ComboBoxControlTypeId,
    UIA_CustomControlTypeId, UIA_DataGridControlTypeId, UIA_DataItemControlTypeId,
    UIA_DocumentControlTypeId, UIA_EditControlTypeId, UIA_ExpandCollapsePatternId,
    UIA_GroupControlTypeId, UIA_HeaderControlTypeId, UIA_HeaderItemControlTypeId,
    UIA_HyperlinkControlTypeId, UIA_ImageControlTypeId, UIA_InvokePatternId, UIA_ListControlTypeId,
    UIA_ListItemControlTypeId, UIA_MenuBarControlTypeId, UIA_MenuControlTypeId,
    UIA_MenuItemControlTypeId, UIA_PaneControlTypeId, UIA_ProgressBarControlTypeId,
    UIA_RadioButtonControlTypeId, UIA_ScrollBarControlTypeId, UIA_SelectionItemPatternId,
    UIA_SeparatorControlTypeId, UIA_SliderControlTypeId, UIA_SpinnerControlTypeId,
    UIA_SplitButtonControlTypeId, UIA_StatusBarControlTypeId, UIA_TabControlTypeId,
    UIA_TabItemControlTypeId, UIA_TableControlTypeId, UIA_TextControlTypeId,
    UIA_ThumbControlTypeId, UIA_TitleBarControlTypeId, UIA_TogglePatternId,
    UIA_ToolBarControlTypeId, UIA_ToolTipControlTypeId, UIA_TreeControlTypeId,
    UIA_TreeItemControlTypeId, UIA_ValuePatternId, UIA_WindowControlTypeId,
};

// ---------------------------------------------------------------- the rules, platform-free

/// A frame in virtual-desktop coordinates: x, y, w, h.
pub type Frame = (f64, f64, f64, f64);

/// Roles that are worth reporting even when they carry no press action: the user can still be told
/// where a text field or a list row is.
pub const AX_ACTIONABLE_ROLES: &[&str] = &[
    "AXButton",
    "AXCell",
    "AXCheckBox",
    "AXComboBox",
    "AXDisclosureTriangle",
    "AXImage",
    "AXIncrementor",
    "AXLink",
    "AXMenuBarItem",
    "AXMenuButton",
    "AXPopUpButton",
    "AXRadioButton",
    "AXRow",
    "AXSearchField",
    "AXSlider",
    "AXTab",
    "AXTextArea",
    "AXTextField",
];

/// A bare child, usually a decorative AXImage, borrows the label of a parent that is itself a
/// control: an icon-only button hangs its name on the button, not on the icon.
pub const AX_LABEL_PARENT_ROLES: &[&str] = &[
    "AXButton",
    "AXCell",
    "AXCheckBox",
    "AXLink",
    "AXMenuButton",
    "AXPopUpButton",
    "AXRadioButton",
    "AXRow",
    "AXTab",
];

/// List containers keep their label in a shallow static text rather than on themselves.
pub const AX_LABEL_DESCENDANT_ROLES: &[&str] = &["AXCell", "AXRow"];

/// A closed menu: thousands of zero-sized items, none of them on screen.
///
/// Ported verbatim from Python, including the fact that the Windows role vocabulary never produces
/// `AXMenu` (`UIA_TO_AX` maps `MenuItemControl` but not `MenuControl`), so on this platform the
/// rule is carried but idle. Mapping `MenuControl` onto `AXMenu` would also throw away an *open*
/// context menu, which on Windows is a real and pressable thing, so the reference leaves it alone.
pub const AX_SKIP_SUBTREE_ROLES: &[&str] = &["AXMenu"];

pub const AX_NODE_CAP: usize = 4000;
pub const AX_TIME_CAP: f64 = 0.6;
/// Off-screen controls collected before the walk stops looking for more.
pub const AX_OFFSCREEN_CAP: usize = 120;
/// Anything thinner is a Chromium sliver for a node that is scrolled out of the viewport.
pub const AX_MIN_SIDE_PT: f64 = 4.0;
/// Children scanned per level when recovering a label.
pub const AX_FANOUT: usize = 8;

/// What the walk needs to know about one node.
#[derive(Debug, Clone, PartialEq)]
pub struct AxAttrs {
    pub role: String,
    pub label: String,
    pub frame: Option<Frame>,
}

impl AxAttrs {
    pub fn new(role: &str, label: &str, frame: Option<Frame>) -> Self {
        Self {
            role: role.to_string(),
            label: label.to_string(),
            frame,
        }
    }
}

/// True when a real frame lies wholly outside the captured display: a note list thousands of
/// screens down, or a web node the browser parked above the viewport. A zero-size frame claims
/// nothing, which is what an application element and a closed menu report, so their subtrees are
/// still worth a look.
///
/// A frame is in virtual-desktop coordinates, which do not start at zero on a second monitor and go
/// negative on one placed left of or above the primary. `origin` is where the captured display
/// sits, so a window at x=5120 counts as on screen when that is the display being read and off it
/// when it is not. This machine has a monitor at x=2561 and one at y=-1440; treating 0 as the left
/// or top edge would make every window on either of them invisible to the walk.
pub fn off_display(
    frame: Option<Frame>,
    display_w: f64,
    display_h: f64,
    origin: (f64, f64),
) -> bool {
    let Some((x, y, w, h)) = frame else {
        return false;
    };
    if w <= 0.0 || h <= 0.0 {
        return false;
    }
    let (left, top) = origin;
    let (right, bottom) = (left + display_w, top + display_h);
    x >= right || y >= bottom || x + w <= left || y + h <= top
}

/// Identity of a node for de-duplication: same role, label and frame is the same control, whatever
/// object the bridge wrapped it in. Frameless and zero-size nodes are containers and are never
/// keyed.
pub fn subtree_key(
    role: &str,
    label: &str,
    frame: Option<Frame>,
) -> Option<(String, String, i64, i64, i64, i64)> {
    let (x, y, w, h) = frame?;
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    Some((
        role.to_string(),
        label.to_string(),
        x.round() as i64,
        y.round() as i64,
        w.round() as i64,
        h.round() as i64,
    ))
}

/// A frame big enough to aim a pixel at. Chromium clamps a scrolled-out web node to a sliver, and a
/// click on a 1px-tall link lands on whatever is behind it.
pub fn clickable(frame: Option<Frame>) -> bool {
    match frame {
        Some((_, _, w, h)) => w.min(h) >= AX_MIN_SIDE_PT,
        None => false,
    }
}

/// The only way the rules reach a tree. A Mac AX tree, a Windows UIA tree and a plain dictionary in
/// a test all walk through the same code because this is the whole interface.
///
/// `pressable` is separate from `attrs` because asking UIA for a pattern is a cross-process call
/// per node, and the walk only needs the answer for a node it is about to emit.
pub trait AxTree {
    type Node: Clone;
    /// Identity across fetches: two handles on one control must give one id, or a self-listing
    /// application walks forever.
    type Id: Eq + Hash;

    fn identity(&self, node: &Self::Node) -> Self::Id;
    fn children(&self, node: &Self::Node) -> Vec<Self::Node>;
    fn attrs(&self, node: &Self::Node) -> AxAttrs;
    fn pressable(&self, node: &Self::Node) -> bool;
}

/// One reported control, carrying the handle it came from so a later press names this element and
/// not a lookalike found again by coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit<N> {
    pub role: String,
    pub label: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub pressable: bool,
    pub handle: N,
}

/// The on-screen controls, the reachable off-screen ones, and whether a cap cut the walk short.
#[derive(Debug, Clone, PartialEq)]
pub struct WalkedOf<N> {
    pub on_screen: Vec<Hit<N>>,
    pub off_screen: Vec<Hit<N>>,
    pub capped: bool,
}

/// The caps are the point: an unbounded walk of a note list or a long web page costs seconds and
/// finds nothing on screen.
///
/// The Python version found that a walk rooted at the *process* was hitting the node cap and never
/// reaching the window it was actually about. Scoping to one window made on-screen controls go up,
/// 124 to 168, on the same desktop.
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    pub node_cap: usize,
    pub time_cap: f64,
    pub offscreen_cap: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            node_cap: AX_NODE_CAP,
            time_cap: AX_TIME_CAP,
            offscreen_cap: AX_OFFSCREEN_CAP,
        }
    }
}

/// The first static text within two levels, which is where list rows hide their label.
fn descendant_label<T: AxTree>(tree: &T, kids: &[T::Node]) -> String {
    for kid in kids.iter().take(AX_FANOUT) {
        let a = tree.attrs(kid);
        if a.role == "AXStaticText" && !a.label.is_empty() {
            return a.label;
        }
    }
    for kid in kids.iter().take(AX_FANOUT) {
        for grandkid in tree.children(kid).iter().take(AX_FANOUT) {
            let a = tree.attrs(grandkid);
            if a.role == "AXStaticText" && !a.label.is_empty() {
                return a.label;
            }
        }
    }
    String::new()
}

/// Breadth-first hunt for labelled controls: the on-screen ones, the reachable off-screen ones, and
/// whether a cap cut the walk short.
///
/// A node that misses the display, or that the app clamped to a sliver, is not on screen and is not
/// offered as one: its subtree stays pruned from the on-screen list. But a press does not need a
/// node to be visible, so a labelled one that accepts the action is collected separately, down to
/// `offscreen_cap`, after which those subtrees are dropped again and the walk is the old one.
pub fn walk_actionable<T: AxTree>(
    tree: &T,
    root: T::Node,
    display_w: f64,
    display_h: f64,
    origin: (f64, f64),
    caps: Caps,
    clock: &mut dyn FnMut() -> f64,
) -> WalkedOf<T::Node> {
    let mut on_screen: Vec<Hit<T::Node>> = Vec::new();
    let mut off_screen: Vec<Hit<T::Node>> = Vec::new();
    let deadline = clock() + caps.time_cap;
    let mut queue: VecDeque<(T::Node, String, bool, bool)> =
        VecDeque::from([(root, String::new(), false, false)]);
    let mut seen = 0usize;
    // Elements compare by identity across fetches, so an app that lists itself as its own child is
    // walked once instead of forever.
    let mut visited: HashSet<T::Id> = HashSet::new();
    // A control handed over as several distinct objects is *emitted* once. The repeat is still
    // walked: UIA wraps a window in panes carrying the window's own size and label, and pruning on
    // the repeat loses the whole app. The re-walk is bounded by node_cap and time_cap.
    let mut visited_keys: HashSet<(String, String, i64, i64, i64, i64)> = HashSet::new();

    while let Some(item) = {
        if seen >= caps.node_cap || clock() >= deadline {
            return WalkedOf {
                on_screen,
                off_screen,
                capped: true,
            };
        }
        queue.pop_front()
    } {
        let (node, parent_label, parent_emitted, mut hidden) = item;
        if !visited.insert(tree.identity(&node)) {
            continue;
        }
        seen += 1;
        let AxAttrs {
            role,
            label: own_label,
            frame,
        } = tree.attrs(&node);
        if AX_SKIP_SUBTREE_ROLES.contains(&role.as_str()) {
            continue;
        }
        let key = subtree_key(&role, &own_label, frame);
        // The same control handed over again, or a wrapper standing in for it.
        let repeat = match &key {
            Some(k) => !visited_keys.insert(k.clone()),
            None => false,
        };
        hidden = hidden || off_display(frame, display_w, display_h, origin);
        if hidden && off_screen.len() >= caps.offscreen_cap {
            continue; // nothing left to collect down there, and it never counted on screen
        }
        let kids = tree.children(&node);
        let mut label = own_label.clone();
        let mut inherited = false;
        if label.is_empty() && AX_LABEL_DESCENDANT_ROLES.contains(&role.as_str()) {
            label = descendant_label(tree, &kids);
        }
        if label.is_empty() && !parent_label.is_empty() {
            label = parent_label;
            inherited = true;
        }
        let mut emitted = false;
        // Something already stands for this control.
        let duplicate = repeat || (inherited && parent_emitted);
        // A nameless group or pane is a Chromium layout box, not a control.
        let nameless_group = role == "AXGroup" && own_label.is_empty();
        let visible = !hidden && clickable(frame);
        if !label.is_empty() && !duplicate && !nameless_group {
            if visible {
                let pressable = tree.pressable(&node);
                if pressable || AX_ACTIONABLE_ROLES.contains(&role.as_str()) {
                    let (x, y, w, h) = frame.expect("clickable implies a frame");
                    on_screen.push(Hit {
                        role: role.clone(),
                        label: label.clone(),
                        x,
                        y,
                        w,
                        h,
                        pressable,
                        handle: node.clone(),
                    });
                    emitted = true;
                }
            } else if let Some((x, y, w, h)) = frame {
                // Off-screen controls are collected separately and only when they accept a press:
                // they are reachable without a pixel, which is exactly why a wrong one is
                // dangerous, so an unpressable one is never offered.
                if off_screen.len() < caps.offscreen_cap && tree.pressable(&node) {
                    off_screen.push(Hit {
                        role: role.clone(),
                        label: label.clone(),
                        x,
                        y,
                        w,
                        h,
                        pressable: true,
                        handle: node.clone(),
                    });
                }
            }
        }
        let child_label = if AX_LABEL_PARENT_ROLES.contains(&role.as_str()) {
            own_label
        } else {
            String::new()
        };
        for kid in kids {
            queue.push_back((kid, child_label.clone(), emitted, hidden));
        }
    }
    WalkedOf {
        on_screen,
        off_screen,
        capped: false,
    }
}

// ---------------------------------------------------------------- the UIA adapter

/// UIA control types, under the AX names the rest of the project already speaks. Ported verbatim
/// from `UIA_TO_AX`; an unlisted type keeps its UIA name, which is how `WindowControl` and
/// `MenuControl` reach the rules unchanged.
pub const UIA_TO_AX: &[(&str, &str)] = &[
    ("ButtonControl", "AXButton"),
    ("CheckBoxControl", "AXCheckBox"),
    ("ComboBoxControl", "AXComboBox"),
    ("DataItemControl", "AXRow"),
    ("DocumentControl", "AXTextArea"),
    ("EditControl", "AXTextField"),
    ("HyperlinkControl", "AXLink"),
    ("ImageControl", "AXImage"),
    ("ListItemControl", "AXCell"),
    ("MenuItemControl", "AXMenuBarItem"),
    ("RadioButtonControl", "AXRadioButton"),
    ("SliderControl", "AXSlider"),
    ("SplitButtonControl", "AXMenuButton"),
    ("TabItemControl", "AXTab"),
    ("TextControl", "AXStaticText"),
    ("TreeItemControl", "AXRow"),
    ("GroupControl", "AXGroup"),
    ("PaneControl", "AXGroup"),
];

/// A value longer than this is a document, not a label.
pub const VALUE_CHARS: usize = 120;

/// The UIA control type name for an id, the string `uiautomation` would have reported.
fn control_type_name(id: i32) -> &'static str {
    match id {
        x if x == UIA_ButtonControlTypeId.0 => "ButtonControl",
        x if x == UIA_CalendarControlTypeId.0 => "CalendarControl",
        x if x == UIA_CheckBoxControlTypeId.0 => "CheckBoxControl",
        x if x == UIA_ComboBoxControlTypeId.0 => "ComboBoxControl",
        x if x == UIA_CustomControlTypeId.0 => "CustomControl",
        x if x == UIA_DataGridControlTypeId.0 => "DataGridControl",
        x if x == UIA_DataItemControlTypeId.0 => "DataItemControl",
        x if x == UIA_DocumentControlTypeId.0 => "DocumentControl",
        x if x == UIA_EditControlTypeId.0 => "EditControl",
        x if x == UIA_GroupControlTypeId.0 => "GroupControl",
        x if x == UIA_HeaderControlTypeId.0 => "HeaderControl",
        x if x == UIA_HeaderItemControlTypeId.0 => "HeaderItemControl",
        x if x == UIA_HyperlinkControlTypeId.0 => "HyperlinkControl",
        x if x == UIA_ImageControlTypeId.0 => "ImageControl",
        x if x == UIA_ListControlTypeId.0 => "ListControl",
        x if x == UIA_ListItemControlTypeId.0 => "ListItemControl",
        x if x == UIA_MenuBarControlTypeId.0 => "MenuBarControl",
        x if x == UIA_MenuControlTypeId.0 => "MenuControl",
        x if x == UIA_MenuItemControlTypeId.0 => "MenuItemControl",
        x if x == UIA_PaneControlTypeId.0 => "PaneControl",
        x if x == UIA_ProgressBarControlTypeId.0 => "ProgressBarControl",
        x if x == UIA_RadioButtonControlTypeId.0 => "RadioButtonControl",
        x if x == UIA_ScrollBarControlTypeId.0 => "ScrollBarControl",
        x if x == UIA_SeparatorControlTypeId.0 => "SeparatorControl",
        x if x == UIA_SliderControlTypeId.0 => "SliderControl",
        x if x == UIA_SpinnerControlTypeId.0 => "SpinnerControl",
        x if x == UIA_SplitButtonControlTypeId.0 => "SplitButtonControl",
        x if x == UIA_StatusBarControlTypeId.0 => "StatusBarControl",
        x if x == UIA_TabControlTypeId.0 => "TabControl",
        x if x == UIA_TabItemControlTypeId.0 => "TabItemControl",
        x if x == UIA_TableControlTypeId.0 => "TableControl",
        x if x == UIA_TextControlTypeId.0 => "TextControl",
        x if x == UIA_ThumbControlTypeId.0 => "ThumbControl",
        x if x == UIA_TitleBarControlTypeId.0 => "TitleBarControl",
        x if x == UIA_ToolBarControlTypeId.0 => "ToolBarControl",
        x if x == UIA_ToolTipControlTypeId.0 => "ToolTipControl",
        x if x == UIA_TreeControlTypeId.0 => "TreeControl",
        x if x == UIA_TreeItemControlTypeId.0 => "TreeItemControl",
        x if x == UIA_WindowControlTypeId.0 => "WindowControl",
        _ => "",
    }
}

fn ax_role(control_type: i32) -> String {
    let name = control_type_name(control_type);
    for (uia, ax) in UIA_TO_AX {
        if *uia == name {
            return (*ax).to_string();
        }
    }
    name.to_string()
}

/// A handle on one live element: what a later press, focus or value read names.
///
/// Holding the element rather than coordinates is the point. A control found again by position can
/// be a different control by the time the press lands.
#[derive(Clone)]
pub struct PressHandle {
    element: IUIAutomationElement,
}

impl std::fmt::Debug for PressHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PressHandle")
    }
}

impl PartialEq for PressHandle {
    fn eq(&self, other: &Self) -> bool {
        self.element == other.element
    }
}

impl PressHandle {
    pub fn element(&self) -> &IUIAutomationElement {
        &self.element
    }
}

/// One control the walk found, with the handle that presses it.
pub type Node = Hit<PressHandle>;
/// The result of [`walk_window`].
pub type Walked = WalkedOf<PressHandle>;

thread_local! {
    /// COM is per-thread and apartment-threaded, and `CoInitializeEx` must happen before the first
    /// `CoCreateInstance` on *this* thread. A `IUIAutomation` may not cross threads, so it is
    /// cached per thread rather than once per process; every entry point here goes through
    /// `with_automation`, which is why no caller can reach UIA from an uninitialised thread.
    static AUTOMATION: RefCell<Option<IUIAutomation>> = const { RefCell::new(None) };
}

fn with_automation<R>(f: impl FnOnce(&IUIAutomation) -> R) -> Option<R> {
    AUTOMATION.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            unsafe {
                // S_FALSE means this thread was already initialised, which is not an error. A
                // thread someone else initialised as multi-threaded gives RPC_E_CHANGED_MODE; UIA
                // still works there, so the result is deliberately not fatal.
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                *slot = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok();
            }
        }
        slot.as_ref().map(f)
    })
}

// Freeing a runtime id needs `SafeArrayDestroy`, which lives in the `Win32_System_Ole` feature.
// Declaring the one function here leaves the manifest alone -- several agents share it -- and a walk
// of four thousand nodes cannot afford to leak an array per node.
#[link(name = "oleaut32")]
extern "system" {
    fn SafeArrayDestroy(psa: *mut SAFEARRAY) -> HRESULT;
}

/// The runtime id: the only identity that survives two fetches of one element, which is what stops
/// an application that lists itself as its own child from being walked forever.
fn runtime_id(element: &IUIAutomationElement) -> Vec<i32> {
    unsafe {
        let Ok(array) = element.GetRuntimeId() else {
            return Vec::new();
        };
        let ids = read_i32_array(array);
        let _ = SafeArrayDestroy(array);
        ids
    }
}

/// A one-dimensional array of i32, read out of the SAFEARRAY fields directly.
unsafe fn read_i32_array(array: *mut SAFEARRAY) -> Vec<i32> {
    if array.is_null() {
        return Vec::new();
    }
    let header = &*array;
    if header.cDims != 1 || header.cbElements as usize != std::mem::size_of::<i32>() {
        return Vec::new();
    }
    let count = header.rgsabound[0].cElements as usize;
    if header.pvData.is_null() || count == 0 {
        return Vec::new();
    }
    std::slice::from_raw_parts(header.pvData as *const i32, count).to_vec()
}

fn value_of(element: &IUIAutomationElement) -> Option<String> {
    unsafe {
        let pattern: IUIAutomationValuePattern = element
            .GetCurrentPattern(UIA_ValuePatternId)
            .ok()?
            .cast()
            .ok()?;
        pattern.CurrentValue().ok().map(|v| v.to_string())
    }
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The control's name, or a short value when it has no name (an address bar, a filled field).
fn label_of(element: &IUIAutomationElement) -> String {
    let name = unsafe { element.CurrentName() }
        .map(|n| n.to_string())
        .unwrap_or_default();
    if !name.trim().is_empty() {
        return collapse_whitespace(&name);
    }
    let value = value_of(element).unwrap_or_default();
    let trimmed = value.trim();
    if !trimmed.is_empty() && trimmed.chars().count() <= VALUE_CHARS {
        return collapse_whitespace(&value);
    }
    String::new()
}

fn frame_of(element: &IUIAutomationElement) -> Option<Frame> {
    let rect = unsafe { element.CurrentBoundingRectangle() }.ok()?;
    Some((
        rect.left as f64,
        rect.top as f64,
        (rect.right - rect.left) as f64,
        (rect.bottom - rect.top) as f64,
    ))
}

/// Patterns that amount to "activate this". A node carrying any of them is pressable.
const PRESS_PATTERNS: &[i32] = &[
    UIA_InvokePatternId.0,
    UIA_TogglePatternId.0,
    UIA_SelectionItemPatternId.0,
    UIA_ExpandCollapsePatternId.0,
];

fn has_press_pattern(element: &IUIAutomationElement) -> bool {
    PRESS_PATTERNS.iter().any(|id| unsafe {
        element
            .GetCurrentPattern(windows::Win32::UI::Accessibility::UIA_PATTERN_ID(*id))
            .is_ok()
    })
}

/// The UIA tree, scoped to one window.
struct UiaTree {
    /// The raw view walker, held once: fetching one per node is a COM call per node.
    walker: IUIAutomationTreeWalker,
}

impl AxTree for UiaTree {
    type Node = PressHandle;
    type Id = Vec<i32>;

    fn identity(&self, node: &Self::Node) -> Self::Id {
        runtime_id(&node.element)
    }

    fn children(&self, node: &Self::Node) -> Vec<Self::Node> {
        // The **raw** view, through a TreeWalker, and it has to be the raw one.
        //
        // `FindAll` is not an alternative here: it traverses the *control* view whatever condition
        // it is given, and the control view hides real controls. Measured on one Chrome window:
        // every `AXTab` in the vertical tab strip reports zero children in the control view, so all
        // 31 per-tab `Close` buttons vanish -- a control view walk found 69 on-screen controls where
        // the Python reference found 109. The raw walker finds the same 109. `uiautomation`'s
        // `GetChildren`, which the reference calls, is a raw walk for exactly this reason.
        unsafe {
            let mut out = Vec::new();
            let Ok(first) = self.walker.GetFirstChildElement(&node.element) else {
                return out;
            };
            let mut current = first;
            loop {
                out.push(PressHandle {
                    element: current.clone(),
                });
                match self.walker.GetNextSiblingElement(&current) {
                    Ok(next) => current = next,
                    Err(_) => return out,
                }
                // A tree that reports itself as its own sibling would otherwise spin here; the node
                // cap would stop the walk, but not before it had built an unbounded child list.
                if out.len() >= AX_NODE_CAP {
                    return out;
                }
            }
        }
    }

    fn attrs(&self, node: &Self::Node) -> AxAttrs {
        let control = unsafe { node.element.CurrentControlType() }
            .map(|c| c.0)
            .unwrap_or(0);
        AxAttrs {
            role: ax_role(control),
            label: label_of(&node.element),
            frame: frame_of(&node.element),
        }
    }

    fn pressable(&self, node: &Self::Node) -> bool {
        has_press_pattern(&node.element)
    }
}

/// Every labelled control of **one window**: the on-screen ones, the pressable off-screen ones, and
/// whether a cap cut the walk short.
///
/// Scoped to the window, never to the process. Chrome is one process hosting every Chrome window,
/// so a whole-process root puts another window's controls in the tree. The display bounds check
/// hides most of them, but an off-screen control is offered *because* nothing on the capture can
/// contradict it: that is how a run whose goal was "like this post" on an X post pressed the `Like`
/// of a YouTube Music tab in a different Chrome window. Rooted at the window, the sibling's
/// controls are simply not reachable, while the window's own toolbar, tab strip and menus still
/// are.
///
/// `origin` is where the display being read sits on the virtual desktop; UIA reports every frame in
/// virtual-desktop coordinates, so without it a window on the second monitor looks thousands of
/// pixels off the right edge and the whole app is pruned as invisible. `display::monitor_of(hwnd)`
/// names the display a window is on.
pub fn walk_window(hwnd: isize, display_w: f64, display_h: f64, origin: (f64, f64)) -> Walked {
    walk_window_capped(hwnd, display_w, display_h, origin, Caps::default())
}

/// [`walk_window`] with the caps spelled out, for a caller that needs a cheaper or deeper walk.
pub fn walk_window_capped(
    hwnd: isize,
    display_w: f64,
    display_h: f64,
    origin: (f64, f64),
    caps: Caps,
) -> Walked {
    let empty = || WalkedOf {
        on_screen: Vec::new(),
        off_screen: Vec::new(),
        capped: false,
    };
    with_automation(|automation| {
        let root = match unsafe { automation.ElementFromHandle(HWND(hwnd as *mut _)) } {
            Ok(element) => PressHandle { element },
            Err(_) => return empty(),
        };
        let Ok(walker) = (unsafe { automation.RawViewWalker() }) else {
            return empty();
        };
        let tree = UiaTree { walker };
        let start = Instant::now();
        let mut clock = move || start.elapsed().as_secs_f64();
        walk_actionable(&tree, root, display_w, display_h, origin, caps, &mut clock)
    })
    .unwrap_or_else(empty)
}

/// Activate an element through whichever pattern it offers, in the order a user would expect:
/// invoke a button, toggle a checkbox, select a list row, expand a disclosure.
pub fn press(handle: &PressHandle) -> bool {
    let element = &handle.element;
    unsafe {
        if let Ok(p) = element.GetCurrentPattern(UIA_InvokePatternId) {
            if let Ok(p) = p.cast::<IUIAutomationInvokePattern>() {
                if p.Invoke().is_ok() {
                    return true;
                }
            }
        }
        if let Ok(p) = element.GetCurrentPattern(UIA_TogglePatternId) {
            if let Ok(p) = p.cast::<IUIAutomationTogglePattern>() {
                if p.Toggle().is_ok() {
                    return true;
                }
            }
        }
        if let Ok(p) = element.GetCurrentPattern(UIA_SelectionItemPatternId) {
            if let Ok(p) = p.cast::<IUIAutomationSelectionItemPattern>() {
                if p.Select().is_ok() {
                    return true;
                }
            }
        }
        if let Ok(p) = element.GetCurrentPattern(UIA_ExpandCollapsePatternId) {
            if let Ok(p) = p.cast::<IUIAutomationExpandCollapsePattern>() {
                if p.Expand().is_ok() {
                    return true;
                }
            }
        }
    }
    false
}

/// Give an element the keyboard focus, so typing goes where the caller means it to.
pub fn focus(handle: &PressHandle) -> bool {
    unsafe { handle.element.SetFocus().is_ok() }
}

/// Write an element's value. A read-only or unwilling element reports false rather than pretending.
pub fn set_value(handle: &PressHandle, text: &str) -> bool {
    unsafe {
        let Ok(pattern) = element_value_pattern(&handle.element) else {
            return false;
        };
        pattern.SetValue(&windows::core::BSTR::from(text)).is_ok()
    }
}

unsafe fn element_value_pattern(
    element: &IUIAutomationElement,
) -> windows::core::Result<IUIAutomationValuePattern> {
    element.GetCurrentPattern(UIA_ValuePatternId)?.cast()
}

/// What an element currently holds, when it holds anything.
pub fn value(handle: &PressHandle) -> Option<String> {
    value_of(&handle.element)
}

/// The control the keyboard is pointing at, so a typed answer lands in the field the user is in
/// rather than wherever the mouse last was.
pub fn focused_field() -> Option<Node> {
    with_automation(|automation| {
        let element = unsafe { automation.GetFocusedElement() }.ok()?;
        let control = unsafe { element.CurrentControlType() }
            .map(|c| c.0)
            .unwrap_or(0);
        let (x, y, w, h) = frame_of(&element).unwrap_or((0.0, 0.0, 0.0, 0.0));
        let role = ax_role(control);
        let label = label_of(&element);
        let pressable = has_press_pattern(&element);
        Some(Hit {
            role,
            label,
            x,
            y,
            w,
            h,
            pressable,
            handle: PressHandle { element },
        })
    })
    .flatten()
}

// ---------------------------------------------------------------- tests
//
// The pruning rules are pure, so they are tested against a plain in-memory tree with no desktop
// involved: exactly what `tests/test_ax.py` does, and the reason those rules are still correct.

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const DISPLAY: (f64, f64) = (1728.0, 1117.0);

    #[derive(Clone)]
    struct Fake {
        role: &'static str,
        label: &'static str,
        frame: Option<Frame>,
        press: bool,
        kids: Vec<&'static str>,
    }

    /// A plain in-memory tree, addressed by name so one node can be hung under several parents.
    struct MemTree {
        nodes: HashMap<&'static str, Fake>,
    }

    impl AxTree for MemTree {
        type Node = &'static str;
        type Id = &'static str;

        fn identity(&self, node: &Self::Node) -> Self::Id {
            *node
        }
        fn children(&self, node: &Self::Node) -> Vec<Self::Node> {
            self.nodes[*node].kids.clone()
        }
        fn attrs(&self, node: &Self::Node) -> AxAttrs {
            let n = &self.nodes[*node];
            AxAttrs::new(n.role, n.label, n.frame)
        }
        fn pressable(&self, node: &Self::Node) -> bool {
            self.nodes[*node].press
        }
    }

    struct Builder {
        nodes: HashMap<&'static str, Fake>,
    }

    impl Builder {
        fn new() -> Self {
            Self {
                nodes: HashMap::new(),
            }
        }

        fn add(
            mut self,
            name: &'static str,
            role: &'static str,
            label: &'static str,
            frame: Option<Frame>,
            press: bool,
            kids: &[&'static str],
        ) -> Self {
            self.nodes.insert(
                name,
                Fake {
                    role,
                    label,
                    frame,
                    press,
                    kids: kids.to_vec(),
                },
            );
            self
        }

        /// A control at the default frame the Python helper uses.
        fn ctl(
            self,
            name: &'static str,
            role: &'static str,
            label: &'static str,
            press: bool,
        ) -> Self {
            self.add(
                name,
                role,
                label,
                Some((10.0, 10.0, 100.0, 20.0)),
                press,
                &[],
            )
        }

        /// An application element reports a zero-size frame at the bottom of the display, not a
        /// real one.
        fn app(self, kids: &[&'static str]) -> MemTree {
            let this = self.add(
                "app",
                "AXApplication",
                "Explorer",
                Some((0.0, 1117.0, 0.0, 0.0)),
                false,
                kids,
            );
            MemTree { nodes: this.nodes }
        }

        fn root(self, name: &'static str) -> MemTree {
            assert!(self.nodes.contains_key(name));
            MemTree { nodes: self.nodes }
        }
    }

    fn walk_at(
        tree: &MemTree,
        root: &'static str,
        display: (f64, f64),
        origin: (f64, f64),
        caps: Caps,
        clock: &mut dyn FnMut() -> f64,
    ) -> WalkedOf<&'static str> {
        walk_actionable(tree, root, display.0, display.1, origin, caps, clock)
    }

    fn walk(tree: &MemTree) -> WalkedOf<&'static str> {
        walk_with(tree, Caps::default())
    }

    fn walk_with(tree: &MemTree, caps: Caps) -> WalkedOf<&'static str> {
        let mut clock = || 0.0;
        walk_at(tree, "app", DISPLAY, (0.0, 0.0), caps, &mut clock)
    }

    fn labels<'a>(out: &'a WalkedOf<&'static str>) -> Vec<&'a str> {
        out.on_screen.iter().map(|n| n.label.as_str()).collect()
    }

    fn hidden_labels<'a>(out: &'a WalkedOf<&'static str>) -> Vec<&'a str> {
        out.off_screen.iter().map(|n| n.label.as_str()).collect()
    }

    #[test]
    fn keeps_labelled_controls_and_reports_no_cap() {
        let tree = Builder::new()
            .ctl("share", "AXButton", "Share", false)
            .add(
                "pricing",
                "AXLink",
                "Pricing",
                Some((0.0, 40.0, 60.0, 16.0)),
                false,
                &[],
            )
            .app(&["share", "pricing"]);
        let out = walk(&tree);
        let seen: Vec<_> = out
            .on_screen
            .iter()
            .map(|n| (n.role.as_str(), n.label.as_str(), n.x, n.w))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("AXButton", "Share", 10.0, 100.0),
                ("AXLink", "Pricing", 0.0, 60.0)
            ]
        );
        assert!(!out.capped);
    }

    #[test]
    fn a_frameless_root_does_not_prune_the_whole_tree() {
        let tree = Builder::new()
            .ctl("share", "AXButton", "Share", false)
            .add("app", "AXApplication", "Explorer", None, false, &["share"])
            .root("app");
        assert_eq!(labels(&walk(&tree)), vec!["Share"]);
    }

    #[test]
    fn drops_unlabelled_controls() {
        let tree = Builder::new().ctl("b", "AXButton", "", false).app(&["b"]);
        assert!(labels(&walk(&tree)).is_empty());
    }

    #[test]
    fn skips_nameless_group_even_when_pressable() {
        let tree = Builder::new()
            .ctl("bare", "AXGroup", "", true)
            .add(
                "bar",
                "AXGroup",
                "Toolbar",
                Some((10.0, 40.0, 100.0, 20.0)),
                true,
                &[],
            )
            .app(&["bare", "bar"]);
        assert_eq!(labels(&walk(&tree)), vec!["Toolbar"]);
    }

    #[test]
    fn press_action_makes_an_unlisted_role_actionable() {
        let yes = Builder::new()
            .ctl("t", "AXStaticText", "Sign in", true)
            .app(&["t"]);
        assert_eq!(labels(&walk(&yes)), vec!["Sign in"]);
        let no = Builder::new()
            .ctl("t", "AXStaticText", "Sign in", false)
            .app(&["t"]);
        assert!(labels(&walk(&no)).is_empty());
    }

    #[test]
    fn prunes_slivers_and_offscreen_frames() {
        let tree = Builder::new()
            .add(
                "clamped",
                "AXLink",
                "clamped",
                Some((100.0, 125.0, 72.0, 1.0)),
                false,
                &[],
            )
            .add(
                "narrow",
                "AXLink",
                "narrow",
                Some((100.0, 125.0, 2.0, 30.0)),
                false,
                &[],
            )
            .add(
                "below",
                "AXLink",
                "below",
                Some((100.0, 40000.0, 200.0, 30.0)),
                false,
                &[],
            )
            .add(
                "above",
                "AXLink",
                "above",
                Some((100.0, -300.0, 200.0, 30.0)),
                false,
                &[],
            )
            .add(
                "ok",
                "AXLink",
                "on screen",
                Some((100.0, 125.0, 72.0, 30.0)),
                false,
                &[],
            )
            .app(&["clamped", "narrow", "below", "above", "ok"]);
        assert_eq!(labels(&walk(&tree)), vec!["on screen"]);
    }

    #[test]
    fn offscreen_container_prunes_its_whole_subtree() {
        let tree = Builder::new()
            .ctl("del", "AXButton", "Delete", false)
            .add(
                "row",
                "AXRow",
                "Note 900",
                Some((1085.0, 42718.0, 280.0, 68.0)),
                false,
                &["del"],
            )
            .app(&["row"]);
        assert!(labels(&walk(&tree)).is_empty());
    }

    #[test]
    fn a_scrolled_out_link_is_collected_off_screen_without_joining_the_items() {
        let tree = Builder::new()
            .add(
                "reg",
                "AXLink",
                "Register Now",
                Some((320.0, -4200.0, 120.0, 32.0)),
                true,
                &[],
            )
            .add(
                "sliver",
                "AXLink",
                "clamped",
                Some((100.0, 125.0, 72.0, 1.0)),
                true,
                &[],
            )
            .add(
                "ok",
                "AXLink",
                "on screen",
                Some((100.0, 125.0, 72.0, 30.0)),
                true,
                &[],
            )
            .app(&["reg", "sliver", "ok"]);
        let out = walk(&tree);
        assert_eq!(labels(&out), vec!["on screen"]);
        assert_eq!(hidden_labels(&out), vec!["Register Now", "clamped"]);
        assert!(out.off_screen.iter().all(|n| n.pressable));
        assert_eq!(
            out.off_screen[0].handle, "reg",
            "the handle follows the control it names"
        );
        assert!(!out.capped);
    }

    #[test]
    fn an_unlabelled_or_unpressable_off_screen_node_is_not_collected() {
        let tree = Builder::new()
            .add(
                "nameless",
                "AXLink",
                "",
                Some((320.0, -4200.0, 120.0, 32.0)),
                true,
                &[],
            )
            .add(
                "row",
                "AXRow",
                "Note 900",
                Some((1085.0, 42718.0, 280.0, 68.0)),
                false,
                &[],
            )
            .add(
                "group",
                "AXGroup",
                "",
                Some((0.0, -900.0, 400.0, 80.0)),
                true,
                &[],
            )
            .app(&["nameless", "row", "group"]);
        assert!(hidden_labels(&walk(&tree)).is_empty());
    }

    #[test]
    fn an_off_display_container_still_yields_its_pressable_children() {
        let tree = Builder::new()
            .add(
                "del",
                "AXButton",
                "Delete",
                Some((1300.0, 42730.0, 40.0, 40.0)),
                true,
                &[],
            )
            .add(
                "row",
                "AXRow",
                "Note 900",
                Some((1085.0, 42718.0, 280.0, 68.0)),
                true,
                &["del"],
            )
            .app(&["row"]);
        let out = walk(&tree);
        assert!(out.on_screen.is_empty());
        assert_eq!(hidden_labels(&out), vec!["Note 900", "Delete"]);
    }

    #[test]
    fn the_offscreen_cap_stops_collection_and_leaves_the_on_screen_walk_alone() {
        let names = ["n0", "n1", "n2", "n3", "n4", "n5", "n6", "n7"];
        let labels_ = [
            "Note 0", "Note 1", "Note 2", "Note 3", "Note 4", "Note 5", "Note 6", "Note 7",
        ];
        let mut b = Builder::new().ctl("new", "AXButton", "New Note", true);
        for (i, name) in names.iter().enumerate() {
            b = b.add(
                name,
                "AXRow",
                labels_[i],
                Some((1085.0, 4000.0 + 70.0 * i as f64, 280.0, 68.0)),
                true,
                &[],
            );
        }
        let mut kids = vec!["new"];
        kids.extend_from_slice(&names);
        let tree = b.app(&kids);
        let out = walk_with(
            &tree,
            Caps {
                offscreen_cap: 3,
                ..Caps::default()
            },
        );
        assert_eq!(hidden_labels(&out), vec!["Note 0", "Note 1", "Note 2"]);
        assert_eq!(labels(&out), vec!["New Note"]);
        assert!(!out.capped);
    }

    #[test]
    fn skips_menu_subtrees_but_keeps_the_menu_bar_item() {
        let tree = Builder::new()
            .add(
                "item",
                "AXMenuItem",
                "New Folder",
                Some((0.0, 0.0, 100.0, 20.0)),
                false,
                &[],
            )
            .add(
                "menu",
                "AXMenu",
                "",
                Some((0.0, 1117.0, 0.0, 0.0)),
                false,
                &["item"],
            )
            .add(
                "file",
                "AXMenuBarItem",
                "File",
                Some((50.0, 0.0, 34.0, 24.0)),
                false,
                &["menu"],
            )
            .app(&["file"]);
        assert_eq!(labels(&walk(&tree)), vec!["File"]);
    }

    #[test]
    fn decorative_child_does_not_repeat_its_parents_label() {
        let tree = Builder::new()
            .add(
                "icon",
                "AXImage",
                "",
                Some((20.0, 12.0, 16.0, 16.0)),
                false,
                &[],
            )
            .add(
                "btn",
                "AXButton",
                "Add reaction",
                Some((10.0, 10.0, 100.0, 20.0)),
                false,
                &["icon"],
            )
            .app(&["btn"]);
        assert_eq!(labels(&walk(&tree)), vec!["Add reaction"]);
    }

    #[test]
    fn child_recovers_the_label_of_a_parent_that_was_not_emitted() {
        let tree = Builder::new()
            .add(
                "icon",
                "AXImage",
                "",
                Some((10.0, 40.0, 20.0, 20.0)),
                false,
                &[],
            )
            .add(
                "cell",
                "AXCell",
                "Inbox",
                Some((10.0, 10.0, 100.0, 2.0)),
                false,
                &["icon"],
            )
            .app(&["cell"]);
        let out = walk(&tree);
        let seen: Vec<_> = out
            .on_screen
            .iter()
            .map(|n| (n.role.as_str(), n.label.as_str()))
            .collect();
        assert_eq!(seen, vec![("AXImage", "Inbox")]);
    }

    #[test]
    fn window_title_does_not_leak_onto_its_buttons() {
        let tree = Builder::new()
            .add(
                "close",
                "AXButton",
                "",
                Some((882.0, 56.0, 16.0, 16.0)),
                false,
                &[],
            )
            .add(
                "win",
                "AXWindow",
                "Notes",
                Some((0.0, 0.0, 900.0, 600.0)),
                false,
                &["close"],
            )
            .app(&["win"]);
        assert!(labels(&walk(&tree)).is_empty());
    }

    #[test]
    fn row_recovers_its_label_from_a_shallow_static_text() {
        let tree = Builder::new()
            .add(
                "t1",
                "AXStaticText",
                "Projects",
                Some((12.0, 12.0, 80.0, 16.0)),
                false,
                &[],
            )
            .add(
                "r1",
                "AXRow",
                "",
                Some((10.0, 10.0, 100.0, 20.0)),
                false,
                &["t1"],
            )
            .add(
                "t2",
                "AXStaticText",
                "Downloads",
                Some((12.0, 42.0, 80.0, 16.0)),
                false,
                &[],
            )
            .add("g2", "AXGroup", "", None, false, &["t2"])
            .add(
                "r2",
                "AXRow",
                "",
                Some((10.0, 40.0, 100.0, 20.0)),
                false,
                &["g2"],
            )
            .app(&["r1", "r2"]);
        assert_eq!(labels(&walk(&tree)), vec!["Projects", "Downloads"]);
    }

    fn wide_app() -> MemTree {
        const NAMES: [&str; 20] = [
            "b0", "b1", "b2", "b3", "b4", "b5", "b6", "b7", "b8", "b9", "b10", "b11", "b12", "b13",
            "b14", "b15", "b16", "b17", "b18", "b19",
        ];
        let mut b = Builder::new();
        for (i, name) in NAMES.iter().enumerate() {
            b = b.add(
                name,
                "AXButton",
                name,
                Some((10.0 * i as f64, 10.0, 8.0, 20.0)),
                false,
                &[],
            );
        }
        b.app(&NAMES)
    }

    #[test]
    fn node_cap_stops_the_walk_and_is_reported() {
        let out = walk_with(
            &wide_app(),
            Caps {
                node_cap: 5,
                ..Caps::default()
            },
        );
        assert!(out.capped);
        // The application element itself costs one visit.
        assert_eq!(out.on_screen.len(), 4);
    }

    #[test]
    fn time_cap_stops_the_walk_and_is_reported() {
        let tree = wide_app();
        let mut tick = 0;
        let mut clock = move || {
            let now = if tick == 0 { 0.0 } else { 0.1 * tick as f64 };
            tick += 1;
            now
        };
        let caps = Caps {
            time_cap: 0.5,
            ..Caps::default()
        };
        let out = walk_at(&tree, "app", DISPLAY, (0.0, 0.0), caps, &mut clock);
        assert!(out.capped);
        assert!(!out.on_screen.is_empty() && out.on_screen.len() < 20);
    }

    #[test]
    fn the_walker_keeps_a_handle_to_every_element_it_reports() {
        let tree = Builder::new()
            .ctl("share", "AXButton", "Share", false)
            .app(&["share"]);
        let out = walk(&tree);
        assert_eq!(
            out.on_screen.iter().map(|n| n.handle).collect::<Vec<_>>(),
            vec!["share"]
        );
    }

    /// Ghostty hangs its menu bar under every window: same role, label and frame, new objects each
    /// time. Pruning the repeat would lose the whole app, so the subtree is walked again and only
    /// the emission is de-duplicated.
    #[test]
    fn a_subtree_repeated_under_several_parents_is_walked_once_and_emitted_once() {
        let win = Some((0.0, 40.0, 800.0, 600.0));
        let bar = Some((0.0, 0.0, 800.0, 24.0));
        let file = Some((40.0, 0.0, 30.0, 24.0));
        let tree = Builder::new()
            .add("file1", "AXMenuBarItem", "File", file, true, &[])
            .add("file2", "AXMenuBarItem", "File", file, true, &[])
            .add("file3", "AXMenuBarItem", "File", file, true, &[])
            .add("bar1", "AXMenuBar", "", bar, true, &["file1"])
            .add("bar2", "AXMenuBar", "", bar, true, &["file2"])
            .add("bar3", "AXMenuBar", "", bar, true, &["file3"])
            .add("win1", "AXWindow", "", win, true, &["bar1"])
            .add("win2", "AXWindow", "", win, true, &["bar2"])
            .add("win3", "AXWindow", "", win, true, &["bar3"])
            .add(
                "app",
                "AXApplication",
                "",
                None,
                true,
                &["win1", "win2", "win3"],
            )
            .root("app");
        let mut clock = || 0.0;
        let out = walk_at(
            &tree,
            "app",
            (1000.0, 800.0),
            (0.0, 0.0),
            Caps::default(),
            &mut clock,
        );
        assert_eq!(labels(&out), vec!["File"]);
        assert!(!out.capped);
    }

    #[test]
    fn an_app_that_lists_itself_as_a_child_terminates() {
        let tree = Builder::new()
            .add(
                "file",
                "AXMenuBarItem",
                "File",
                Some((40.0, 0.0, 30.0, 24.0)),
                true,
                &[],
            )
            .add(
                "bar",
                "AXMenuBar",
                "",
                Some((0.0, 0.0, 800.0, 24.0)),
                true,
                &["file"],
            )
            .add(
                "app",
                "AXApplication",
                "",
                None,
                true,
                &["app", "app", "bar"],
            )
            .root("app");
        let mut clock = || 0.0;
        let out = walk_at(
            &tree,
            "app",
            (1000.0, 800.0),
            (0.0, 0.0),
            Caps::default(),
            &mut clock,
        );
        assert_eq!(labels(&out), vec!["File"]);
        assert!(!out.capped);
    }

    // ------------------------------------------------------------ more than one monitor

    /// The frames the tree reports are virtual-desktop coordinates, so a window at x=5112 is only
    /// off screen relative to the primary display. Reading its own monitor, it is right there.
    #[test]
    fn a_window_on_a_second_monitor_is_on_screen_for_that_monitor() {
        let frame = Some((5112.0, 138.0, 1936.0, 1096.0));
        assert!(off_display(frame, 2560.0, 1440.0, (0.0, 0.0)));
        assert!(!off_display(frame, 1920.0, 1080.0, (5120.0, 0.0)));
    }

    /// Windows places a screen to the left at a negative origin, and one above the primary at a
    /// negative y: this machine has a monitor at y=-1440. Treating 0 as the left or top edge would
    /// make every window on it invisible to the walk.
    #[test]
    fn a_monitor_left_of_or_above_the_primary_has_negative_coordinates() {
        let left = Some((-1800.0, 100.0, 600.0, 400.0));
        assert!(off_display(left, 2560.0, 1440.0, (0.0, 0.0)));
        assert!(!off_display(left, 1920.0, 1080.0, (-1920.0, 0.0)));
        let above = Some((300.0, -1300.0, 600.0, 400.0));
        assert!(off_display(above, 2560.0, 1440.0, (0.0, 0.0)));
        assert!(!off_display(above, 2560.0, 1440.0, (0.0, -1440.0)));
    }

    #[test]
    fn a_frame_straddling_the_edge_of_the_captured_display_still_counts_as_on_it() {
        assert!(!off_display(
            Some((2500.0, 10.0, 200.0, 50.0)),
            2560.0,
            1440.0,
            (0.0, 0.0)
        ));
    }

    /// A control sitting on the second monitor must be found when that monitor is being walked.
    #[test]
    fn the_walk_passes_its_origin_down_to_the_on_screen_test() {
        let tree = Builder::new()
            .add(
                "button",
                "AXButton",
                "Send",
                Some((5200.0, 200.0, 120.0, 40.0)),
                true,
                &[],
            )
            .add("app", "AXApplication", "", None, true, &["button"])
            .root("app");
        let mut clock = || 0.0;
        let primary = walk_at(
            &tree,
            "app",
            (1920.0, 1080.0),
            (0.0, 0.0),
            Caps::default(),
            &mut clock,
        );
        assert!(
            labels(&primary).is_empty(),
            "against the primary display it is nowhere near the screen"
        );
        let mut clock = || 0.0;
        let second = walk_at(
            &tree,
            "app",
            (1920.0, 1080.0),
            (5120.0, 0.0),
            Caps::default(),
            &mut clock,
        );
        assert_eq!(labels(&second), vec!["Send"]);
    }

    // ------------------------------------------------------------ one process, several windows

    /// Two Chrome windows of one process: an X post in front, YouTube Music behind it. The music
    /// window's transport controls are scrolled out of the display, which is exactly why they
    /// survive as off-screen candidates: nothing on the capture contradicts a control nobody sees.
    fn chrome_windows() -> Builder {
        let win = Some((0.0, 0.0, 1728.0, 1000.0));
        Builder::new()
            .add(
                "reload",
                "AXButton",
                "Reload",
                Some((80.0, 40.0, 24.0, 24.0)),
                true,
                &[],
            )
            .add(
                "tab",
                "AXTab",
                "karpathy on X",
                Some((200.0, 8.0, 180.0, 24.0)),
                true,
                &[],
            )
            .add(
                "like",
                "AXButton",
                "Like",
                Some((400.0, -5000.0, 32.0, 32.0)),
                true,
                &[],
            )
            .add(
                "post",
                "AXWindow",
                "karpathy on X",
                win,
                false,
                &["reload", "tab", "like"],
            )
            .add(
                "play",
                "AXButton",
                "Play",
                Some((60.0, -9000.0, 32.0, 32.0)),
                true,
                &[],
            )
            .add(
                "dislike",
                "AXButton",
                "Dislike",
                Some((100.0, -9000.0, 32.0, 32.0)),
                true,
                &[],
            )
            .add(
                "mlike",
                "AXButton",
                "Like",
                Some((140.0, -9000.0, 32.0, 32.0)),
                true,
                &[],
            )
            .add(
                "music",
                "AXWindow",
                "YouTube Music",
                win,
                false,
                &["play", "dislike", "mlike"],
            )
    }

    /// The incident of runs/20261006-202038: the goal was "like this post" on an X post in Chrome,
    /// and the run pressed an off-screen `Like` at 0.92 confidence that belonged to a YouTube Music
    /// tab in a *different* Chrome window. Chrome is one process hosting many windows, and the walk
    /// was rooted at every window of the process, so the music window's controls were in the tree;
    /// being off screen, nothing on the capture could contradict them.
    ///
    /// Rooted at the window being looked at, the sibling's controls are simply not reachable, while
    /// the window's own chrome -- its toolbar and tab strip -- still is. That is what
    /// `ElementFromHandle` buys, and why this crate has no "every window of this pid" entry point.
    #[test]
    fn a_walk_scoped_to_one_window_cannot_offer_a_sibling_windows_control() {
        let whole_process = chrome_windows()
            .add("app", "AXGroup", "", None, false, &["post", "music"])
            .root("app");
        let mut clock = || 0.0;
        let bug = walk_at(
            &whole_process,
            "app",
            DISPLAY,
            (0.0, 0.0),
            Caps::default(),
            &mut clock,
        );
        assert_eq!(
            hidden_labels(&bug).iter().filter(|l| **l == "Like").count(),
            2,
            "the bug: both windows' Like buttons are offered"
        );
        assert!(hidden_labels(&bug).contains(&"Play"));

        let scoped = chrome_windows()
            .add("app", "AXGroup", "", None, false, &["post"])
            .root("app");
        let mut clock = || 0.0;
        let out = walk_at(
            &scoped,
            "app",
            DISPLAY,
            (0.0, 0.0),
            Caps::default(),
            &mut clock,
        );
        let hidden: Vec<_> = out
            .off_screen
            .iter()
            .map(|n| (n.role.as_str(), n.label.as_str()))
            .collect();
        assert_eq!(
            hidden,
            vec![("AXButton", "Like")],
            "only this window's own Like"
        );
        let mut found = labels(&out);
        found.sort_unstable();
        assert_eq!(
            found,
            vec!["Reload", "karpathy on X"],
            "the window's own toolbar and tab strip stay"
        );
        assert!(!hidden_labels(&out).contains(&"Play"));
        assert!(!hidden_labels(&out).contains(&"Dislike"));
    }

    // ------------------------------------------------------------ the role vocabulary

    #[test]
    fn roles_come_back_under_the_ax_names_the_project_speaks() {
        assert_eq!(ax_role(UIA_ButtonControlTypeId.0), "AXButton");
        assert_eq!(ax_role(UIA_EditControlTypeId.0), "AXTextField");
        assert_eq!(ax_role(UIA_ListItemControlTypeId.0), "AXCell");
        assert_eq!(ax_role(UIA_HyperlinkControlTypeId.0), "AXLink");
        assert_eq!(ax_role(UIA_TextControlTypeId.0), "AXStaticText");
        assert_eq!(ax_role(UIA_PaneControlTypeId.0), "AXGroup");
        assert_eq!(ax_role(UIA_TreeItemControlTypeId.0), "AXRow");
        // An unmapped type keeps its UIA name, so nothing is silently turned into a control.
        assert_eq!(ax_role(UIA_WindowControlTypeId.0), "WindowControl");
        assert_eq!(ax_role(UIA_MenuControlTypeId.0), "MenuControl");
    }

    // ------------------------------------------------------------ live, run explicitly

    /// Walk a real Chrome window and print the counts, to compare against the Python reference:
    ///
    /// ```text
    /// cargo test -p platform --lib live_chrome_window -- --ignored --nocapture
    /// ```
    ///
    /// `TCU_HWND=<handle>` walks that window instead of the first Chrome one, which is how this is
    /// compared against the Python reference on exactly the same window rather than a sibling.
    #[test]
    #[ignore = "needs a visible Chrome window on this desktop"]
    fn live_chrome_window() {
        use crate::{display, winlist};

        let asked: Option<isize> = std::env::var("TCU_HWND")
            .ok()
            .and_then(|v| v.trim().parse().ok());
        let windows = winlist::open_windows(200);
        let window = match asked {
            Some(hwnd) => windows
                .into_iter()
                .find(|w| w.hwnd == hwnd)
                .unwrap_or_else(|| panic!("no open window with handle {hwnd}")),
            None => windows
                .into_iter()
                .find(|w| w.app.contains("chrome") || w.title.contains("Chrome"))
                .expect("no Chrome window on this desktop"),
        };
        let monitors = display::monitors();
        let index = display::monitor_of(window.hwnd);
        let monitor = monitors[index];
        let started = Instant::now();
        let out = walk_window(
            window.hwnd,
            monitor.width() as f64,
            monitor.height() as f64,
            (monitor.left as f64, monitor.top as f64),
        );
        println!(
            "window {:?} hwnd {} on monitor {} {:?} in {:.2}s\non {} off {} capped {}",
            window.title,
            window.hwnd,
            index,
            monitor.bounds(),
            started.elapsed().as_secs_f64(),
            out.on_screen.len(),
            out.off_screen.len(),
            out.capped
        );
        for node in out.on_screen.iter().take(15) {
            println!("  {} {:?} at {},{}", node.role, node.label, node.x, node.y);
        }
        println!("off screen, the ones a press could reach without a pixel:");
        for node in out.off_screen.iter().take(15) {
            println!(
                "  {} {:?} at {},{} {}x{}",
                node.role, node.label, node.x, node.y, node.w, node.h
            );
        }
        assert!(!out.on_screen.is_empty(), "a Chrome window has controls");
    }
}
