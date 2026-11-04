use std::collections::HashMap;

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
