use egui::{Color32, CursorIcon, Id, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SplitTree {
    Pane(u64),
    Split {
        axis: Axis,
        ratio: f32,
        first: Box<SplitTree>,
        second: Box<SplitTree>,
    },
}

pub struct Tab {
    pub id: u64,
    pub root: SplitTree,
}

pub struct Workspace {
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    pub active_pane: u64,
    next_id: u64,
}

#[derive(Clone, Copy)]
pub struct PaneLayout {
    pub id: u64,
    pub rect: Rect,
}

impl Workspace {
    pub fn snapshot(&self) -> Vec<SplitTree> {
        self.tabs.iter().map(|tab| tab.root.clone()).collect()
    }
    pub fn restore(trees: Vec<SplitTree>, active: usize) -> Self {
        if trees.is_empty() {
            return Self::new();
        }
        let mut next = 1;
        let tabs = trees
            .into_iter()
            .map(|mut root| {
                root.remap(&mut next);
                Tab {
                    id: root.first(),
                    root,
                }
            })
            .collect::<Vec<_>>();
        let active_tab = active.min(tabs.len() - 1);
        let active_pane = tabs[active_tab].root.first();
        Self {
            tabs,
            active_tab,
            active_pane,
            next_id: next,
        }
    }
    pub fn new() -> Self {
        Self {
            tabs: vec![Tab {
                id: 1,
                root: SplitTree::Pane(1),
            }],
            active_tab: 0,
            active_pane: 1,
            next_id: 2,
        }
    }
    pub fn allocate(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
    pub fn new_tab(&mut self, id: u64) {
        self.tabs.push(Tab {
            id,
            root: SplitTree::Pane(id),
        });
        self.active_tab = self.tabs.len() - 1;
        self.active_pane = id;
    }
    pub fn activate_tab(&mut self, index: usize) {
        if let Some(tab) = self.tabs.get(index) {
            self.active_tab = index;
            self.active_pane = tab.root.first();
        }
    }
    pub fn split(&mut self, id: u64, axis: Axis) {
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.root.split(self.active_pane, id, axis);
            self.active_pane = id;
        }
    }
    pub fn close_pane(&mut self, id: u64) {
        let active_tab_id = self.tabs.get(self.active_tab).map(|tab| tab.id);
        let Some(index) = self.tabs.iter().position(|tab| tab.root.contains(id)) else {
            return;
        };
        if !self.tabs[index].root.remove(id) {
            self.tabs.remove(index);
        }
        if self.tabs.is_empty() {
            return;
        }
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| Some(tab.id) == active_tab_id)
        {
            self.active_tab = index;
        }
        if !self.tabs[self.active_tab].root.contains(self.active_pane) {
            self.active_pane = self.tabs[self.active_tab].root.first();
        }
    }
    pub fn active_ids(&self) -> Vec<u64> {
        let mut ids = Vec::new();
        if let Some(tab) = self.tabs.get(self.active_tab) {
            tab.root.ids(&mut ids);
        }
        ids
    }
    pub fn tab_ids(&self, index: usize) -> Vec<u64> {
        let mut ids = Vec::new();
        if let Some(tab) = self.tabs.get(index) {
            tab.root.ids(&mut ids);
        }
        ids
    }
    pub fn layout(&mut self, ui: &mut Ui, rect: Rect) -> Vec<PaneLayout> {
        let mut panes = Vec::new();
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.root.layout(ui, rect, Id::new(tab.id), &mut panes);
        }
        panes
    }
}

impl SplitTree {
    fn remap(&mut self, next: &mut u64) {
        match self {
            Self::Pane(id) => {
                *id = *next;
                *next += 1;
            }
            Self::Split {
                first,
                second,
                ratio,
                ..
            } => {
                *ratio = if ratio.is_finite() {
                    ratio.clamp(0.15, 0.85)
                } else {
                    0.5
                };
                first.remap(next);
                second.remap(next);
            }
        }
    }
    fn first(&self) -> u64 {
        match self {
            Self::Pane(id) => *id,
            Self::Split { first, .. } => first.first(),
        }
    }
    fn contains(&self, target: u64) -> bool {
        match self {
            Self::Pane(id) => *id == target,
            Self::Split { first, second, .. } => first.contains(target) || second.contains(target),
        }
    }
    fn ids(&self, result: &mut Vec<u64>) {
        match self {
            Self::Pane(id) => result.push(*id),
            Self::Split { first, second, .. } => {
                first.ids(result);
                second.ids(result);
            }
        }
    }
    fn split(&mut self, target: u64, next: u64, axis: Axis) {
        match self {
            Self::Pane(id) if *id == target => {
                *self = Self::Split {
                    axis,
                    ratio: 0.5,
                    first: Box::new(Self::Pane(*id)),
                    second: Box::new(Self::Pane(next)),
                }
            }
            Self::Split { first, second, .. } => {
                first.split(target, next, axis);
                second.split(target, next, axis);
            }
            _ => {}
        }
    }
    fn remove(&mut self, target: u64) -> bool {
        match self {
            Self::Pane(id) => *id != target,
            Self::Split { first, second, .. } => {
                if !first.remove(target) {
                    *self = (**second).clone();
                } else if !second.remove(target) {
                    *self = (**first).clone();
                }
                true
            }
        }
    }
    fn layout(&mut self, ui: &mut Ui, rect: Rect, id: Id, panes: &mut Vec<PaneLayout>) {
        let Self::Split {
            axis,
            ratio,
            first,
            second,
        } = self
        else {
            if let Self::Pane(id) = self {
                panes.push(PaneLayout { id: *id, rect });
            }
            return;
        };
        let horizontal = *axis == Axis::Horizontal;
        let extent = if horizontal {
            rect.width()
        } else {
            rect.height()
        };
        let divider = if horizontal {
            Rect::from_min_size(
                Pos2::new(rect.left() + extent * *ratio - 3.0, rect.top()),
                Vec2::new(6.0, rect.height()),
            )
        } else {
            Rect::from_min_size(
                Pos2::new(rect.left(), rect.top() + extent * *ratio - 3.0),
                Vec2::new(rect.width(), 6.0),
            )
        };
        let response = ui.interact(divider, id, Sense::drag());
        let fast_drag = if response.dragged() {
            None
        } else {
            same_frame_drag(ui, &response)
        };
        if response.dragged() || fast_drag.is_some() {
            if let Some(pointer) = fast_drag.or_else(|| response.interact_pointer_pos()) {
                *ratio = ((if horizontal {
                    pointer.x - rect.left()
                } else {
                    pointer.y - rect.top()
                }) / extent.max(1.0))
                .clamp(0.15, 0.85);
                if fast_drag.is_some() {
                    // The divider rectangle was calculated before this ratio update.
                    ui.ctx().request_repaint();
                }
            }
        }
        if response.hovered() || response.dragged() {
            ui.ctx().set_cursor_icon(if horizontal {
                CursorIcon::ResizeHorizontal
            } else {
                CursorIcon::ResizeVertical
            });
        }
        ui.painter().rect_stroke(
            divider.shrink(2.0),
            1.0,
            Stroke::new(
                1.0_f32,
                if response.hovered() {
                    Color32::from_gray(170)
                } else {
                    Color32::from_gray(58)
                },
            ),
            StrokeKind::Inside,
        );
        let (a, b) = if horizontal {
            (
                Rect::from_min_max(rect.min, Pos2::new(divider.left(), rect.bottom())),
                Rect::from_min_max(Pos2::new(divider.right(), rect.top()), rect.max),
            )
        } else {
            (
                Rect::from_min_max(rect.min, Pos2::new(rect.right(), divider.top())),
                Rect::from_min_max(Pos2::new(rect.left(), divider.bottom()), rect.max),
            )
        };
        first.layout(ui, a, id.with(0), panes);
        second.layout(ui, b, id.with(1), panes);
    }
}

fn same_frame_drag(ui: &Ui, response: &egui::Response) -> Option<Pos2> {
    if !response.enabled() {
        return None;
    }
    let (start, end) = ui.input(|input| {
        if !input.focused || input.pointer.any_down() {
            return None;
        }
        let mut start = None;
        let mut end = None;
        for event in &input.events {
            match event {
                egui::Event::PointerGone
                | egui::Event::WindowFocused(false)
                | egui::Event::Touch { .. }
                | egui::Event::Key {
                    key: egui::Key::Escape,
                    pressed: true,
                    ..
                } => return None,
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    ..
                } if start.is_none() && end.is_none() => start = Some(*pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    ..
                } if start.is_some() && end.is_none() => end = Some(*pos),
                egui::Event::PointerButton { .. } => return None,
                _ => {}
            }
        }
        Some((start?, end?))
    })?;
    if ui.ctx().dragged_id().is_some_and(|id| id != response.id) {
        return None;
    }
    let threshold = ui
        .ctx()
        .options(|options| options.input_options.max_click_dist);
    (response.interact_rect.contains(start)
        && ui.ctx().layer_id_at(start) == Some(response.layer_id)
        && start.distance(end) > threshold)
        .then_some(end)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fast_drag_events(start: Pos2, end: Pos2) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(start),
            egui::Event::PointerButton {
                pos: start,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerMoved(end),
            egui::Event::PointerButton {
                pos: end,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    }
    fn frame(
        context: &egui::Context,
        workspace: &mut Workspace,
        events: Vec<egui::Event>,
        overlay: bool,
    ) {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
            events,
            ..Default::default()
        };
        let _ = context.run(input, |ctx| {
            if overlay {
                egui::Area::new(Id::new("overlay"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(Pos2::new(360.0, 100.0))
                    .show(ctx, |ui| {
                        ui.allocate_space(Vec2::new(80.0, 150.0));
                    });
            }
            egui::CentralPanel::default()
                .frame(egui::Frame::new().inner_margin(egui::Margin::ZERO))
                .show(ctx, |ui| {
                    workspace.layout(ui, ui.available_rect_before_wrap());
                });
        });
    }
    fn root_ratio(workspace: &Workspace) -> f32 {
        match &workspace.tabs[0].root {
            SplitTree::Split { ratio, .. } => *ratio,
            _ => panic!("expected split"),
        }
    }
    #[test]
    fn same_frame_primary_drag_resizes_divider() {
        for (axis, start, end) in [
            (
                Axis::Horizontal,
                Pos2::new(400.0, 150.0),
                Pos2::new(300.0, 150.0),
            ),
            (
                Axis::Vertical,
                Pos2::new(150.0, 300.0),
                Pos2::new(150.0, 225.0),
            ),
        ] {
            let context = egui::Context::default();
            let mut workspace = Workspace::new();
            workspace.split(2, axis);
            frame(&context, &mut workspace, Vec::new(), false);
            frame(
                &context,
                &mut workspace,
                fast_drag_events(start, end),
                false,
            );
            assert!((root_ratio(&workspace) - 0.375).abs() < 0.001);
        }
    }
    #[test]
    fn same_frame_drag_cannot_resize_behind_an_overlay() {
        let context = egui::Context::default();
        let mut workspace = Workspace::new();
        workspace.split(2, Axis::Horizontal);
        frame(&context, &mut workspace, Vec::new(), true);
        frame(
            &context,
            &mut workspace,
            fast_drag_events(Pos2::new(400.0, 150.0), Pos2::new(300.0, 150.0)),
            true,
        );
        assert_eq!(root_ratio(&workspace), 0.5);
    }
    #[test]
    fn stationary_cancelled_and_multiple_button_sequences_preserve_ratio() {
        let start = Pos2::new(400.0, 150.0);
        let mut cancelled = fast_drag_events(start, Pos2::new(300.0, 150.0));
        cancelled.insert(2, egui::Event::PointerGone);
        let mut multiple = fast_drag_events(start, Pos2::new(300.0, 150.0));
        multiple.insert(
            2,
            egui::Event::PointerButton {
                pos: start,
                button: egui::PointerButton::Secondary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        );
        for events in [fast_drag_events(start, start), cancelled, multiple] {
            let context = egui::Context::default();
            let mut workspace = Workspace::new();
            workspace.split(2, Axis::Horizontal);
            frame(&context, &mut workspace, Vec::new(), false);
            frame(&context, &mut workspace, events, false);
            assert_eq!(root_ratio(&workspace), 0.5);
        }
    }
    #[test]
    fn closing_splits_preserves_siblings_and_other_tabs() {
        let mut workspace = Workspace::new();
        workspace.split(2, Axis::Horizontal);
        workspace.split(3, Axis::Vertical);
        workspace.close_pane(2);
        assert_eq!(workspace.active_ids(), vec![1, 3]);
        workspace.new_tab(4);
        workspace.close_pane(4);
        assert_eq!(workspace.active_ids(), vec![1, 3]);
        workspace.close_pane(1);
        workspace.close_pane(3);
        assert!(workspace.tabs.is_empty());
    }
    #[test]
    fn closing_background_tab_keeps_selected_tab() {
        let mut workspace = Workspace::new();
        workspace.new_tab(2);
        workspace.new_tab(3);
        workspace.close_pane(1);
        assert_eq!(workspace.active_pane, 3);
        assert_eq!(workspace.tabs[workspace.active_tab].id, 3);
    }
    #[test]
    fn restoring_layout_allocates_fresh_unique_panes() {
        let tree = SplitTree::Split {
            axis: Axis::Horizontal,
            ratio: 0.6,
            first: Box::new(SplitTree::Pane(90)),
            second: Box::new(SplitTree::Pane(90)),
        };
        let workspace = Workspace::restore(vec![tree, SplitTree::Pane(200)], 1);
        assert_eq!(workspace.tab_ids(0), vec![1, 2]);
        assert_eq!(workspace.active_ids(), vec![3]);
        assert_eq!(workspace.active_pane, 3);
    }
}

// FVPROV1 16525 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMzYyMjgyN2VmZjE5Y2M4YzBhODY1MGRhY2FiNmQwZDA2MTgxODJjN2MxZDA1NDUwZDc0YjJlZDYxY2JkMGRkNCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiYmNlZTZjOGM2NTVjYTU4NDU3Y2RiYWQ4OGQxN2Y3MGZkMDlkYjYzMDkxOGY5MTczYzcxYmJkZmExMmEwNzcxNTYxNWVlYjA1MDU3ZTY5NDMyYjdmYTRjMTY2ZGI0ZTlmNDhhZDg3OGE5YzFmZGZhMDMzZGIwMzM1NDY1YzFhMGEifV19XQ== FVPROV1-END
