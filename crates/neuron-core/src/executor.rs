//! Shared live dispatch execution.
//!
//! The engine owns matching (`Trigger -> Action`). This executor owns the repeatable runtime
//! algorithm around it: resolve, capture context only when needed, remember the last non-echo
//! action, run host actions, and delegate device/app/profile intents to the embedding runtime.

use crate::action::{Action, Intent};
use crate::engine::{Engine, Trigger};
use crate::macros::context::Context;
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub trait IntentRunner {
    fn run_intent(&mut self, intent: &Intent) -> String;
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DispatchOutcome {
    pub trigger: String,
    pub action: String,
    pub matched: usize,
    pub turbo: Vec<TurboStart>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurboStart {
    pub trigger: Trigger,
    pub action: Action,
    pub cps: u16,
}

struct HeldTurbo {
    action: Action,
    interval: Duration,
    next: Instant,
}

#[derive(Default)]
pub struct TurboRuntime {
    held: HashMap<Trigger, HeldTurbo>,
}

impl TurboRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn start(&mut self, starts: Vec<TurboStart>) {
        let now = Instant::now();
        for start in starts {
            // Clamp to a sane band: `.max(1)` floors it, and a ceiling stops a fat-fingered config
            // (`cps` is a u16 — up to 65535) from asking the tick loop to fire hundreds of inputs.
            let cps = start.cps.clamp(1, 100);
            let interval = Duration::from_secs_f64(1.0 / f64::from(cps));
            self.held.insert(
                start.trigger,
                HeldTurbo {
                    action: start.action,
                    interval,
                    next: now + interval,
                },
            );
        }
    }

    pub fn release(&mut self, trigger: &Trigger) {
        self.held.remove(trigger);
    }

    pub fn clear(&mut self) {
        self.held.clear();
    }

    /// Is ANY turbo currently held? Cheap (a length check) — the pump-cadence seam (see
    /// `neuron-app::dispatch::live_tick`) calls this every tick to decide whether it needs a short
    /// wake interval, so it must stay O(1).
    pub fn is_active(&self) -> bool {
        !self.held.is_empty()
    }

    /// The shortest repeat interval among currently-held turbos, if any are held — so the pump-
    /// cadence seam can use the turbo's OWN rate instead of a guessed constant. `None` when no
    /// turbo is held (mirrors [`is_active`](Self::is_active)).
    pub fn min_interval(&self) -> Option<Duration> {
        self.held.values().map(|t| t.interval).min()
    }

    pub fn tick(&mut self, exec: &mut DispatchExecutor, intents: &mut impl IntentRunner) {
        let now = Instant::now();
        for turbo in self.held.values_mut() {
            // Cap the catch-up burst: after a long stall (a blocked tick), don't dump the whole
            // backlog of missed intervals as one storm of inputs — fire a few, then resync the clock.
            let mut fired = 0u32;
            while now >= turbo.next {
                let _ = exec.run_repeated_action(&turbo.action, intents);
                turbo.next += turbo.interval;
                fired += 1;
                if fired >= 8 {
                    turbo.next = now + turbo.interval;
                    break;
                }
            }
        }
    }
}

#[derive(Default)]
pub struct DispatchExecutor {
    last_action: Option<Action>,
}

impl DispatchExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn last_action(&self) -> Option<&Action> {
        self.last_action.as_ref()
    }

    pub fn last_action_desc(&self) -> Option<String> {
        self.last_action.as_ref().map(Action::describe)
    }

    pub fn clear(&mut self) {
        self.last_action = None;
    }

    pub fn fire(
        &mut self,
        engine: &Engine,
        trigger: &Trigger,
        intents: &mut impl IntentRunner,
    ) -> Option<DispatchOutcome> {
        let matched = engine.resolve(trigger);
        if matched.is_empty() {
            return None;
        }
        let ctx = if matched.iter().any(|r| r.action.needs_context()) {
            Context::capture()
        } else {
            Context::default()
        };
        let mut last = String::new();
        let mut turbo = Vec::new();
        let n = matched.len();
        for rule in matched {
            if let Some((cps, inner)) = rule.action.turbo() {
                turbo.push(TurboStart {
                    trigger: trigger.clone(),
                    action: inner.clone(),
                    cps,
                });
            }
            last = self.run_action(&rule.action, &ctx, intents);
        }
        Some(DispatchOutcome {
            trigger: trigger.describe(),
            action: last,
            matched: n,
            turbo,
        })
    }

    pub fn run_repeated_action(
        &mut self,
        action: &Action,
        intents: &mut impl IntentRunner,
    ) -> String {
        let ctx = if action.needs_context() {
            Context::capture()
        } else {
            Context::default()
        };
        if let Some(intent) = action.intent() {
            intents.run_intent(&intent)
        } else if let Some((_cps, inner)) = action.turbo() {
            inner.run_ctx(&ctx)
        } else {
            action.run_ctx(&ctx)
        }
    }

    fn run_action(
        &mut self,
        action: &Action,
        ctx: &Context,
        intents: &mut impl IntentRunner,
    ) -> String {
        if matches!(action, Action::Echo) {
            return self.echo(ctx, intents);
        }

        self.last_action = Some(action.clone());
        if let Some(intent) = action.intent() {
            intents.run_intent(&intent)
        } else if let Some((cps, inner)) = action.turbo() {
            let r = inner.run_ctx(ctx);
            format!("turbo {cps}cps (single press): {r}")
        } else {
            action.run_ctx(ctx)
        }
    }

    fn echo(&mut self, ctx: &Context, intents: &mut impl IntentRunner) -> String {
        let Some(action) = self.last_action.clone() else {
            return "nothing to echo yet".into();
        };
        let result = self.run_action(&action, ctx, intents);
        format!("echo -> {result}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;
    use crate::engine::Rule;

    #[derive(Default)]
    struct NullIntents;

    impl IntentRunner for NullIntents {
        fn run_intent(&mut self, intent: &Intent) -> String {
            format!("intent: {intent:?}")
        }
    }

    #[derive(Default)]
    struct CountingIntents {
        count: usize,
    }

    impl IntentRunner for CountingIntents {
        fn run_intent(&mut self, _intent: &Intent) -> String {
            self.count += 1;
            format!("intent {}", self.count)
        }
    }

    #[test]
    fn echo_replays_turbo_through_the_same_executor_path() {
        let trigger = Trigger::AppFocus {
            app: "game.exe".into(),
        };
        let echo = Trigger::MicTap;
        let engine = Engine::from_rules(vec![
            Rule::new(
                trigger.clone(),
                Action::Turbo {
                    action: Box::new(Action::Noop),
                    cps: 12,
                },
            ),
            Rule::new(echo.clone(), Action::Echo),
        ]);
        let mut exec = DispatchExecutor::new();
        let mut intents = NullIntents;

        let first = exec
            .fire(&engine, &trigger, &mut intents)
            .expect("turbo fires");
        assert!(first.action.contains("turbo 12cps"));
        let echoed = exec.fire(&engine, &echo, &mut intents).expect("echo fires");
        assert!(echoed.action.contains("echo -> turbo 12cps"), "{echoed:?}");
    }

    #[test]
    fn echo_without_history_is_honest() {
        let engine = Engine::from_rules(vec![Rule::new(Trigger::MicTap, Action::Echo)]);
        let mut exec = DispatchExecutor::new();
        let mut intents = NullIntents;
        let echoed = exec
            .fire(&engine, &Trigger::MicTap, &mut intents)
            .expect("echo fires");
        assert_eq!(echoed.action, "nothing to echo yet");
    }

    #[test]
    fn turbo_runtime_ticks_until_release() {
        let trigger = Trigger::MicTap;
        let mut turbo = TurboRuntime::new();
        turbo.start(vec![TurboStart {
            trigger: trigger.clone(),
            action: Action::ProfileSwitch { name: "p".into() },
            cps: 100, // within the clamped band → 10ms interval
        }]);

        let mut exec = DispatchExecutor::new();
        let mut intents = CountingIntents::default();
        std::thread::sleep(Duration::from_millis(25)); // > one interval, so a tick fires at least once
        turbo.tick(&mut exec, &mut intents);
        assert!(intents.count >= 1);

        turbo.release(&trigger);
        let before = intents.count;
        std::thread::sleep(Duration::from_millis(25));
        turbo.tick(&mut exec, &mut intents);
        assert_eq!(intents.count, before);
    }
}
