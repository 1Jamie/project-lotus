use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneAnchor {
    None = 0,
    Fill = 1,
    Left = 2,
    Right = 3,
    Top = 4,
    Bottom = 5,
}

impl From<u32> for PaneAnchor {
    fn from(v: u32) -> Self {
        match v {
            1 => PaneAnchor::Fill,
            2 => PaneAnchor::Left,
            3 => PaneAnchor::Right,
            4 => PaneAnchor::Top,
            5 => PaneAnchor::Bottom,
            _ => PaneAnchor::None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StagedPaneLayout {
    pub rect: euclid::default::Rect<i32>,
    pub visible: bool,
}

impl StagedPaneLayout {
    pub fn new(rect: euclid::default::Rect<i32>, visible: bool) -> Self {
        Self { rect, visible }
    }
}

#[derive(Debug, Clone, Default)]
pub struct StagedLayout {
    pub panes: HashMap<String, StagedPaneLayout>,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
}

// ---------------------------------------------------------------------------
// Virtual layout tree (geometry only — compositor still sees flat leaf rects)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LayoutLength {
    Fixed(f32),
    Weight(f32),
}

impl LayoutLength {
    pub fn clamp_weight(w: f32) -> f32 {
        if !w.is_finite() || w < 0.0 {
            0.0
        } else {
            w
        }
    }
}

#[derive(Debug, Clone)]
pub struct DockChild {
    pub anchor: PaneAnchor, // Left/Right/Top/Bottom/Fill only
    pub node: LayoutNode,
    pub size: LayoutLength, // Fixed thickness for edge docks; Fill ignores
    pub min: f32,
    pub max: Option<f32>,
    pub resizable: bool,
    pub visible: bool,
    pub detached: bool,
}

#[derive(Debug, Clone)]
pub struct SplitChild {
    pub node: LayoutNode,
    pub size: LayoutLength,
    pub min: f32,
    pub max: Option<f32>,
    pub resizable: bool,
    pub visible: bool,
    pub detached: bool,
}

#[derive(Debug, Clone)]
pub enum LayoutNode {
    DockRoot { children: Vec<DockChild> },
    Split {
        id: String,
        direction: Axis,
        children: Vec<SplitChild>,
    },
    Leaf { pane_id: String },
}

impl LayoutNode {
    pub fn leaf(pane_id: impl Into<String>) -> Self {
        LayoutNode::Leaf {
            pane_id: pane_id.into(),
        }
    }

    /// Validate unique ids across leaves and splits. Returns Err with duplicate id.
    pub fn validate_unique_ids(&self) -> Result<(), String> {
        let mut seen = HashSet::new();
        self.collect_ids(&mut seen)
    }

    fn collect_ids(&self, seen: &mut HashSet<String>) -> Result<(), String> {
        match self {
            LayoutNode::Leaf { pane_id } => {
                if !seen.insert(pane_id.clone()) {
                    return Err(pane_id.clone());
                }
            }
            LayoutNode::Split { id, children, .. } => {
                if !seen.insert(id.clone()) {
                    return Err(id.clone());
                }
                for c in children {
                    c.node.collect_ids(seen)?;
                }
            }
            LayoutNode::DockRoot { children } => {
                for c in children {
                    c.node.collect_ids(seen)?;
                }
            }
        }
        Ok(())
    }

    pub fn set_leaf_fixed_size(&mut self, pane_id: &str, size: f32) -> bool {
        let size = if size.is_finite() { size.max(1.0) } else { 1.0 };
        self.map_split_child(pane_id, &mut |c| {
            c.size = LayoutLength::Fixed(size);
        }) || self.map_dock_child_leaf(pane_id, &mut |c| {
            c.size = LayoutLength::Fixed(size);
        })
    }

    pub fn set_leaf_weight(&mut self, pane_id: &str, weight: f32) -> bool {
        let weight = LayoutLength::clamp_weight(weight);
        self.map_split_child(pane_id, &mut |c| {
            c.size = LayoutLength::Weight(weight);
        }) || self.map_dock_child_leaf(pane_id, &mut |c| {
            c.size = LayoutLength::Weight(weight);
        })
    }

    /// Fixed size for a split/dock child keyed by leaf `pane_id` **or** nested split `id`.
    pub fn set_child_fixed_size(&mut self, id: &str, size: f32) -> bool {
        let size = if size.is_finite() { size.max(1.0) } else { 1.0 };
        self.map_split_child_key(id, &mut |c| {
            c.size = LayoutLength::Fixed(size);
        }) || self.map_dock_child_leaf(id, &mut |c| {
            c.size = LayoutLength::Fixed(size);
        })
    }

    /// Weight for a split/dock child keyed by leaf `pane_id` **or** nested split `id`.
    pub fn set_child_weight(&mut self, id: &str, weight: f32) -> bool {
        let weight = LayoutLength::clamp_weight(weight);
        self.map_split_child_key(id, &mut |c| {
            c.size = LayoutLength::Weight(weight);
        }) || self.map_dock_child_leaf(id, &mut |c| {
            c.size = LayoutLength::Weight(weight);
        })
    }

    /// Look up a split/dock child's length + Fixed flag by id (leaf or nested split).
    pub fn child_length(&self, id: &str) -> Option<(LayoutLength, bool /*is_fixed*/)> {
        let mut out = None;
        let _ = self.find_split_child(id, &mut |c| {
            let is_fixed = matches!(c.size, LayoutLength::Fixed(_));
            out = Some((c.size, is_fixed));
        }) || self.find_dock_child_leaf(id, &mut |c| {
            let is_fixed = matches!(c.size, LayoutLength::Fixed(_));
            out = Some((c.size, is_fixed));
        });
        out
    }

    pub fn child_participating(&self, id: &str) -> bool {
        let mut ok = false;
        let _ = self.find_split_child(id, &mut |c| {
            ok = child_participates_split(c);
        }) || self.find_dock_child_leaf(id, &mut |c| {
            ok = child_participates_dock(c);
        });
        ok
    }

    pub fn set_leaf_visible(&mut self, pane_id: &str, visible: bool) -> bool {
        self.map_split_child(pane_id, &mut |c| c.visible = visible)
            || self.map_dock_child_leaf(pane_id, &mut |c| c.visible = visible)
    }

    pub fn set_leaf_detached(&mut self, pane_id: &str, detached: bool) -> bool {
        self.map_split_child(pane_id, &mut |c| c.detached = detached)
            || self.map_dock_child_leaf(pane_id, &mut |c| c.detached = detached)
    }

    /// Apply weights keyed by child id (leaf or nested split id). Flips to Weight.
    pub fn set_split_weights_map(&mut self, split_id: &str, weights: &HashMap<String, f32>) -> bool {
        self.with_split_mut(split_id, &mut |children| {
            for c in children.iter_mut() {
                let key = child_key(&c.node);
                if let Some(&w) = weights.get(&key) {
                    c.size = LayoutLength::Weight(LayoutLength::clamp_weight(w));
                }
            }
        })
    }

    /// Positional weights: index = declared child order; hidden/detached consume index but skip apply.
    pub fn set_split_weights_list(&mut self, split_id: &str, weights: &[f32]) -> bool {
        self.with_split_mut(split_id, &mut |children| {
            for (i, c) in children.iter_mut().enumerate() {
                if i >= weights.len() {
                    break;
                }
                if c.visible && !c.detached {
                    c.size = LayoutLength::Weight(LayoutLength::clamp_weight(weights[i]));
                }
            }
        })
    }

    pub fn set_split_sizes_map(&mut self, split_id: &str, sizes: &HashMap<String, f32>) -> bool {
        self.with_split_mut(split_id, &mut |children| {
            for c in children.iter_mut() {
                let key = child_key(&c.node);
                if let Some(&s) = sizes.get(&key) {
                    let s = if s.is_finite() { s.max(1.0) } else { 1.0 };
                    c.size = LayoutLength::Fixed(s);
                }
            }
        })
    }

    pub fn set_split_sizes_list(&mut self, split_id: &str, sizes: &[f32]) -> bool {
        self.with_split_mut(split_id, &mut |children| {
            for (i, c) in children.iter_mut().enumerate() {
                if i >= sizes.len() {
                    break;
                }
                if c.visible && !c.detached {
                    let s = if sizes[i].is_finite() {
                        sizes[i].max(1.0)
                    } else {
                        1.0
                    };
                    c.size = LayoutLength::Fixed(s);
                }
            }
        })
    }

    fn with_split_mut<F: FnMut(&mut Vec<SplitChild>)>(&mut self, split_id: &str, f: &mut F) -> bool {
        match self {
            LayoutNode::Split { id, children, .. } if id == split_id => {
                f(children);
                true
            }
            LayoutNode::Split { children, .. } => {
                for c in children.iter_mut() {
                    if c.node.with_split_mut(split_id, f) {
                        return true;
                    }
                }
                false
            }
            LayoutNode::DockRoot { children } => {
                for c in children.iter_mut() {
                    if c.node.with_split_mut(split_id, f) {
                        return true;
                    }
                }
                false
            }
            LayoutNode::Leaf { .. } => false,
        }
    }

    fn map_split_child<F: FnMut(&mut SplitChild)>(&mut self, pane_id: &str, f: &mut F) -> bool {
        match self {
            LayoutNode::Split { children, .. } => {
                for c in children.iter_mut() {
                    if matches!(&c.node, LayoutNode::Leaf { pane_id: id } if id == pane_id) {
                        f(c);
                        return true;
                    }
                    if c.node.map_split_child(pane_id, f) {
                        return true;
                    }
                }
                false
            }
            LayoutNode::DockRoot { children } => {
                for c in children.iter_mut() {
                    if c.node.map_split_child(pane_id, f) {
                        return true;
                    }
                }
                false
            }
            LayoutNode::Leaf { .. } => false,
        }
    }

    /// Match split children by leaf pane_id **or** nested Split id.
    fn map_split_child_key<F: FnMut(&mut SplitChild)>(&mut self, id: &str, f: &mut F) -> bool {
        match self {
            LayoutNode::Split { children, .. } => {
                for c in children.iter_mut() {
                    if child_key(&c.node) == id {
                        f(c);
                        return true;
                    }
                    if c.node.map_split_child_key(id, f) {
                        return true;
                    }
                }
                false
            }
            LayoutNode::DockRoot { children } => {
                for c in children.iter_mut() {
                    if c.node.map_split_child_key(id, f) {
                        return true;
                    }
                }
                false
            }
            LayoutNode::Leaf { .. } => false,
        }
    }

    fn find_split_child<F: FnMut(&SplitChild)>(&self, id: &str, f: &mut F) -> bool {
        match self {
            LayoutNode::Split { children, .. } => {
                for c in children {
                    if child_key(&c.node) == id {
                        f(c);
                        return true;
                    }
                    if c.node.find_split_child(id, f) {
                        return true;
                    }
                }
                false
            }
            LayoutNode::DockRoot { children } => {
                for c in children {
                    if c.node.find_split_child(id, f) {
                        return true;
                    }
                }
                false
            }
            LayoutNode::Leaf { .. } => false,
        }
    }

    fn find_dock_child_leaf<F: FnMut(&DockChild)>(&self, pane_id: &str, f: &mut F) -> bool {
        match self {
            LayoutNode::DockRoot { children } => {
                for c in children {
                    if matches!(&c.node, LayoutNode::Leaf { pane_id: id } if id == pane_id) {
                        f(c);
                        return true;
                    }
                    if c.node.find_dock_child_leaf(pane_id, f) {
                        return true;
                    }
                }
                false
            }
            LayoutNode::Split { children, .. } => {
                for c in children {
                    if c.node.find_dock_child_leaf(pane_id, f) {
                        return true;
                    }
                }
                false
            }
            LayoutNode::Leaf { .. } => false,
        }
    }

    fn map_dock_child_leaf<F: FnMut(&mut DockChild)>(&mut self, pane_id: &str, f: &mut F) -> bool {
        match self {
            LayoutNode::DockRoot { children } => {
                for c in children.iter_mut() {
                    if matches!(&c.node, LayoutNode::Leaf { pane_id: id } if id == pane_id) {
                        f(c);
                        return true;
                    }
                    if c.node.map_dock_child_leaf(pane_id, f) {
                        return true;
                    }
                }
                false
            }
            LayoutNode::Split { children, .. } => {
                for c in children.iter_mut() {
                    if c.node.map_dock_child_leaf(pane_id, f) {
                        return true;
                    }
                }
                false
            }
            LayoutNode::Leaf { .. } => false,
        }
    }
}

fn child_key(node: &LayoutNode) -> String {
    match node {
        LayoutNode::Leaf { pane_id } => pane_id.clone(),
        LayoutNode::Split { id, .. } => id.clone(),
        LayoutNode::DockRoot { .. } => String::from("__dock_root__"),
    }
}

fn child_participates_split(c: &SplitChild) -> bool {
    c.visible && !c.detached && node_has_participant(&c.node)
}

fn child_participates_dock(c: &DockChild) -> bool {
    c.visible && !c.detached && node_has_participant(&c.node)
}

fn node_has_participant(node: &LayoutNode) -> bool {
    match node {
        LayoutNode::Leaf { .. } => true, // unspawned still reserves when visible+!detached
        LayoutNode::Split { children, .. } => children.iter().any(child_participates_split),
        LayoutNode::DockRoot { children } => children.iter().any(child_participates_dock),
    }
}

fn to_phys(logical: f32, scale: f32) -> i32 {
    (logical * scale).round() as i32
}

/// Synthesize a DockRoot from legacy flat (anchor, dock_order, rect) panes.
/// Absolute (None) panes are omitted — handled separately after solve.
pub fn synthesize_dock_root(
    panes: &[(String, PaneAnchor, u32, f32, f32)], // id, anchor, dock_order, width, height
) -> LayoutNode {
    let mut items: Vec<_> = panes
        .iter()
        .filter(|(_, a, _, _, _)| *a != PaneAnchor::None)
        .cloned()
        .collect();
    items.sort_by_key(|(_, _, order, _, _)| *order);

    let children = items
        .into_iter()
        .map(|(id, anchor, _, w, h)| {
            let fixed = match anchor {
                PaneAnchor::Left | PaneAnchor::Right => w.max(1.0),
                PaneAnchor::Top | PaneAnchor::Bottom => h.max(1.0),
                _ => 0.0,
            };
            DockChild {
                anchor,
                node: LayoutNode::leaf(id),
                size: LayoutLength::Fixed(fixed),
                min: 1.0,
                max: None,
                resizable: false,
                visible: true,
                detached: false,
            }
        })
        .collect();

    LayoutNode::DockRoot { children }
}

pub type SolvedRects = HashMap<String, euclid::default::Rect<i32>>;

pub const DOCK_ROOT_ID: &str = "__dock__";
pub const DOCK_INNER_ID: &str = "__dock_inner__";

#[derive(Debug, Clone)]
pub struct HitTestGutter {
    pub rect: euclid::default::Rect<i32>,
    pub split_id: String,
    pub left_id: String,
    pub right_id: String,
    pub axis: Axis,
    pub depth: u32,
    /// Set for DockRoot seams (Left/Right/Top/Bottom). `None` for Split seams.
    pub dock_edge: Option<PaneAnchor>,
    pub left_min: i32,
    pub left_max: i32,
    pub right_min: i32,
    pub right_max: i32,
    /// Last solved main-axis sizes (physical) — snapshotted at drag start.
    pub left_solved_px: i32,
    pub right_solved_px: i32,
}

#[derive(Debug, Clone)]
pub struct SolveOutput {
    pub rects: SolvedRects,
    pub gutters: Vec<HitTestGutter>,
}

#[derive(Debug, Clone)]
pub struct ActiveGutterDrag {
    pub split_id: String,
    pub left_id: String,
    pub right_id: String,
    pub axis: Axis,
    pub dock_edge: Option<PaneAnchor>,
    pub start_cursor_main: f32,
    pub start_left_px: i32,
    pub start_right_px: i32,
    pub start_left_len: f32,
    pub start_right_len: f32,
    pub left_is_fixed: bool,
    pub right_is_fixed: bool,
    pub left_min: i32,
    pub left_max: i32,
    pub right_min: i32,
    pub right_max: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragCancel {
    MissingParticipant,
}

/// Parse JS `config().layout` JSON into a LayoutNode tree.
pub fn parse_layout_json(value: &serde_json::Value) -> Result<LayoutNode, String> {
    parse_node(value)
}

fn parse_length(v: &serde_json::Value) -> LayoutLength {
    let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("weight");
    let num = v.get("value").and_then(|x| x.as_f64()).unwrap_or(1.0) as f32;
    match kind {
        "fixed" => LayoutLength::Fixed(if num.is_finite() { num.max(1.0) } else { 1.0 }),
        _ => LayoutLength::Weight(LayoutLength::clamp_weight(num)),
    }
}

fn parse_anchor(v: &serde_json::Value) -> PaneAnchor {
    match v.as_u64().unwrap_or(0) as u32 {
        1 => PaneAnchor::Fill,
        2 => PaneAnchor::Left,
        3 => PaneAnchor::Right,
        4 => PaneAnchor::Top,
        5 => PaneAnchor::Bottom,
        _ => PaneAnchor::None,
    }
}

fn parse_node(v: &serde_json::Value) -> Result<LayoutNode, String> {
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "leaf" => {
            let pane_id = v
                .get("paneId")
                .and_then(|p| p.as_str())
                .ok_or_else(|| "leaf missing paneId".to_string())?
                .to_string();
            Ok(LayoutNode::Leaf { pane_id })
        }
        "split" => {
            let id = v
                .get("id")
                .and_then(|p| p.as_str())
                .ok_or_else(|| "split missing id".to_string())?
                .to_string();
            let direction = match v.get("direction").and_then(|d| d.as_str()).unwrap_or("horizontal") {
                "vertical" => Axis::Vertical,
                _ => Axis::Horizontal,
            };
            let mut children = Vec::new();
            if let Some(arr) = v.get("children").and_then(|c| c.as_array()) {
                for c in arr {
                    children.push(parse_split_child(c)?);
                }
            }
            Ok(LayoutNode::Split {
                id,
                direction,
                children,
            })
        }
        "dockRoot" => {
            let mut children = Vec::new();
            if let Some(arr) = v.get("children").and_then(|c| c.as_array()) {
                for c in arr {
                    children.push(parse_dock_child(c)?);
                }
            }
            Ok(LayoutNode::DockRoot { children })
        }
        other => Err(format!("unknown layout node type: {other}")),
    }
}

fn parse_split_child(v: &serde_json::Value) -> Result<SplitChild, String> {
    let node = parse_node(v.get("node").ok_or("split child missing node")?)?;
    Ok(SplitChild {
        node,
        size: v.get("size").map(parse_length).unwrap_or(LayoutLength::Weight(1.0)),
        min: v.get("min").and_then(|m| m.as_f64()).unwrap_or(1.0) as f32,
        max: v.get("max").and_then(|m| m.as_f64()).map(|x| x as f32),
        resizable: v.get("resizable").and_then(|r| r.as_bool()).unwrap_or(false),
        visible: v.get("visible").and_then(|r| r.as_bool()).unwrap_or(true),
        detached: v.get("detached").and_then(|r| r.as_bool()).unwrap_or(false),
    })
}

fn parse_dock_child(v: &serde_json::Value) -> Result<DockChild, String> {
    let node = parse_node(v.get("node").ok_or("dock child missing node")?)?;
    let anchor = parse_anchor(v.get("anchor").unwrap_or(&serde_json::json!(0)));
    Ok(DockChild {
        anchor,
        node,
        size: v.get("size").map(parse_length).unwrap_or(LayoutLength::Fixed(1.0)),
        min: v.get("min").and_then(|m| m.as_f64()).unwrap_or(1.0) as f32,
        max: v.get("max").and_then(|m| m.as_f64()).map(|x| x as f32),
        resizable: v.get("resizable").and_then(|r| r.as_bool()).unwrap_or(false),
        visible: v.get("visible").and_then(|r| r.as_bool()).unwrap_or(true),
        detached: v.get("detached").and_then(|r| r.as_bool()).unwrap_or(false),
    })
}

/// Ensure a docked leaf exists in the DockRoot (for late createPane remotes).
pub fn ensure_dock_leaf(
    root: &mut LayoutNode,
    pane_id: &str,
    anchor: PaneAnchor,
    fixed: f32,
    dock_order_hint: usize,
) {
    if matches!(anchor, PaneAnchor::None) {
        return;
    }
    // Already present?
    let mut found = false;
    root.map_split_child(pane_id, &mut |_| found = true);
    if found {
        return;
    }
    root.map_dock_child_leaf(pane_id, &mut |_| found = true);
    if found {
        return;
    }
    if let LayoutNode::DockRoot { children } = root {
        let child = DockChild {
            anchor,
            node: LayoutNode::leaf(pane_id),
            size: LayoutLength::Fixed(fixed.max(1.0)),
            min: 1.0,
            max: None,
            resizable: false,
            visible: true,
            detached: false,
        };
        let idx = dock_order_hint.min(children.len());
        children.insert(idx, child);
    }
}

fn gutter_half_width(scale: f32) -> i32 {
    ((3.0 * scale).round() as i32).max(3)
}

fn seam_draggable_split(left: &SplitChild, right: &SplitChild) -> bool {
    let lf = matches!(left.size, LayoutLength::Fixed(_));
    let rf = matches!(right.size, LayoutLength::Fixed(_));
    match (lf, rf) {
        (false, false) => left.resizable || right.resizable,
        (true, false) => left.resizable,
        (false, true) => right.resizable,
        (true, true) => left.resizable && right.resizable,
    }
}

/// Horizontal seam strip. Biased into the **right** neighbor so the left pane's
/// trailing overlay scrollbar (v0.4) is not covered by the hit target.
fn strip_rect_horizontal(
    seam_x: i32,
    y: i32,
    height: i32,
    hw: i32,
) -> euclid::default::Rect<i32> {
    let w = (hw * 2).max(1);
    euclid::Rect::new(
        euclid::default::Point2D::new(seam_x - (hw / 2).max(1), y),
        euclid::default::Size2D::new(w, height.max(0)),
    )
}

/// Vertical seam strip. Biased into the **bottom** neighbor for the same reason.
fn strip_rect_vertical(
    seam_y: i32,
    x: i32,
    width: i32,
    hw: i32,
) -> euclid::default::Rect<i32> {
    let h = (hw * 2).max(1);
    euclid::Rect::new(
        euclid::default::Point2D::new(x, seam_y - (hw / 2).max(1)),
        euclid::default::Size2D::new(width.max(0), h),
    )
}

fn dock_child_min_phys(c: &DockChild, scale: f32) -> i32 {
    to_phys(c.min.max(1.0), scale).max(1)
}

/// Remaining-inner floor after carving `from_idx` onward (exclusive of carved child).
fn dock_inner_floor(children: &[DockChild], from_idx: usize, scale: f32) -> i32 {
    let mut sum = 1i32; // keep at least 1px of inward region
    for c in children.iter().skip(from_idx) {
        if !child_participates_dock(c) {
            continue;
        }
        if matches!(c.anchor, PaneAnchor::Fill | PaneAnchor::None) {
            continue;
        }
        sum = sum.saturating_add(dock_child_min_phys(c, scale));
    }
    sum
}

/// Solve the virtual tree into physical leaf rects + hit-only gutters.
pub fn solve_layout(
    root: &LayoutNode,
    window: euclid::default::Rect<i32>,
    scale_factor: f32,
) -> SolveOutput {
    let mut out = SolvedRects::new();
    let mut gutters = Vec::new();
    solve_node(root, window, scale_factor, 0, &mut out, &mut gutters);
    SolveOutput {
        rects: out,
        gutters,
    }
}

fn solve_node(
    node: &LayoutNode,
    rect: euclid::default::Rect<i32>,
    scale: f32,
    depth: u32,
    out: &mut SolvedRects,
    gutters: &mut Vec<HitTestGutter>,
) {
    match node {
        LayoutNode::Leaf { pane_id } => {
            if rect.size.width > 0 && rect.size.height > 0 {
                out.insert(pane_id.clone(), rect);
            }
        }
        LayoutNode::DockRoot { children } => {
            solve_dock(children, rect, scale, depth, out, gutters)
        }
        LayoutNode::Split {
            id,
            direction,
            children,
        } => {
            if !children.iter().any(child_participates_split) {
                return;
            }
            solve_split(id, *direction, children, rect, scale, depth, out, gutters);
        }
    }
}

fn solve_dock(
    children: &[DockChild],
    mut available: euclid::default::Rect<i32>,
    scale: f32,
    depth: u32,
    out: &mut SolvedRects,
    gutters: &mut Vec<HitTestGutter>,
) {
    let hw = gutter_half_width(scale);
    for (idx, c) in children.iter().enumerate() {
        if !child_participates_dock(c) {
            continue;
        }
        let mut child_rect = euclid::Rect::new(available.origin, euclid::default::Size2D::new(0, 0));
        let before = available;
        match c.anchor {
            PaneAnchor::Left => {
                let floor = dock_inner_floor(children, idx + 1, scale);
                let mut pw = match c.size {
                    LayoutLength::Fixed(v) => to_phys(v, scale),
                    LayoutLength::Weight(_) => to_phys(c.min.max(1.0), scale),
                };
                let min_p = to_phys(c.min.max(1.0), scale);
                pw = pw.max(min_p);
                if let Some(max) = c.max {
                    pw = pw.min(to_phys(max, scale));
                }
                let max_by_floor = (before.size.width - floor).max(0);
                pw = pw.min(max_by_floor).min(before.size.width).max(0);
                child_rect.size = euclid::default::Size2D::new(pw, before.size.height);
                available.origin.x += pw;
                available.size.width -= pw;
                if c.resizable && pw > 0 {
                    let seam = child_rect.origin.x + pw;
                    gutters.push(HitTestGutter {
                        rect: strip_rect_horizontal(seam, before.origin.y, before.size.height, hw),
                        split_id: DOCK_ROOT_ID.into(),
                        left_id: child_key(&c.node),
                        right_id: DOCK_INNER_ID.into(),
                        axis: Axis::Horizontal,
                        depth,
                        dock_edge: Some(PaneAnchor::Left),
                        left_min: min_p,
                        left_max: max_by_floor
                            .min(c.max.map(|m| to_phys(m, scale)).unwrap_or(i32::MAX))
                            .max(min_p),
                        right_min: floor,
                        right_max: i32::MAX,
                        left_solved_px: pw,
                        right_solved_px: available.size.width.max(0),
                    });
                }
            }
            PaneAnchor::Right => {
                let floor = dock_inner_floor(children, idx + 1, scale);
                let mut pw = match c.size {
                    LayoutLength::Fixed(v) => to_phys(v, scale),
                    LayoutLength::Weight(_) => to_phys(c.min.max(1.0), scale),
                };
                let min_p = to_phys(c.min.max(1.0), scale);
                pw = pw.max(min_p);
                if let Some(max) = c.max {
                    pw = pw.min(to_phys(max, scale));
                }
                let max_by_floor = (before.size.width - floor).max(0);
                pw = pw.min(max_by_floor).min(before.size.width).max(0);
                child_rect.origin.x = before.origin.x + before.size.width - pw;
                child_rect.origin.y = before.origin.y;
                child_rect.size = euclid::default::Size2D::new(pw, before.size.height);
                available.size.width -= pw;
                if c.resizable && pw > 0 {
                    let seam = child_rect.origin.x;
                    gutters.push(HitTestGutter {
                        rect: strip_rect_horizontal(seam, before.origin.y, before.size.height, hw),
                        split_id: DOCK_ROOT_ID.into(),
                        left_id: DOCK_INNER_ID.into(),
                        right_id: child_key(&c.node),
                        axis: Axis::Horizontal,
                        depth,
                        dock_edge: Some(PaneAnchor::Right),
                        left_min: floor,
                        left_max: i32::MAX,
                        right_min: min_p,
                        right_max: max_by_floor
                            .min(c.max.map(|m| to_phys(m, scale)).unwrap_or(i32::MAX))
                            .max(min_p),
                        left_solved_px: available.size.width.max(0),
                        right_solved_px: pw,
                    });
                }
            }
            PaneAnchor::Top => {
                let floor = dock_inner_floor(children, idx + 1, scale);
                let mut ph = match c.size {
                    LayoutLength::Fixed(v) => to_phys(v, scale),
                    LayoutLength::Weight(_) => to_phys(c.min.max(1.0), scale),
                };
                let min_p = to_phys(c.min.max(1.0), scale);
                ph = ph.max(min_p);
                if let Some(max) = c.max {
                    ph = ph.min(to_phys(max, scale));
                }
                let max_by_floor = (before.size.height - floor).max(0);
                ph = ph.min(max_by_floor).min(before.size.height).max(0);
                child_rect.size = euclid::default::Size2D::new(before.size.width, ph);
                available.origin.y += ph;
                available.size.height -= ph;
                if c.resizable && ph > 0 {
                    let seam = child_rect.origin.y + ph;
                    gutters.push(HitTestGutter {
                        rect: strip_rect_vertical(seam, before.origin.x, before.size.width, hw),
                        split_id: DOCK_ROOT_ID.into(),
                        left_id: child_key(&c.node),
                        right_id: DOCK_INNER_ID.into(),
                        axis: Axis::Vertical,
                        depth,
                        dock_edge: Some(PaneAnchor::Top),
                        left_min: min_p,
                        left_max: max_by_floor
                            .min(c.max.map(|m| to_phys(m, scale)).unwrap_or(i32::MAX))
                            .max(min_p),
                        right_min: floor,
                        right_max: i32::MAX,
                        left_solved_px: ph,
                        right_solved_px: available.size.height.max(0),
                    });
                }
            }
            PaneAnchor::Bottom => {
                let floor = dock_inner_floor(children, idx + 1, scale);
                let mut ph = match c.size {
                    LayoutLength::Fixed(v) => to_phys(v, scale),
                    LayoutLength::Weight(_) => to_phys(c.min.max(1.0), scale),
                };
                let min_p = to_phys(c.min.max(1.0), scale);
                ph = ph.max(min_p);
                if let Some(max) = c.max {
                    ph = ph.min(to_phys(max, scale));
                }
                let max_by_floor = (before.size.height - floor).max(0);
                ph = ph.min(max_by_floor).min(before.size.height).max(0);
                child_rect.origin.x = before.origin.x;
                child_rect.origin.y = before.origin.y + before.size.height - ph;
                child_rect.size = euclid::default::Size2D::new(before.size.width, ph);
                available.size.height -= ph;
                if c.resizable && ph > 0 {
                    let seam = child_rect.origin.y;
                    gutters.push(HitTestGutter {
                        rect: strip_rect_vertical(seam, before.origin.x, before.size.width, hw),
                        split_id: DOCK_ROOT_ID.into(),
                        left_id: DOCK_INNER_ID.into(),
                        right_id: child_key(&c.node),
                        axis: Axis::Vertical,
                        depth,
                        dock_edge: Some(PaneAnchor::Bottom),
                        left_min: floor,
                        left_max: i32::MAX,
                        right_min: min_p,
                        right_max: max_by_floor
                            .min(c.max.map(|m| to_phys(m, scale)).unwrap_or(i32::MAX))
                            .max(min_p),
                        left_solved_px: available.size.height.max(0),
                        right_solved_px: ph,
                    });
                }
            }
            PaneAnchor::Fill => {
                child_rect = available;
                available.size = euclid::default::Size2D::new(0, 0);
            }
            PaneAnchor::None => continue,
        }
        solve_node(&c.node, child_rect, scale, depth + 1, out, gutters);
    }
}

fn solve_split(
    split_id: &str,
    axis: Axis,
    children: &[SplitChild],
    parent: euclid::default::Rect<i32>,
    scale: f32,
    depth: u32,
    out: &mut SolvedRects,
    gutters: &mut Vec<HitTestGutter>,
) {
    let main_avail = match axis {
        Axis::Horizontal => parent.size.width,
        Axis::Vertical => parent.size.height,
    };
    if main_avail <= 0 {
        return;
    }

    let parts: Vec<usize> = children
        .iter()
        .enumerate()
        .filter(|(_, c)| child_participates_split(c))
        .map(|(i, _)| i)
        .collect();
    if parts.is_empty() {
        return;
    }

    let n = parts.len();
    let mut sizes = vec![0i32; n];
    let mut is_fixed = vec![false; n];
    let mut weights = vec![0.0f32; n];
    let mut mins = vec![0i32; n];
    let mut maxs = vec![i32::MAX; n];

    for (slot, &ci) in parts.iter().enumerate() {
        let c = &children[ci];
        mins[slot] = to_phys(c.min.max(0.0), scale).max(0);
        maxs[slot] = c
            .max
            .map(|m| to_phys(m, scale))
            .unwrap_or(i32::MAX)
            .max(mins[slot]);
        match c.size {
            LayoutLength::Fixed(v) => {
                is_fixed[slot] = true;
                sizes[slot] = to_phys(v, scale).max(mins[slot]).min(maxs[slot]);
            }
            LayoutLength::Weight(w) => {
                weights[slot] = LayoutLength::clamp_weight(w);
            }
        }
    }

    let fixed_min_sum: i32 = parts
        .iter()
        .enumerate()
        .filter(|(s, _)| is_fixed[*s])
        .map(|(s, _)| mins[s])
        .sum();
    if fixed_min_sum > main_avail && fixed_min_sum > 0 {
        for s in 0..n {
            if is_fixed[s] {
                sizes[s] = ((mins[s] as i64 * main_avail as i64) / fixed_min_sum as i64) as i32;
            }
        }
    }

    let mut used_fixed: i32 = 0;
    for s in 0..n {
        if is_fixed[s] {
            let rem = (main_avail - used_fixed).max(0);
            sizes[s] = sizes[s].min(rem).max(0);
            used_fixed += sizes[s];
        }
    }

    let leftover = (main_avail - used_fixed).max(0);
    let weight_slots: Vec<usize> = (0..n).filter(|s| !is_fixed[*s]).collect();

    if !weight_slots.is_empty() {
        let sum_mins: i32 = weight_slots.iter().map(|&s| mins[s]).sum();
        if sum_mins > leftover && sum_mins > 0 {
            for &s in &weight_slots {
                sizes[s] = ((mins[s] as i64 * leftover as i64) / sum_mins as i64) as i32;
            }
            let assigned: i32 = weight_slots.iter().map(|&s| sizes[s]).sum();
            if let Some(&last) = weight_slots.last() {
                sizes[last] = (sizes[last] + (leftover - assigned)).max(0);
            }
        } else {
            water_fill_weights(&weight_slots, &mut sizes, &weights, &mins, &maxs, leftover);
        }
    }

    let allocated: i32 = sizes.iter().take(n).map(|&s| s.max(0)).sum::<i32>().min(main_avail);
    let mut offset = match axis {
        Axis::Horizontal => parent.origin.x,
        Axis::Vertical => parent.origin.y,
    };
    let allocated_end = offset + allocated;
    let hw = gutter_half_width(scale);
    let mut placed: Vec<(usize, i32, i32)> = Vec::with_capacity(n); // (ci, main_size, seam_start)

    for (slot, &ci) in parts.iter().enumerate() {
        let is_last = slot + 1 == n;
        let main_size = if is_last {
            (allocated_end - offset).max(0)
        } else {
            sizes[slot].max(0)
        };
        let child_rect = match axis {
            Axis::Horizontal => euclid::Rect::new(
                euclid::default::Point2D::new(offset, parent.origin.y),
                euclid::default::Size2D::new(main_size, parent.size.height),
            ),
            Axis::Vertical => euclid::Rect::new(
                euclid::default::Point2D::new(parent.origin.x, offset),
                euclid::default::Size2D::new(parent.size.width, main_size),
            ),
        };
        placed.push((ci, main_size, offset));
        offset += main_size;
        solve_node(&children[ci].node, child_rect, scale, depth + 1, out, gutters);
    }

    for i in 0..placed.len().saturating_sub(1) {
        let (ci_l, size_l, _) = placed[i];
        let (ci_r, size_r, start_r) = placed[i + 1];
        let left = &children[ci_l];
        let right = &children[ci_r];
        if !seam_draggable_split(left, right) {
            continue;
        }
        let seam = start_r;
        let rect = match axis {
            Axis::Horizontal => {
                strip_rect_horizontal(seam, parent.origin.y, parent.size.height, hw)
            }
            Axis::Vertical => strip_rect_vertical(seam, parent.origin.x, parent.size.width, hw),
        };
        gutters.push(HitTestGutter {
            rect,
            split_id: split_id.to_string(),
            left_id: child_key(&left.node),
            right_id: child_key(&right.node),
            axis,
            depth,
            dock_edge: None,
            left_min: mins[i],
            left_max: maxs[i],
            right_min: mins[i + 1],
            right_max: maxs[i + 1],
            left_solved_px: size_l,
            right_solved_px: size_r,
        });
    }
}

fn length_value(len: LayoutLength) -> f32 {
    match len {
        LayoutLength::Fixed(v) | LayoutLength::Weight(v) => v,
    }
}

/// Map cursor Δ along the seam axis into a thickness Δ for the docked side.
/// Right/Bottom grow toward the origin, so the sign flips.
pub fn dock_signed_delta(dock_edge: PaneAnchor, cursor_delta_main: i32) -> i32 {
    match dock_edge {
        PaneAnchor::Left | PaneAnchor::Top => cursor_delta_main,
        PaneAnchor::Right | PaneAnchor::Bottom => -cursor_delta_main,
        _ => cursor_delta_main,
    }
}

/// Apply a gutter drag from the **start snapshot** + total Δ (not incremental).
/// `total_delta_px == 0` restores `start_*_len` exactly (Escape revert).
pub fn apply_gutter_drag(
    root: &mut LayoutNode,
    drag: &ActiveGutterDrag,
    total_delta_px: i32,
    scale: f32,
) -> Result<bool, DragCancel> {
    if drag.right_id != DOCK_INNER_ID && drag.left_id != DOCK_INNER_ID {
        if !root.child_participating(&drag.left_id) || !root.child_participating(&drag.right_id) {
            return Err(DragCancel::MissingParticipant);
        }
    } else {
        // Dock seam: only the dock leaf must participate.
        let dock_id = if drag.left_id == DOCK_INNER_ID {
            &drag.right_id
        } else {
            &drag.left_id
        };
        if !root.child_participating(dock_id) {
            return Err(DragCancel::MissingParticipant);
        }
    }

    // Escape / explicit restore: write snapshot lengths verbatim.
    if total_delta_px == 0 {
        let mut changed = false;
        if drag.left_id != DOCK_INNER_ID {
            changed |= if drag.left_is_fixed {
                root.set_child_fixed_size(&drag.left_id, drag.start_left_len)
            } else {
                root.set_child_weight(&drag.left_id, drag.start_left_len)
            };
        }
        if drag.right_id != DOCK_INNER_ID {
            changed |= if drag.right_is_fixed {
                root.set_child_fixed_size(&drag.right_id, drag.start_right_len)
            } else {
                root.set_child_weight(&drag.right_id, drag.start_right_len)
            };
        }
        return Ok(changed);
    }

    // Dock Fixed thickness path.
    if let Some(edge) = drag.dock_edge {
        let signed = dock_signed_delta(edge, total_delta_px);
        let dock_id = match edge {
            PaneAnchor::Left | PaneAnchor::Top => drag.left_id.as_str(),
            PaneAnchor::Right | PaneAnchor::Bottom => drag.right_id.as_str(),
            _ => return Err(DragCancel::MissingParticipant),
        };
        let (start_px, min_p, max_p) = match edge {
            PaneAnchor::Left | PaneAnchor::Top => {
                (drag.start_left_px, drag.left_min, drag.left_max)
            }
            PaneAnchor::Right | PaneAnchor::Bottom => {
                (drag.start_right_px, drag.right_min, drag.right_max)
            }
            _ => return Err(DragCancel::MissingParticipant),
        };
        let target = (start_px + signed).max(min_p).min(max_p);
        let logical = if scale > 0.0 {
            (target as f32) / scale
        } else {
            target as f32
        };
        return Ok(root.set_child_fixed_size(dock_id, logical.max(1.0)));
    }

    // Split seam: clamp the pair jointly so both stay in min/max.
    let pair = drag.start_left_px + drag.start_right_px;
    let mut left_t = drag.start_left_px + total_delta_px;
    left_t = left_t.max(drag.left_min).min(drag.left_max);
    let mut right_t = pair - left_t;
    if right_t < drag.right_min {
        right_t = drag.right_min;
        left_t = (pair - right_t).max(drag.left_min).min(drag.left_max);
        right_t = pair - left_t;
    } else if right_t > drag.right_max {
        right_t = drag.right_max;
        left_t = (pair - right_t).max(drag.left_min).min(drag.left_max);
        right_t = pair - left_t;
    }

    let mut changed = false;
    match (drag.left_is_fixed, drag.right_is_fixed) {
        (false, false) => {
            let pair_w = LayoutLength::clamp_weight(drag.start_left_len)
                + LayoutLength::clamp_weight(drag.start_right_len);
            let pair_px = (left_t + right_t).max(1) as f32;
            let left_w = LayoutLength::clamp_weight(pair_w * (left_t as f32) / pair_px);
            let right_w = LayoutLength::clamp_weight(pair_w - left_w);
            changed |= root.set_child_weight(&drag.left_id, left_w);
            changed |= root.set_child_weight(&drag.right_id, right_w);
        }
        (true, false) => {
            let logical = if scale > 0.0 {
                left_t as f32 / scale
            } else {
                left_t as f32
            };
            changed |= root.set_child_fixed_size(&drag.left_id, logical.max(1.0));
            // Weight side keeps its weight; solver absorbs remainder.
        }
        (false, true) => {
            let logical = if scale > 0.0 {
                right_t as f32 / scale
            } else {
                right_t as f32
            };
            changed |= root.set_child_fixed_size(&drag.right_id, logical.max(1.0));
        }
        (true, true) => {
            let left_log = if scale > 0.0 {
                left_t as f32 / scale
            } else {
                left_t as f32
            };
            let right_log = if scale > 0.0 {
                right_t as f32 / scale
            } else {
                right_t as f32
            };
            changed |= root.set_child_fixed_size(&drag.left_id, left_log.max(1.0));
            changed |= root.set_child_fixed_size(&drag.right_id, right_log.max(1.0));
        }
    }
    Ok(changed)
}

/// Build an [`ActiveGutterDrag`] snapshot from a hit gutter + current tree lengths.
pub fn begin_gutter_drag(
    root: &LayoutNode,
    gutter: &HitTestGutter,
    cursor_main: f32,
) -> Option<ActiveGutterDrag> {
    let (left_len, left_fixed) = if gutter.left_id == DOCK_INNER_ID {
        (0.0, true)
    } else {
        let (len, fixed) = root.child_length(&gutter.left_id)?;
        (length_value(len), fixed)
    };
    let (right_len, right_fixed) = if gutter.right_id == DOCK_INNER_ID {
        (0.0, true)
    } else {
        let (len, fixed) = root.child_length(&gutter.right_id)?;
        (length_value(len), fixed)
    };
    Some(ActiveGutterDrag {
        split_id: gutter.split_id.clone(),
        left_id: gutter.left_id.clone(),
        right_id: gutter.right_id.clone(),
        axis: gutter.axis,
        dock_edge: gutter.dock_edge,
        start_cursor_main: cursor_main,
        start_left_px: gutter.left_solved_px,
        start_right_px: gutter.right_solved_px,
        start_left_len: left_len,
        start_right_len: right_len,
        left_is_fixed: left_fixed,
        right_is_fixed: right_fixed,
        left_min: gutter.left_min,
        left_max: gutter.left_max,
        right_min: gutter.right_min,
        right_max: gutter.right_max,
    })
}

/// Pick a gutter under `point`. Deeper seams win; equal depth prefers vertical.
pub fn hit_test_gutters(
    gutters: &[HitTestGutter],
    point: euclid::default::Point2D<f32>,
) -> Option<&HitTestGutter> {
    let px = point.x as i32;
    let py = point.y as i32;
    let mut best: Option<&HitTestGutter> = None;
    for g in gutters {
        if px < g.rect.origin.x
            || py < g.rect.origin.y
            || px >= g.rect.origin.x + g.rect.size.width
            || py >= g.rect.origin.y + g.rect.size.height
        {
            continue;
        }
        best = Some(match best {
            None => g,
            Some(cur) => {
                if g.depth > cur.depth {
                    g
                } else if g.depth < cur.depth {
                    cur
                } else if g.axis == Axis::Horizontal && cur.axis != Axis::Horizontal {
                    // Prefer vertical seam (column resize) at equal depth.
                    g
                } else {
                    cur
                }
            }
        });
    }
    best
}

/// CSS-flex-style freeze: sum clamp deltas; freeze only min- OR only max-violators per pass.
fn water_fill_weights(
    slots: &[usize],
    sizes: &mut [i32],
    weights: &[f32],
    mins: &[i32],
    maxs: &[i32],
    leftover_initial: i32,
) {
    let mut frozen = vec![false; sizes.len()];
    let mut frozen_budget = 0i32;

    for _ in 0..32 {
        let leftover = (leftover_initial - frozen_budget).max(0);
        let active: Vec<usize> = slots.iter().copied().filter(|&s| !frozen[s]).collect();
        if active.is_empty() {
            break;
        }

        let tw: f32 = active.iter().map(|&s| weights[s]).sum();
        let mut assigned = 0i32;
        for (i, &s) in active.iter().enumerate() {
            if i + 1 == active.len() {
                sizes[s] = (leftover - assigned).max(0);
            } else if tw <= f32::EPSILON {
                let share = leftover / active.len() as i32;
                sizes[s] = share.max(0);
                assigned += sizes[s];
            } else {
                let portion = ((leftover as f32) * (weights[s] / tw)).round() as i32;
                sizes[s] = portion.max(0);
                assigned += sizes[s];
            }
        }

        let mut delta_sum: i64 = 0;
        let mut min_viol = Vec::new();
        let mut max_viol = Vec::new();
        for &s in &active {
            if sizes[s] < mins[s] {
                delta_sum += (mins[s] - sizes[s]) as i64;
                min_viol.push(s);
            } else if sizes[s] > maxs[s] {
                delta_sum -= (sizes[s] - maxs[s]) as i64;
                max_viol.push(s);
            }
        }

        if min_viol.is_empty() && max_viol.is_empty() {
            break;
        }

        let to_freeze: Vec<usize> = if delta_sum > 0 {
            min_viol
        } else if delta_sum < 0 {
            max_viol
        } else {
            min_viol.into_iter().chain(max_viol).collect()
        };

        if to_freeze.is_empty() {
            break;
        }

        for s in to_freeze {
            let clamped = sizes[s].max(mins[s]).min(maxs[s]);
            sizes[s] = clamped;
            frozen[s] = true;
            frozen_budget += clamped;
        }

        if slots.iter().all(|&s| frozen[s]) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_weights_no_hairline() {
        let root = LayoutNode::Split {
            id: "pools".into(),
            direction: Axis::Horizontal,
            children: vec![
                SplitChild {
                    node: LayoutNode::leaf("a"),
                    size: LayoutLength::Weight(1.0),
                    min: 1.0,
                    max: None,
                    resizable: true,
                    visible: true,
                    detached: false,
                },
                SplitChild {
                    node: LayoutNode::leaf("b"),
                    size: LayoutLength::Weight(1.0),
                    min: 1.0,
                    max: None,
                    resizable: true,
                    visible: true,
                    detached: false,
                },
            ],
        };
        let win = euclid::Rect::new(
            euclid::default::Point2D::new(0, 0),
            euclid::default::Size2D::new(981, 600),
        );
        let solved = solve_layout(&root, win, 1.0);
        let a = solved.rects.get("a").unwrap();
        let b = solved.rects.get("b").unwrap();
        assert_eq!(a.size.width + b.size.width, 981);
        assert_eq!(a.origin.x + a.size.width, b.origin.x);
        assert_eq!(solved.gutters.len(), 1);
        assert_eq!(solved.gutters[0].left_id, "a");
        assert_eq!(solved.gutters[0].right_id, "b");
    }

    #[test]
    fn unique_ids() {
        let root = LayoutNode::Split {
            id: "pools".into(),
            direction: Axis::Horizontal,
            children: vec![
                SplitChild {
                    node: LayoutNode::leaf("a"),
                    size: LayoutLength::Weight(1.0),
                    min: 1.0,
                    max: None,
                    resizable: false,
                    visible: true,
                    detached: false,
                },
                SplitChild {
                    node: LayoutNode::leaf("a"),
                    size: LayoutLength::Weight(1.0),
                    min: 1.0,
                    max: None,
                    resizable: false,
                    visible: true,
                    detached: false,
                },
            ],
        };
        assert!(root.validate_unique_ids().is_err());
    }

    fn weight_pair(a: f32, b: f32, min_a: f32, min_b: f32) -> LayoutNode {
        LayoutNode::Split {
            id: "pools".into(),
            direction: Axis::Horizontal,
            children: vec![
                SplitChild {
                    node: LayoutNode::leaf("a"),
                    size: LayoutLength::Weight(a),
                    min: min_a,
                    max: None,
                    resizable: true,
                    visible: true,
                    detached: false,
                },
                SplitChild {
                    node: LayoutNode::leaf("b"),
                    size: LayoutLength::Weight(b),
                    min: min_b,
                    max: None,
                    resizable: true,
                    visible: true,
                    detached: false,
                },
            ],
        }
    }

    #[test]
    fn gutter_drag_round_trip_within_one_px() {
        let mut root = weight_pair(0.65, 0.35, 40.0, 40.0);
        let win = euclid::Rect::new(
            euclid::default::Point2D::new(0, 0),
            euclid::default::Size2D::new(1000, 600),
        );
        let out = solve_layout(&root, win, 1.0);
        let g = out.gutters[0].clone();
        let drag = begin_gutter_drag(&root, &g, 650.0).unwrap();
        apply_gutter_drag(&mut root, &drag, 40, 1.0).unwrap();
        let after = solve_layout(&root, win, 1.0);
        let a = after.rects.get("a").unwrap().size.width;
        let b = after.rects.get("b").unwrap().size.width;
        let target_a = (drag.start_left_px + 40)
            .max(drag.left_min)
            .min(drag.left_max);
        assert!((a - target_a).abs() <= 1, "a={a} target={target_a}");
        assert_eq!(a + b, 1000);
    }

    #[test]
    fn gutter_drag_plus_minus_no_weight_drift() {
        let mut root = weight_pair(0.65, 0.35, 1.0, 1.0);
        let win = euclid::Rect::new(
            euclid::default::Point2D::new(0, 0),
            euclid::default::Size2D::new(1000, 600),
        );
        let out = solve_layout(&root, win, 1.0);
        let g = out.gutters[0].clone();
        let drag = begin_gutter_drag(&root, &g, 650.0).unwrap();
        let (w0a, _) = root.child_length("a").unwrap();
        let (w0b, _) = root.child_length("b").unwrap();
        apply_gutter_drag(&mut root, &drag, 80, 1.0).unwrap();
        apply_gutter_drag(&mut root, &drag, -80, 1.0).unwrap();
        // Exact restore via delta 0 (Escape path).
        apply_gutter_drag(&mut root, &drag, 0, 1.0).unwrap();
        let (w1a, _) = root.child_length("a").unwrap();
        let (w1b, _) = root.child_length("b").unwrap();
        assert_eq!(length_value(w0a), length_value(w1a));
        assert_eq!(length_value(w0b), length_value(w1b));
    }

    #[test]
    fn gutter_drag_hidpi_odd_width() {
        for scale in [1.25f32, 1.5f32] {
            let mut root = weight_pair(1.0, 1.0, 10.0, 10.0);
            let phys_w = 981;
            let win = euclid::Rect::new(
                euclid::default::Point2D::new(0, 0),
                euclid::default::Size2D::new(phys_w, 600),
            );
            let out = solve_layout(&root, win, scale);
            let g = out.gutters[0].clone();
            let drag = begin_gutter_drag(&root, &g, g.left_solved_px as f32).unwrap();
            apply_gutter_drag(&mut root, &drag, 25, scale).unwrap();
            let after = solve_layout(&root, win, scale);
            let a = after.rects.get("a").unwrap().size.width;
            let b = after.rects.get("b").unwrap().size.width;
            assert_eq!(a + b, phys_w, "scale={scale}");
            assert!(after.gutters[0].left_solved_px.abs_diff(a) <= 1);
        }
    }

    #[test]
    fn gutter_drag_third_sibling_frozen_at_min() {
        let mut root = LayoutNode::Split {
            id: "pools".into(),
            direction: Axis::Horizontal,
            children: vec![
                SplitChild {
                    node: LayoutNode::leaf("a"),
                    size: LayoutLength::Weight(1.0),
                    min: 1.0,
                    max: None,
                    resizable: true,
                    visible: true,
                    detached: false,
                },
                SplitChild {
                    node: LayoutNode::leaf("b"),
                    size: LayoutLength::Weight(1.0),
                    min: 1.0,
                    max: None,
                    resizable: true,
                    visible: true,
                    detached: false,
                },
                SplitChild {
                    node: LayoutNode::leaf("c"),
                    size: LayoutLength::Weight(1.0),
                    min: 200.0,
                    max: Some(200.0),
                    resizable: false,
                    visible: true,
                    detached: false,
                },
            ],
        };
        let win = euclid::Rect::new(
            euclid::default::Point2D::new(0, 0),
            euclid::default::Size2D::new(1000, 400),
        );
        let out = solve_layout(&root, win, 1.0);
        let c0 = out.rects.get("c").unwrap().size.width;
        let g = out
            .gutters
            .iter()
            .find(|g| g.left_id == "a" && g.right_id == "b")
            .unwrap()
            .clone();
        let drag = begin_gutter_drag(&root, &g, 400.0).unwrap();
        let pair_w = drag.start_left_len + drag.start_right_len;
        apply_gutter_drag(&mut root, &drag, 50, 1.0).unwrap();
        let after = solve_layout(&root, win, 1.0);
        assert_eq!(after.rects.get("c").unwrap().size.width, c0);
        let (wa, _) = root.child_length("a").unwrap();
        let (wb, _) = root.child_length("b").unwrap();
        let sum = length_value(wa) + length_value(wb);
        assert!((sum - pair_w).abs() < 1e-3, "pair sum drifted: {sum} vs {pair_w}");
    }

    #[test]
    fn gutter_drag_cancels_when_participant_hidden() {
        let mut root = weight_pair(1.0, 1.0, 1.0, 1.0);
        let win = euclid::Rect::new(
            euclid::default::Point2D::new(0, 0),
            euclid::default::Size2D::new(800, 400),
        );
        let out = solve_layout(&root, win, 1.0);
        let drag = begin_gutter_drag(&root, &out.gutters[0], 400.0).unwrap();
        root.set_leaf_visible("b", false);
        let err = apply_gutter_drag(&mut root, &drag, 10, 1.0).unwrap_err();
        assert_eq!(err, DragCancel::MissingParticipant);
    }

    #[test]
    fn dock_right_sign_and_inner_floor() {
        let mut root = LayoutNode::DockRoot {
            children: vec![
                DockChild {
                    anchor: PaneAnchor::Right,
                    node: LayoutNode::leaf("side"),
                    size: LayoutLength::Fixed(200.0),
                    min: 80.0,
                    max: Some(500.0),
                    resizable: true,
                    visible: true,
                    detached: false,
                },
                DockChild {
                    anchor: PaneAnchor::Fill,
                    node: LayoutNode::leaf("main"),
                    size: LayoutLength::Weight(1.0),
                    min: 1.0,
                    max: None,
                    resizable: false,
                    visible: true,
                    detached: false,
                },
            ],
        };
        let win = euclid::Rect::new(
            euclid::default::Point2D::new(0, 0),
            euclid::default::Size2D::new(1000, 600),
        );
        let out = solve_layout(&root, win, 1.0);
        assert_eq!(out.gutters.len(), 1);
        assert_eq!(out.gutters[0].dock_edge, Some(PaneAnchor::Right));
        let drag = begin_gutter_drag(&root, &out.gutters[0], 800.0).unwrap();
        // Positive cursor Δ (moving right) → thinner Right dock (sign flip inside apply).
        apply_gutter_drag(&mut root, &drag, 50, 1.0).unwrap();
        let after = solve_layout(&root, win, 1.0);
        let side = after.rects.get("side").unwrap().size.width;
        assert!(side < 200, "right dock should shrink on +cursor Δ, got {side}");
        // Inner floor: cannot consume the whole window.
        let drag2 = begin_gutter_drag(&root, &after.gutters[0], 800.0).unwrap();
        apply_gutter_drag(&mut root, &drag2, -10_000, 1.0).unwrap();
        let capped = solve_layout(&root, win, 1.0);
        let main = capped.rects.get("main").unwrap().size.width;
        assert!(main >= 1, "inner floor must leave main >= 1, main={main}");
        assert!(
            capped.rects.get("side").unwrap().size.width + main <= 1000
        );
    }

    #[test]
    fn set_child_weight_updates_nested_split_container() {
        let mut root = LayoutNode::Split {
            id: "outer".into(),
            direction: Axis::Horizontal,
            children: vec![
                SplitChild {
                    node: LayoutNode::leaf("editor"),
                    size: LayoutLength::Weight(1.0),
                    min: 1.0,
                    max: None,
                    resizable: true,
                    visible: true,
                    detached: false,
                },
                SplitChild {
                    node: LayoutNode::Split {
                        id: "right_panel".into(),
                        direction: Axis::Vertical,
                        children: vec![
                            SplitChild {
                                node: LayoutNode::leaf("top"),
                                size: LayoutLength::Weight(1.0),
                                min: 1.0,
                                max: None,
                                resizable: false,
                                visible: true,
                                detached: false,
                            },
                            SplitChild {
                                node: LayoutNode::leaf("bot"),
                                size: LayoutLength::Weight(1.0),
                                min: 1.0,
                                max: None,
                                resizable: false,
                                visible: true,
                                detached: false,
                            },
                        ],
                    },
                    size: LayoutLength::Weight(1.0),
                    min: 1.0,
                    max: None,
                    resizable: true,
                    visible: true,
                    detached: false,
                },
            ],
        };
        assert!(root.set_child_weight("right_panel", 2.0));
        let (len, fixed) = root.child_length("right_panel").unwrap();
        assert!(!fixed);
        assert_eq!(length_value(len), 2.0);
    }

    #[test]
    fn sandbox_like_tree_emits_trusted_and_pools_gutters() {
        let root = LayoutNode::DockRoot {
            children: vec![
                DockChild {
                    anchor: PaneAnchor::Bottom,
                    node: LayoutNode::leaf("tape"),
                    size: LayoutLength::Fixed(180.0),
                    min: 1.0,
                    max: None,
                    resizable: false,
                    visible: true,
                    detached: false,
                },
                DockChild {
                    anchor: PaneAnchor::Left,
                    node: LayoutNode::leaf("trusted"),
                    size: LayoutLength::Fixed(300.0),
                    min: 180.0,
                    max: None,
                    resizable: true,
                    visible: true,
                    detached: false,
                },
                DockChild {
                    anchor: PaneAnchor::Fill,
                    node: LayoutNode::Split {
                        id: "pools".into(),
                        direction: Axis::Horizontal,
                        children: vec![
                            SplitChild {
                                node: LayoutNode::leaf("sandboxed"),
                                size: LayoutLength::Weight(0.65),
                                min: 160.0,
                                max: None,
                                resizable: true,
                                visible: true,
                                detached: false,
                            },
                            SplitChild {
                                node: LayoutNode::leaf("aux_sandbox"),
                                size: LayoutLength::Weight(0.35),
                                min: 160.0,
                                max: None,
                                resizable: true,
                                visible: true,
                                detached: false,
                            },
                        ],
                    },
                    size: LayoutLength::Weight(1.0),
                    min: 1.0,
                    max: None,
                    resizable: false,
                    visible: true,
                    detached: false,
                },
            ],
        };
        let win = euclid::Rect::new(
            euclid::default::Point2D::new(0, 0),
            euclid::default::Size2D::new(1280, 800),
        );
        let out = solve_layout(&root, win, 1.0);
        assert!(
            out.gutters.iter().any(|g| g.left_id == "trusted"),
            "trusted|fill dock gutter missing: {:?}",
            out.gutters.iter().map(|g| (&g.left_id, &g.right_id)).collect::<Vec<_>>()
        );
        assert!(
            out.gutters.iter().any(|g| g.left_id == "sandboxed" && g.right_id == "aux_sandbox"),
            "pools A|B gutter missing: {:?}",
            out.gutters.iter().map(|g| (&g.left_id, &g.right_id)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn escape_delta_zero_restores_start_lens() {
        let mut root = weight_pair(0.7, 0.3, 1.0, 1.0);
        let win = euclid::Rect::new(
            euclid::default::Point2D::new(0, 0),
            euclid::default::Size2D::new(1000, 400),
        );
        let out = solve_layout(&root, win, 1.0);
        let drag = begin_gutter_drag(&root, &out.gutters[0], 700.0).unwrap();
        apply_gutter_drag(&mut root, &drag, 100, 1.0).unwrap();
        apply_gutter_drag(&mut root, &drag, 0, 1.0).unwrap();
        let (wa, _) = root.child_length("a").unwrap();
        let (wb, _) = root.child_length("b").unwrap();
        assert_eq!(length_value(wa), drag.start_left_len);
        assert_eq!(length_value(wb), drag.start_right_len);
    }
}
