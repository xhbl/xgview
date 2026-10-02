use crate::layout::{self, Direction, GridLayout, NavigateOutcome, PageInfo};
use crate::model::{CameraSource, RtspTransport, StreamKind};

/// Decoding decision for a single channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelMode {
    /// The channel is decoded using the given stream.
    Decode(StreamKind),
    /// The channel is parked: no socket, no decoder, no GPU texture.
    Suspend,
}

impl ChannelMode {
    pub fn is_suspended(self) -> bool {
        matches!(self, ChannelMode::Suspend)
    }

    /// Stream used for decoding, if any.
    pub fn stream(self) -> Option<StreamKind> {
        match self {
            ChannelMode::Decode(stream) => Some(stream),
            ChannelMode::Suspend => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ChannelMode::Decode(StreamKind::Main) => "main",
            ChannelMode::Decode(StreamKind::Sub) => "sub",
            ChannelMode::Suspend => "suspended",
        }
    }
}

/// Planned mode of one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelPlan {
    /// Global channel index (index inside the enabled camera list).
    pub index: usize,
    pub camera_id: String,
    pub mode: ChannelMode,
    /// URL the worker opens, `None` while the channel is parked.
    ///
    /// It is part of the plan because a worker only reopens its socket when the
    /// plan changes: filling in a sub stream from the settings must re-target
    /// the channel, otherwise it keeps streaming the URL it was started with.
    pub uri: Option<String>,
    /// Transport the worker opens the stream with. Part of the plan too, so
    /// switching a camera between TCP and UDP reconnects it.
    pub transport: RtspTransport,
}

/// The complete decoding plan derived from layout, page, zoom and focus.
#[derive(Debug, Clone, PartialEq)]
pub struct Schedule {
    /// Grid layout currently displayed.
    pub layout: GridLayout,
    /// Current page (already clamped).
    pub page: usize,
    pub page_info: PageInfo,
    /// When set, the referenced channel is displayed full screen (temporary
    /// 1x1 magnification) using its main stream.
    pub zoom: Option<usize>,
    pub focus: Option<usize>,
    /// Per channel plan, one entry per camera passed to the scheduler.
    pub plans: Vec<ChannelPlan>,
    /// Grid cells of the current page: `Some(channel index)` or `None` when the
    /// cell is empty (partial last page). Length equals the layout capacity,
    /// or 1 when zoomed.
    pub visible: Vec<Option<usize>>,
}

impl Schedule {
    pub fn rows(&self) -> usize {
        self.layout.rows()
    }

    pub fn cols(&self) -> usize {
        self.layout.cols()
    }

    /// Number of grid cells to draw.
    pub fn cells(&self) -> usize {
        self.visible.len()
    }

    pub fn is_zoomed(&self) -> bool {
        self.zoom.is_some()
    }

    /// Channels that must be decoded.
    pub fn live(&self) -> impl Iterator<Item = &ChannelPlan> {
        self.plans.iter().filter(|plan| !plan.mode.is_suspended())
    }

    pub fn mode_of(&self, index: usize) -> ChannelMode {
        self.plans
            .iter()
            .find(|plan| plan.index == index)
            .map(|plan| plan.mode)
            .unwrap_or(ChannelMode::Suspend)
    }

    /// Grid cell of a channel on the current page, if visible.
    pub fn cell_of(&self, index: usize) -> Option<usize> {
        self.visible.iter().position(|cell| *cell == Some(index))
    }

    pub fn is_visible(&self, index: usize) -> bool {
        self.cell_of(index).is_some()
    }

    /// Computes the transitions needed to move from `previous` to `self`.
    pub fn changes_from(&self, previous: &Schedule) -> Vec<ScheduleChange> {
        let mut changes = Vec::new();
        for plan in &self.plans {
            let previous_plan = previous.plans.iter().find(|item| item.index == plan.index);
            // The whole plan is compared, not only its mode: a camera whose URL
            // was edited keeps decoding the same kind of stream, and would
            // otherwise stay on the URL it was started with.
            if previous_plan == Some(plan) {
                continue;
            }
            match plan.mode {
                ChannelMode::Decode(stream) => {
                    // Only a real stream change keeps the last frame as a
                    // placeholder: a plan that changed for another reason - the
                    // transport of the camera, say - reconnects without a
                    // transition, because the stream being pulled is the same.
                    let previous_stream = previous_plan
                        .and_then(|item| item.mode.stream())
                        .filter(|previous| *previous != stream);
                    changes.push(ScheduleChange::Activate {
                        index: plan.index,
                        camera_id: plan.camera_id.clone(),
                        stream,
                        previous_stream,
                    });
                }
                ChannelMode::Suspend => changes.push(ScheduleChange::Suspend {
                    index: plan.index,
                    camera_id: plan.camera_id.clone(),
                }),
            }
        }
        // Channels that disappeared from the schedule entirely must be parked.
        for plan in &previous.plans {
            if self.plans.iter().any(|item| item.index == plan.index) {
                continue;
            }
            changes.push(ScheduleChange::Suspend {
                index: plan.index,
                camera_id: plan.camera_id.clone(),
            });
        }
        changes
    }
}

/// A single transition emitted by the scheduler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleChange {
    /// Start (or re-target) the streaming worker of a channel.
    Activate {
        index: usize,
        camera_id: String,
        stream: StreamKind,
        /// Stream that was previously decoded, when it changes the pipeline
        /// keeps the last frame until the new stream delivers a key frame.
        previous_stream: Option<StreamKind>,
    },
    /// Stop the streaming worker of a channel and release its resources.
    Suspend { index: usize, camera_id: String },
}

impl ScheduleChange {
    pub fn index(&self) -> usize {
        match self {
            ScheduleChange::Activate { index, .. } => *index,
            ScheduleChange::Suspend { index, .. } => *index,
        }
    }

    pub fn camera_id(&self) -> &str {
        match self {
            ScheduleChange::Activate { camera_id, .. } => camera_id,
            ScheduleChange::Suspend { camera_id, .. } => camera_id,
        }
    }

    /// `true` when this change switches the stream of an already live channel.
    pub fn is_stream_switch(&self) -> bool {
        matches!(
            self,
            ScheduleChange::Activate { previous_stream: Some(_), .. }
        )
    }
}

/// Owns the navigation state (layout / page / focus / zoom) and produces the
/// decoding [`Schedule`] plus the transitions between two schedules.
#[derive(Debug, Clone)]
pub struct Scheduler {
    layout: GridLayout,
    page: usize,
    focus: Option<usize>,
    zoom: Option<usize>,
    channel_count: usize,
    current: Option<Schedule>,
}

impl Scheduler {
    pub fn new(
        layout: GridLayout,
        page: usize,
        focus: Option<usize>,
        channel_count: usize,
    ) -> Self {
        let mut scheduler = Self {
            layout,
            page,
            focus,
            zoom: None,
            channel_count,
            current: None,
        };
        scheduler.normalize();
        scheduler
    }

    pub fn layout(&self) -> GridLayout {
        self.layout
    }

    pub fn page(&self) -> usize {
        self.page
    }

    pub fn focus(&self) -> Option<usize> {
        self.focus
    }

    pub fn zoom(&self) -> Option<usize> {
        self.zoom
    }

    pub fn channel_count(&self) -> usize {
        self.channel_count
    }

    pub fn page_count(&self) -> usize {
        layout::page_count(self.channel_count, self.layout.capacity())
    }

    pub fn page_info(&self) -> PageInfo {
        layout::page_info(self.page, self.layout, self.channel_count)
    }

    pub fn is_zoomed(&self) -> bool {
        self.zoom.is_some()
    }

    /// Last computed schedule, if [`Scheduler::refresh`] was called already.
    pub fn current(&self) -> Option<&Schedule> {
        self.current.as_ref()
    }

    pub fn set_channel_count(&mut self, count: usize) {
        self.channel_count = count;
        self.normalize();
    }

    pub fn set_layout(&mut self, layout: GridLayout) {
        self.layout = layout;
        self.normalize();
    }

    /// Cycles 1x1 -> 2x2 -> 3x3 -> 4x4 (or the reverse).
    pub fn cycle_layout(&mut self, forward: bool) {
        let layout = if forward { self.layout.next() } else { self.layout.prev() };
        self.set_layout(layout);
    }

    pub fn set_page(&mut self, page: usize) -> bool {
        let target = page.min(self.page_count().saturating_sub(1));
        let changed = target != self.page;
        self.page = target;
        self.normalize();
        changed
    }

    pub fn next_page(&mut self) -> bool {
        if self.page + 1 >= self.page_count() {
            return false;
        }
        self.page += 1;
        self.normalize();
        true
    }

    pub fn prev_page(&mut self) -> bool {
        if self.page == 0 {
            return false;
        }
        self.page -= 1;
        self.normalize();
        true
    }

    pub fn set_focus(&mut self, focus: Option<usize>) {
        self.focus = focus;
        self.normalize();
    }

    /// Focuses the n-th cell of the current page.
    pub fn focus_cell(&mut self, cell: usize) {
        let info = self.page_info();
        if cell >= info.len() {
            self.focus = None;
        } else {
            self.focus = Some(info.start + cell);
        }
        self.normalize();
    }

    /// Moves the focus; turns the page when the focus leaves the current page.
    pub fn navigate(&mut self, dir: Direction) -> NavigateOutcome {
        let outcome = layout::navigate(self.layout, self.page, self.focus, dir, self.channel_count);
        match outcome {
            NavigateOutcome::Moved { focus } => self.focus = Some(focus),
            NavigateOutcome::PageTurned { page, focus } => {
                self.page = page;
                self.focus = Some(focus);
            }
            NavigateOutcome::Blocked => {}
        }
        if self.zoom.is_some() {
            // A magnified viewport follows the DPAD focus.
            self.zoom = self.focus;
        }
        outcome
    }

    /// Magnifies the focused channel to full screen (1x1) using its main stream.
    pub fn zoom_in(&mut self) -> bool {
        if self.channel_count == 0 {
            return false;
        }
        let Some(focus) = self.focus.or_else(|| Some(self.page_info().start)) else {
            return false;
        };
        let focus = focus.min(self.channel_count - 1);
        self.focus = Some(focus);
        self.zoom = Some(focus);
        true
    }

    /// Leaves the magnified view.
    pub fn zoom_out(&mut self) -> bool {
        self.zoom.take().is_some()
    }

    pub fn toggle_zoom(&mut self) -> bool {
        if self.is_zoomed() {
            self.zoom_out();
            false
        } else {
            self.zoom_in();
            true
        }
    }

    /// Builds the decoding schedule for the given (enabled) cameras.
    pub fn schedule(&self, cameras: &[CameraSource]) -> Schedule {
        let total = cameras.len();
        let page_info = layout::page_info(self.page, self.layout, total);

        let zoom = match self.zoom {
            Some(index) if index < total => Some(index),
            _ => None,
        };

        let (visible, live): (Vec<Option<usize>>, Vec<usize>) = match zoom {
            Some(index) => {
                // A magnified viewport decodes the main stream of one camera,
                // every other channel is parked to save bandwidth and CPU.
                (vec![Some(index)], vec![index])
            }
            None => {
                let indices: Vec<usize> = (page_info.start..page_info.end).collect();
                let mut cells = vec![None; self.layout.capacity()];
                for (slot, index) in indices.iter().enumerate() {
                    cells[slot] = Some(*index);
                }
                (cells, indices)
            }
        };

        // Multi grid pulls the cheap sub stream; a full screen viewport — either
        // the 1x1 layout or a temporary magnification — pulls the main stream.
        let main_stream = zoom.is_some() || self.layout.is_single();
        let plans = cameras
            .iter()
            .enumerate()
            .map(|(index, camera)| {
                let mode = if live.contains(&index) {
                    ChannelMode::Decode(if main_stream { StreamKind::Main } else { StreamKind::Sub })
                } else {
                    ChannelMode::Suspend
                };
                let uri = mode.stream().map(|stream| camera.stream_uri(stream).to_string());
                ChannelPlan { index, camera_id: camera.id.clone(), mode, uri, transport: camera.transport }
            })
            .collect();

        Schedule {
            layout: self.layout,
            page: page_info.page,
            page_info,
            zoom,
            focus: self.focus.filter(|focus| *focus < total),
            plans,
            visible,
        }
    }

    /// Recomputes the schedule and returns the transitions since the previous
    /// call. The first call activates the channels that must be live right away.
    pub fn refresh(&mut self, cameras: &[CameraSource]) -> Vec<ScheduleChange> {
        let schedule = self.schedule(cameras);
        let changes = match &self.current {
            Some(previous) => schedule.changes_from(previous),
            None => schedule
                .plans
                .iter()
                .filter(|plan| !plan.mode.is_suspended())
                .map(|plan| match plan.mode {
                    ChannelMode::Decode(stream) => ScheduleChange::Activate {
                        index: plan.index,
                        camera_id: plan.camera_id.clone(),
                        stream,
                        previous_stream: None,
                    },
                    ChannelMode::Suspend => ScheduleChange::Suspend {
                        index: plan.index,
                        camera_id: plan.camera_id.clone(),
                    },
                })
                .collect(),
        };
        self.current = Some(schedule);
        changes
    }

    /// Clamps page / focus / zoom to the current channel count and layout.
    pub fn normalize(&mut self) {
        let pages = self.page_count();
        if self.page >= pages {
            self.page = pages - 1;
        }
        if self.channel_count == 0 {
            self.focus = None;
            self.zoom = None;
            return;
        }
        match self.focus {
            Some(focus) if focus >= self.channel_count => {
                self.focus = Some(self.channel_count - 1);
            }
            None => {
                self.focus = Some(self.page_info().start);
            }
            _ => {}
        }
        if let Some(zoom) = self.zoom {
            if zoom >= self.channel_count {
                self.zoom = None;
            }
        }
        // Keep the focus on the visible page so DPAD navigation stays coherent.
        let info = self.page_info();
        if let Some(focus) = self.focus {
            if !info.contains(focus) {
                self.focus = Some(info.start + focus.min(info.len().saturating_sub(1)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cameras(count: usize) -> Vec<CameraSource> {
        (0..count)
            .map(|index| CameraSource::new(format!("cam {index}"), format!("rtsp://10.0.0.{index}/live")))
            .collect()
    }

    #[test]
    fn single_page_schedules_sub_streams() {
        let cameras = cameras(4);
        let scheduler = Scheduler::new(GridLayout::G2x2, 0, Some(0), cameras.len());
        let schedule = scheduler.schedule(&cameras);
        assert_eq!(schedule.live().count(), 4);
        assert!(schedule.plans.iter().all(|plan| plan.mode == ChannelMode::Decode(StreamKind::Sub)));
    }

    #[test]
    fn channels_outside_the_page_are_suspended() {
        let cameras = cameras(32);
        let scheduler = Scheduler::new(GridLayout::G4x4, 0, Some(0), cameras.len());
        let schedule = scheduler.schedule(&cameras);
        assert_eq!(schedule.page_info.page_count, 2);
        assert_eq!(schedule.live().count(), 16);
        assert_eq!(schedule.mode_of(16), ChannelMode::Suspend);
    }

    #[test]
    fn single_grid_layout_pulls_the_main_stream() {
        let cameras = cameras(4);
        let scheduler = Scheduler::new(GridLayout::G1x1, 0, Some(0), cameras.len());
        let schedule = scheduler.schedule(&cameras);
        assert_eq!(schedule.live().count(), 1);
        assert_eq!(schedule.mode_of(0), ChannelMode::Decode(StreamKind::Main));
    }

    #[test]
    fn zoom_switches_to_main_stream_and_suspends_the_rest() {
        let cameras = cameras(9);
        let mut scheduler = Scheduler::new(GridLayout::G3x3, 0, Some(0), cameras.len());
        assert!(scheduler.zoom_in());
        let schedule = scheduler.schedule(&cameras);
        assert_eq!(schedule.live().count(), 1);
        assert_eq!(schedule.mode_of(0), ChannelMode::Decode(StreamKind::Main));
        assert_eq!(schedule.mode_of(1), ChannelMode::Suspend);
    }

    #[test]
    fn refresh_emits_stream_switch_on_zoom() {
        let cameras = cameras(4);
        let mut scheduler = Scheduler::new(GridLayout::G2x2, 0, Some(0), cameras.len());
        let first = scheduler.refresh(&cameras);
        assert_eq!(first.len(), 4);
        scheduler.zoom_in();
        let changes = scheduler.refresh(&cameras);
        assert!(changes.iter().any(ScheduleChange::is_stream_switch));
        assert!(changes.iter().any(|change| matches!(change, ScheduleChange::Suspend { .. })));
    }

    #[test]
    fn editing_a_stream_reactivates_the_channel() {
        let mut cameras = cameras(4);
        let mut scheduler = Scheduler::new(GridLayout::G2x2, 0, Some(0), cameras.len());
        assert_eq!(scheduler.refresh(&cameras).len(), 4);
        // Nothing moved: an unchanged schedule must not restart any channel.
        assert!(scheduler.refresh(&cameras).is_empty());

        // Filling in the sub stream of one camera re-targets that channel only.
        cameras[1].rtsp_sub = Some("rtsp://10.0.0.1/sub".to_string());
        let changes = scheduler.refresh(&cameras);
        assert_eq!(changes.len(), 1);
        assert!(matches!(
            &changes[0],
            ScheduleChange::Activate { index: 1, stream: StreamKind::Sub, .. }
        ));
    }

    #[test]
    fn page_turn_changes_the_live_set() {
        let cameras = cameras(8);
        let mut scheduler = Scheduler::new(GridLayout::G2x2, 0, Some(0), cameras.len());
        scheduler.refresh(&cameras);
        assert!(scheduler.next_page());
        let changes = scheduler.refresh(&cameras);
        assert_eq!(changes.len(), 8);
    }
}
