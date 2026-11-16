//! Transactional display topology, independent of capture/encoding for deterministic tests.
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::capture::DisplayId;
use crate::encode::VideoCodec;

pub const DISPLAYS: [DisplayId; 2] = [DisplayId::Internal, DisplayId::External];
const TRANSITION_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputSettings {
    pub monitor_name: [u8; 13],
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: VideoCodec,
    pub bitrate_bps: u32,
}

#[derive(Debug, Clone)]
pub struct TopologyRequest {
    pub revision: u32,
    pub outputs: [Option<OutputSettings>; 2],
}

#[derive(Debug, Clone, Copy)]
pub struct PresentationAck {
    pub revision: u32,
    pub display: DisplayId,
    pub generation: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DisplayPhase {
    Streaming = 0,
    Committed = 1,
    Failed = 2,
    Removed = 3,
}

pub struct DisplayStatus {
    pub revision: u32,
    pub display: DisplayId,
    pub phase: DisplayPhase,
    pub active_mask: u8,
    pub generation: u32,
    pub settings: Option<OutputSettings>,
    pub detail: String,
}

impl DisplayStatus {
    pub fn encode(&self) -> Vec<u8> {
        let mut msg = vec![crate::protocol::daemon::DISPLAY_STATUS];
        msg.extend_from_slice(&self.revision.to_be_bytes());
        msg.extend_from_slice(&[self.display as u8, self.phase as u8, self.active_mask]);
        msg.extend_from_slice(&self.generation.to_be_bytes());
        msg.extend_from_slice(&(self.settings.map_or(0, |s| s.width) as u16).to_be_bytes());
        msg.extend_from_slice(&(self.settings.map_or(0, |s| s.height) as u16).to_be_bytes());
        // Preserve valid UTF-8 while bounding outbound error messages.
        msg.extend(self.detail.chars().take(256).collect::<String>().bytes());
        msg
    }
}

pub trait Pipeline {
    fn settings(&self) -> OutputSettings;
    fn generation(&self) -> u32;
    fn ready(&self) -> bool;
    fn failure(&self) -> Option<String>;
    /// Restore a retained output with a fresh namespace after abandoning a newer candidate.
    fn resume(&mut self);
    fn signal_stop(&self);
    fn join(&mut self);
}

struct Transition {
    request: TopologyRequest,
    deadline: Instant,
    announced: [bool; 2],
    presented: [bool; 2],
    /// Outputs to plug only after the internal candidate is up (ordered replug).
    deferred: [Option<OutputSettings>; 2],
    /// Topology unplugged for an ordered replug; replugged if the transition fails.
    rollback: Option<[Option<OutputSettings>; 2]>,
}

/// Plug order decides which EVDI card each monitor gets (lowest free card first), and GNOME
/// numbers monitors by card. Whenever the result has both monitors and the internal one must
/// (re)start, unplug everything and plug internal first, so it is always monitor 1.
fn needs_ordered_replug(
    active: [Option<OutputSettings>; 2],
    outputs: [Option<OutputSettings>; 2],
) -> bool {
    outputs[0].is_some() && outputs[1].is_some() && active[0] != outputs[0]
}

pub struct DisplayManager<W: Pipeline> {
    active: [Option<W>; 2],
    candidates: [Option<W>; 2],
    transition: Option<Transition>,
    last_revision: u32,
    presented_generations: [u32; 2],
}

impl<W: Pipeline> DisplayManager<W> {
    pub fn new() -> Self {
        Self {
            active: [None, None],
            candidates: [None, None],
            transition: None,
            last_revision: 0,
            presented_generations: [0; 2],
        }
    }

    fn active_mask(&self) -> u8 {
        self.active.iter().enumerate().fold(0, |mask, (i, w)| {
            mask | if w.is_some() { 1 << i } else { 0 }
        })
    }

    fn status(
        &self,
        revision: u32,
        display: DisplayId,
        phase: DisplayPhase,
        detail: String,
    ) -> DisplayStatus {
        let i = display as usize;
        let worker = if phase == DisplayPhase::Streaming {
            self.candidates[i].as_ref().or(self.active[i].as_ref())
        } else {
            self.active[i].as_ref()
        };
        DisplayStatus {
            revision,
            display,
            phase,
            active_mask: self.active_mask(),
            generation: worker.map_or(0, |w| w.generation()),
            settings: worker.map(|w| w.settings()),
            detail,
        }
    }

    fn cancel_candidates(&mut self, restore: bool) {
        for worker in self.candidates.iter().flatten() {
            worker.signal_stop();
        }
        for i in 0..2 {
            if let Some(mut worker) = self.candidates[i].take() {
                worker.join();
                if restore {
                    if let Some(old) = &mut self.active[i] {
                        old.resume();
                    }
                }
            }
        }
        self.transition = None;
    }

    fn failed_request(
        &self,
        revision: u32,
        display: DisplayId,
        detail: String,
    ) -> Vec<DisplayStatus> {
        DISPLAYS
            .into_iter()
            .map(|id| {
                self.status(
                    revision,
                    id,
                    DisplayPhase::Failed,
                    format!("{display:?} display: {detail}"),
                )
            })
            .collect()
    }

    fn active_settings(&self) -> [Option<OutputSettings>; 2] {
        std::array::from_fn(|i| {
            self.active[i]
                .as_ref()
                .filter(|w| w.failure().is_none())
                .map(|w| w.settings())
        })
    }

    /// Abandon a failed transition. If it had unplugged monitors for an ordered replug, plug the
    /// previous topology back (also in order) and report that as the surviving active mask.
    fn fail_transition(
        &mut self,
        revision: u32,
        display: DisplayId,
        detail: String,
        now: Instant,
        spawn: &mut impl FnMut(DisplayId, OutputSettings) -> Result<W>,
    ) -> Vec<DisplayStatus> {
        let rollback = self.transition.as_mut().and_then(|t| t.rollback.take());
        self.cancel_candidates(true);
        let mut statuses = self.failed_request(revision, display, detail);
        if let Some(outputs) = rollback.filter(|o| o.iter().any(Option::is_some)) {
            let restored = TopologyRequest { revision, outputs };
            let mask = outputs
                .iter()
                .enumerate()
                .fold(0, |m, (i, o)| m | if o.is_some() { 1 << i } else { 0 });
            if self.begin(restored, now, spawn, false).is_empty() {
                for status in &mut statuses {
                    status.active_mask = mask;
                }
            }
        }
        statuses
    }

    pub fn request(
        &mut self,
        request: TopologyRequest,
        now: Instant,
        spawn: &mut impl FnMut(DisplayId, OutputSettings) -> Result<W>,
    ) -> Vec<DisplayStatus> {
        if request.revision == 0 || request.revision <= self.last_revision {
            return vec![];
        }
        self.last_revision = request.revision;
        self.cancel_candidates(true);
        self.begin(request, now, spawn, true)
    }

    fn begin(
        &mut self,
        request: TopologyRequest,
        now: Instant,
        spawn: &mut impl FnMut(DisplayId, OutputSettings) -> Result<W>,
        allow_rollback: bool,
    ) -> Vec<DisplayStatus> {
        let mut presented = [false; 2];
        let mut deferred = [None; 2];
        let mut rollback = None;
        if needs_ordered_replug(self.active_settings(), request.outputs) {
            // Unplug every monitor first (signal all, then join), so the internal one is plugged
            // into the lowest free card and the external one after it.
            if allow_rollback {
                rollback = Some(self.active_settings());
            }
            for worker in self.active.iter().flatten() {
                worker.signal_stop();
            }
            for slot in &mut self.active {
                if let Some(mut worker) = slot.take() {
                    worker.join();
                }
            }
            self.presented_generations = [0; 2];
            deferred[1] = request.outputs[1];
        }
        for display in DISPLAYS {
            let i = display as usize;
            if deferred[i].is_some() {
                continue;
            }
            if let Some(settings) = request.outputs[i] {
                if self.active[i]
                    .as_ref()
                    .is_some_and(|w| w.settings() == settings && w.failure().is_none())
                {
                    presented[i] = self.active[i]
                        .as_ref()
                        .is_some_and(|w| self.presented_generations[i] == w.generation());
                } else {
                    match spawn(display, settings) {
                        Ok(worker) => self.candidates[i] = Some(worker),
                        Err(error) => {
                            let revision = request.revision;
                            self.transition = Some(Transition {
                                request,
                                deadline: now + TRANSITION_TIMEOUT,
                                announced: [false; 2],
                                presented,
                                deferred,
                                rollback,
                            });
                            return self.fail_transition(
                                revision,
                                display,
                                format!("{error:#}"),
                                now,
                                spawn,
                            );
                        }
                    }
                }
            }
        }
        self.transition = Some(Transition {
            request,
            deadline: now + TRANSITION_TIMEOUT,
            announced: [false; 2],
            presented,
            deferred,
            rollback,
        });
        vec![]
    }

    pub fn acknowledge(&mut self, ack: PresentationAck) {
        let i = ack.display as usize;
        if self.transition.is_none() {
            if ack.revision == self.last_revision
                && self.active[i]
                    .as_ref()
                    .is_some_and(|w| w.ready() && w.generation() == ack.generation)
            {
                self.presented_generations[i] = ack.generation;
            }
            return;
        }
        let transition = self.transition.as_mut().unwrap();
        let worker = self.candidates[i].as_ref().or(self.active[i].as_ref());
        if transition.request.revision == ack.revision
            && transition.request.outputs[i].is_some()
            && worker.is_some_and(|w| w.ready() && w.generation() == ack.generation)
        {
            transition.presented[i] = true;
        }
    }

    pub fn tick(
        &mut self,
        now: Instant,
        spawn: &mut impl FnMut(DisplayId, OutputSettings) -> Result<W>,
    ) -> Vec<DisplayStatus> {
        let mut events = Vec::new();
        if let Some(transition) = &self.transition {
            let revision = transition.request.revision;
            for display in DISPLAYS {
                let i = display as usize;
                if transition.request.outputs[i].is_none() {
                    continue;
                }
                let failure = self.candidates[i]
                    .as_ref()
                    .or(self.active[i].as_ref())
                    .and_then(|w| w.failure());
                if let Some(error) = failure {
                    return self.fail_transition(revision, display, error, now, spawn);
                }
            }
            if now >= transition.deadline {
                let display = DISPLAYS
                    .into_iter()
                    .find(|id| {
                        !transition.presented[*id as usize]
                            && transition.request.outputs[*id as usize].is_some()
                    })
                    .unwrap_or(DisplayId::Internal);
                return self.fail_transition(
                    revision,
                    display,
                    "Timed out waiting for the replacement display to present video".into(),
                    now,
                    spawn,
                );
            }
            // Ordered replug: plug the external monitor only once the internal one is streaming,
            // which guarantees it already holds its EVDI card.
            let internal_up = self.candidates[0].as_ref().is_some_and(|w| w.ready());
            if internal_up {
                if let Some(settings) = self.transition.as_mut().unwrap().deferred[1].take() {
                    match spawn(DisplayId::External, settings) {
                        Ok(worker) => self.candidates[1] = Some(worker),
                        Err(error) => {
                            return self.fail_transition(
                                revision,
                                DisplayId::External,
                                format!("{error:#}"),
                                now,
                                spawn,
                            );
                        }
                    }
                }
            }
        }
        if self.transition.is_none() {
            // A dead committed worker must not silently remain marked as a working output.
            for display in DISPLAYS {
                let i = display as usize;
                if let Some(error) = self.active[i].as_ref().and_then(|w| w.failure()) {
                    if let Some(mut worker) = self.active[i].take() {
                        worker.signal_stop();
                        worker.join();
                    }
                    events.push(self.status(
                        self.last_revision,
                        display,
                        DisplayPhase::Failed,
                        error,
                    ));
                }
            }
            return events;
        }
        let mut all_presented = true;
        for display in DISPLAYS {
            let i = display as usize;
            let transition = self.transition.as_mut().unwrap();
            if transition.request.outputs[i].is_none() {
                continue;
            }
            let ready = self.candidates[i]
                .as_ref()
                .or(self.active[i].as_ref())
                .is_some_and(|w| w.ready());
            all_presented &= ready && transition.presented[i];
            if ready && !transition.announced[i] {
                transition.announced[i] = true;
                let revision = transition.request.revision;
                events.push(self.status(revision, display, DisplayPhase::Streaming, String::new()));
            }
        }
        if all_presented {
            let transition = self.transition.take().unwrap();
            // Signal every retiring worker before joining any of them.
            for i in 0..2 {
                if self.candidates[i].is_some() || transition.request.outputs[i].is_none() {
                    if let Some(old) = &self.active[i] {
                        old.signal_stop();
                    }
                }
            }
            for i in 0..2 {
                if self.candidates[i].is_some() || transition.request.outputs[i].is_none() {
                    if let Some(mut old) = self.active[i].take() {
                        old.join();
                    }
                    self.active[i] = self.candidates[i].take();
                }
                self.presented_generations[i] =
                    self.active[i].as_ref().map_or(0, |w| w.generation());
            }
            for display in DISPLAYS {
                let phase = if self.active[display as usize].is_some() {
                    DisplayPhase::Committed
                } else {
                    DisplayPhase::Removed
                };
                events.push(self.status(
                    transition.request.revision,
                    display,
                    phase,
                    String::new(),
                ));
            }
        }
        events
    }
}

impl<W: Pipeline> Drop for DisplayManager<W> {
    fn drop(&mut self) {
        for worker in self.active.iter().chain(self.candidates.iter()).flatten() {
            worker.signal_stop();
        }
        for worker in self
            .active
            .iter_mut()
            .chain(self.candidates.iter_mut())
            .flatten()
        {
            worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    struct State {
        display: DisplayId,
        generation: Cell<u32>,
        ready: Cell<bool>,
        stopped: Cell<bool>,
        joined: Cell<bool>,
        error: RefCell<Option<String>>,
    }
    struct FakePipeline {
        state: Rc<State>,
        settings: OutputSettings,
        counter: Rc<Cell<u32>>,
    }
    impl Pipeline for FakePipeline {
        fn settings(&self) -> OutputSettings {
            self.settings
        }
        fn generation(&self) -> u32 {
            self.state.generation.get()
        }
        fn ready(&self) -> bool {
            self.state.ready.get()
        }
        fn failure(&self) -> Option<String> {
            self.state.error.borrow().clone()
        }
        fn resume(&mut self) {
            self.counter.set(self.counter.get() + 1);
            self.state.generation.set(self.counter.get());
            self.state.ready.set(false);
        }
        fn signal_stop(&self) {
            self.state.stopped.set(true);
        }
        fn join(&mut self) {
            assert!(self.state.stopped.get());
            self.state.joined.set(true);
        }
    }
    #[derive(Default)]
    struct Factory {
        counter: Rc<Cell<u32>>,
        states: Vec<Rc<State>>,
    }
    impl Factory {
        fn spawn(&mut self, display: DisplayId, settings: OutputSettings) -> Result<FakePipeline> {
            self.counter.set(self.counter.get() + 1);
            let state = Rc::new(State {
                display,
                generation: Cell::new(self.counter.get()),
                ready: Cell::new(false),
                stopped: Cell::new(false),
                joined: Cell::new(false),
                error: RefCell::new(None),
            });
            self.states.push(Rc::clone(&state));
            Ok(FakePipeline {
                state,
                settings,
                counter: Rc::clone(&self.counter),
            })
        }
        fn latest(&self, display: DisplayId) -> Rc<State> {
            Rc::clone(
                self.states
                    .iter()
                    .rev()
                    .find(|s| s.display == display)
                    .unwrap(),
            )
        }
    }
    fn request(revision: u32, mask: u8) -> TopologyRequest {
        let settings = OutputSettings {
            monitor_name: *b"Screx        ",
            width: 1920,
            height: 1080,
            fps: 60,
            codec: VideoCodec::H264,
            bitrate_bps: 20_000_000,
        };
        TopologyRequest {
            revision,
            outputs: [
                (mask & 1 != 0).then_some(settings),
                (mask & 2 != 0).then_some(settings),
            ],
        }
    }
    fn ack(revision: u32, state: &State) -> PresentationAck {
        PresentationAck {
            revision,
            display: state.display,
            generation: state.generation.get(),
        }
    }
    fn bootstrap() -> (DisplayManager<FakePipeline>, Factory, Instant) {
        let now = Instant::now();
        let mut manager = DisplayManager::new();
        let mut factory = Factory::default();
        manager.request(request(1, 1), now, &mut |id, settings| {
            factory.spawn(id, settings)
        });
        let internal = factory.latest(DisplayId::Internal);
        internal.ready.set(true);
        manager.acknowledge(ack(1, &internal));
        assert!(manager
            .tick(now, &mut |id, s| factory.spawn(id, s))
            .iter()
            .any(|s| s.phase == DisplayPhase::Committed));
        (manager, factory, now)
    }

    #[test]
    fn adding_external_does_not_restart_internal_and_needs_presentation_ack() {
        let (mut manager, mut factory, now) = bootstrap();
        let internal = factory.latest(DisplayId::Internal);
        let original_generation = internal.generation.get();
        manager.request(request(2, 3), now, &mut |id, s| factory.spawn(id, s));
        let external = factory.latest(DisplayId::External);
        manager.acknowledge(ack(2, &external)); // Too early: no keyframe has been sent.
        external.ready.set(true);
        manager.tick(now, &mut |id, s| factory.spawn(id, s));
        assert_eq!(manager.active_mask(), 1);
        manager.acknowledge(PresentationAck {
            generation: 999,
            ..ack(2, &external)
        });
        manager.tick(now, &mut |id, s| factory.spawn(id, s));
        assert_eq!(manager.active_mask(), 1);
        manager.acknowledge(ack(2, &external));
        manager.tick(now, &mut |id, s| factory.spawn(id, s));
        assert_eq!(manager.active_mask(), 3);
        assert_eq!(factory.states.len(), 2);
        assert_eq!(internal.generation.get(), original_generation);
        assert!(!internal.stopped.get());
    }

    #[test]
    fn external_only_keeps_internal_until_external_presents_then_unplug_restores_internal() {
        let (mut manager, mut factory, now) = bootstrap();
        let old_internal = factory.latest(DisplayId::Internal);
        manager.request(request(2, 2), now, &mut |id, s| factory.spawn(id, s));
        let external = factory.latest(DisplayId::External);
        external.ready.set(true);
        manager.tick(now, &mut |id, s| factory.spawn(id, s));
        assert!(!old_internal.stopped.get());
        manager.acknowledge(ack(2, &external));
        manager.tick(now, &mut |id, s| factory.spawn(id, s));
        assert!(old_internal.stopped.get() && old_internal.joined.get());
        assert_eq!(manager.active_mask(), 2);
        manager.request(request(3, 1), now, &mut |id, s| factory.spawn(id, s));
        let new_internal = factory.latest(DisplayId::Internal);
        assert!(!external.stopped.get());
        new_internal.ready.set(true);
        manager.acknowledge(ack(3, &new_internal));
        manager.tick(now, &mut |id, s| factory.spawn(id, s));
        assert_eq!(manager.active_mask(), 1);
        assert!(external.stopped.get() && external.joined.get());
        assert_ne!(new_internal.generation.get(), old_internal.generation.get());
    }

    #[test]
    fn timeout_or_failed_candidate_preserves_the_previous_topology() {
        for fail in [false, true] {
            let (mut manager, mut factory, now) = bootstrap();
            let internal = factory.latest(DisplayId::Internal);
            manager.request(request(2, 2), now, &mut |id, s| factory.spawn(id, s));
            let external = factory.latest(DisplayId::External);
            if fail {
                *external.error.borrow_mut() = Some("Encoder unavailable".into());
            }
            let statuses = manager.tick(
                now + if fail {
                    Duration::ZERO
                } else {
                    TRANSITION_TIMEOUT
                },
                &mut |id, s| factory.spawn(id, s),
            );
            assert!(statuses.iter().any(|s| s.phase == DisplayPhase::Failed));
            assert_eq!(manager.active_mask(), 1);
            assert!(!internal.stopped.get());
            assert!(external.stopped.get() && external.joined.get());
        }
    }

    #[test]
    fn superseded_requests_ignore_stale_acks_and_stop_abandoned_candidates() {
        let (mut manager, mut factory, now) = bootstrap();
        manager.request(request(2, 2), now, &mut |id, s| factory.spawn(id, s));
        let abandoned = factory.latest(DisplayId::External);
        manager.request(request(3, 1), now, &mut |id, s| factory.spawn(id, s));
        manager.acknowledge(ack(2, &abandoned));
        manager.tick(now, &mut |id, s| factory.spawn(id, s));
        assert!(abandoned.stopped.get() && abandoned.joined.get());
        assert_eq!(manager.active_mask(), 1);
        manager.request(request(2, 2), now, &mut |_, _| {
            panic!("stale revision must not spawn")
        });
        assert_eq!(manager.active_mask(), 1);
    }

    #[test]
    fn resolution_rollback_reissues_retained_stream_with_a_new_generation() {
        let (mut manager, mut factory, now) = bootstrap();
        let old = factory.latest(DisplayId::Internal);
        let original_generation = old.generation.get();
        let mut next = request(2, 1);
        next.outputs[0].as_mut().unwrap().width = 2560;
        manager.request(next, now, &mut |id, s| factory.spawn(id, s));
        let replacement = factory.latest(DisplayId::Internal);
        let abandoned_generation = replacement.generation.get();
        replacement.ready.set(true);
        manager.tick(now + TRANSITION_TIMEOUT, &mut |id, s| factory.spawn(id, s));
        assert!(!old.stopped.get());
        assert!(
            old.generation.get() > abandoned_generation
                && old.generation.get() > original_generation
        );
        assert!(replacement.stopped.get());
        // A subsequent request must not mistake the old epoch's ACK for the restored epoch.
        manager.request(request(3, 1), now, &mut |id, s| factory.spawn(id, s));
        old.ready.set(true);
        assert!(!manager
            .tick(now, &mut |id, s| factory.spawn(id, s))
            .iter()
            .any(|s| s.phase == DisplayPhase::Committed));
        manager.acknowledge(ack(3, &old));
        assert!(manager
            .tick(now, &mut |id, s| factory.spawn(id, s))
            .iter()
            .any(|s| s.phase == DisplayPhase::Committed));
    }

    fn external_only(
        manager: &mut DisplayManager<FakePipeline>,
        factory: &mut Factory,
        now: Instant,
    ) -> Rc<State> {
        manager.request(request(2, 2), now, &mut |id, s| factory.spawn(id, s));
        let external = factory.latest(DisplayId::External);
        external.ready.set(true);
        manager.acknowledge(ack(2, &external));
        manager.tick(now, &mut |id, s| factory.spawn(id, s));
        assert_eq!(manager.active_mask(), 2);
        external
    }

    #[test]
    fn external_only_to_both_unplugs_all_then_plugs_internal_before_external() {
        let (mut manager, mut factory, now) = bootstrap();
        let old_external = external_only(&mut manager, &mut factory, now);
        let spawned_before = factory.states.len();
        manager.request(request(3, 3), now, &mut |id, s| factory.spawn(id, s));
        assert!(
            old_external.stopped.get() && old_external.joined.get(),
            "external unplugged first"
        );
        assert_eq!(manager.active_mask(), 0);
        assert_eq!(
            factory.states.len(),
            spawned_before + 1,
            "only internal is plugged at first"
        );
        let internal = factory.latest(DisplayId::Internal);
        manager.tick(now, &mut |id, s| factory.spawn(id, s));
        assert_eq!(
            factory.states.len(),
            spawned_before + 1,
            "external waits for internal"
        );
        internal.ready.set(true);
        manager.tick(now, &mut |id, s| factory.spawn(id, s));
        let external = factory.latest(DisplayId::External);
        assert!(!Rc::ptr_eq(&external, &old_external));
        let order: Vec<_> = factory.states[spawned_before..]
            .iter()
            .map(|s| s.display)
            .collect();
        assert_eq!(order, [DisplayId::Internal, DisplayId::External]);
        external.ready.set(true);
        manager.acknowledge(ack(3, &internal));
        manager.acknowledge(ack(3, &external));
        let statuses = manager.tick(now, &mut |id, s| factory.spawn(id, s));
        assert!(statuses.iter().any(|s| s.phase == DisplayPhase::Committed));
        assert_eq!(manager.active_mask(), 3);
    }

    #[test]
    fn failed_ordered_replug_restores_the_previous_monitors() {
        let (mut manager, mut factory, now) = bootstrap();
        let old_external = external_only(&mut manager, &mut factory, now);
        manager.request(request(3, 3), now, &mut |id, s| factory.spawn(id, s));
        let internal = factory.latest(DisplayId::Internal);
        *internal.error.borrow_mut() = Some("Encoder unavailable".into());
        let statuses = manager.tick(now, &mut |id, s| factory.spawn(id, s));
        assert!(statuses.iter().all(|s| s.phase == DisplayPhase::Failed));
        assert!(
            statuses.iter().all(|s| s.active_mask == 2),
            "client keeps external-only"
        );
        assert!(internal.stopped.get() && internal.joined.get());
        let restored = factory.latest(DisplayId::External);
        assert!(
            !Rc::ptr_eq(&restored, &old_external),
            "external is plugged back in"
        );
        restored.ready.set(true);
        manager.acknowledge(ack(3, &restored));
        manager.tick(now, &mut |id, s| factory.spawn(id, s));
        assert_eq!(manager.active_mask(), 2);
    }

    #[test]
    fn internal_resolution_change_in_both_replugs_in_order() {
        let (mut manager, mut factory, now) = bootstrap();
        manager.request(request(2, 3), now, &mut |id, s| factory.spawn(id, s));
        let first_external = factory.latest(DisplayId::External);
        first_external.ready.set(true);
        manager.acknowledge(ack(2, &first_external));
        manager.tick(now, &mut |id, s| factory.spawn(id, s));
        assert_eq!(manager.active_mask(), 3);
        let mut next = request(3, 3);
        next.outputs[0].as_mut().unwrap().width = 2560;
        manager.request(next, now, &mut |id, s| factory.spawn(id, s));
        assert!(
            first_external.stopped.get(),
            "external is replugged after the new internal"
        );
        assert_eq!(
            factory.latest(DisplayId::Internal).display,
            DisplayId::Internal
        );
    }

    #[test]
    fn shutdown_signals_and_joins_both_active_and_pending_workers() {
        let (mut manager, mut factory, now) = bootstrap();
        manager.request(request(2, 3), now, &mut |id, s| factory.spawn(id, s));
        drop(manager);
        assert!(factory
            .states
            .iter()
            .all(|s| s.stopped.get() && s.joined.get()));
    }

    #[test]
    fn changing_external_fps_keeps_internal_pipeline_unchanged() {
        let (mut manager, mut factory, now) = bootstrap();
        let internal = factory.latest(DisplayId::Internal);
        let original_generation = internal.generation.get();
        manager.request(request(2, 3), now, &mut |id, s| factory.spawn(id, s));
        let first_external = factory.latest(DisplayId::External);
        first_external.ready.set(true);
        manager.acknowledge(ack(2, &first_external));
        manager.tick(now, &mut |id, s| factory.spawn(id, s));
        let mut slower_external = request(3, 3);
        slower_external.outputs[1].as_mut().unwrap().fps = 30;
        manager.request(slower_external, now, &mut |id, s| factory.spawn(id, s));
        let replacement = factory.latest(DisplayId::External);
        replacement.ready.set(true);
        manager.acknowledge(ack(3, &replacement));
        manager.tick(now, &mut |id, s| factory.spawn(id, s));
        assert_eq!(factory.states.len(), 3);
        assert!(!internal.stopped.get());
        assert_eq!(internal.generation.get(), original_generation);
        assert!(first_external.stopped.get());
        assert_eq!(manager.active[0].as_ref().unwrap().settings().fps, 60);
        assert_eq!(manager.active[1].as_ref().unwrap().settings().fps, 30);
    }

    #[test]
    fn status_wire_format_matches_swift_fixture() {
        let status = DisplayStatus {
            revision: 7,
            display: DisplayId::External,
            phase: DisplayPhase::Streaming,
            active_mask: 1,
            generation: 0x01020304,
            settings: Some(OutputSettings {
                monitor_name: *b"Screx        ",
                width: 3840,
                height: 2160,
                fps: 60,
                codec: VideoCodec::H264,
                bitrate_bps: 15_000_000,
            }),
            detail: String::new(),
        };
        assert_eq!(
            status.encode(),
            [0x82, 0, 0, 0, 7, 1, 0, 1, 1, 2, 3, 4, 0x0f, 0, 8, 0x70]
        );
    }
}
