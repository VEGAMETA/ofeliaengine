//! The bus graph as the game thread edits it, and its compiled form for the audio thread.
//!
//! Buses form a tree through their parents; sends and sidechains add more edges. Any
//! change compiles the whole graph into a [`Schedule`]: buses in an order where every bus
//! comes after everything that feeds it (children, send sources, sidechain sources), so the
//! audio thread walks an array and never traverses a graph. Ties are broken by bus index,
//! so the same graph always compiles to the same schedule.

use crate::command::NO_BUS;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// One bus in processing order.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Step {
    pub(crate) bus: u16,
    /// Where the bus's output goes after its fader; `NO_BUS` for the master.
    pub(crate) parent: u16,
    /// Range into [`Schedule::effects`].
    pub(crate) effects: (u32, u32),
    /// Range into [`Schedule::sends`].
    pub(crate) sends: (u32, u32),
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SendStep {
    pub(crate) target: u16,
    /// Slot of the send's gain on the audio side.
    pub(crate) send: u32,
    pub(crate) pre_fader: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct EffectStep {
    /// Slot of the effect on the audio side.
    pub(crate) slot: u32,
    /// Bus whose signal keys the effect; `NO_BUS` for none.
    pub(crate) sidechain: u16,
}

/// The compiled graph.
#[derive(Debug, Default)]
pub(crate) struct Schedule {
    pub(crate) steps: Vec<Step>,
    pub(crate) sends: Vec<SendStep>,
    pub(crate) effects: Vec<EffectStep>,
}

pub(crate) struct BusNode {
    pub(crate) name: String,
    pub(crate) parent: u16,
    /// Effect slots in processing order.
    pub(crate) effects: Vec<u32>,
}

pub(crate) struct SendNode {
    pub(crate) from: u16,
    pub(crate) to: u16,
    pub(crate) pre_fader: bool,
}

pub(crate) struct EffectNode {
    pub(crate) bus: u16,
    pub(crate) sidechain: u16,
    /// Left out of the schedule (its sidechain's edge stays, so turning it back on never
    /// makes a cycle).
    pub(crate) bypassed: bool,
}

pub(crate) struct Graph {
    pub(crate) buses: Vec<Option<BusNode>>,
    pub(crate) sends: Vec<Option<SendNode>>,
    pub(crate) effects: Vec<Option<EffectNode>>,
}

impl Graph {
    pub(crate) fn new() -> Self {
        Self {
            buses: vec![Some(BusNode {
                name: String::from("master"),
                parent: NO_BUS,
                effects: Vec::new(),
            })],
            sends: Vec::new(),
            effects: Vec::new(),
        }
    }

    pub(crate) fn bus(&self, bus: u16) -> Option<&BusNode> {
        self.buses.get(usize::from(bus))?.as_ref()
    }

    /// Compiles the graph; `None` if it has a cycle.
    pub(crate) fn compile(&self) -> Option<Schedule> {
        let n = self.buses.len();
        let mut edges: Vec<Vec<u16>> = vec![Vec::new(); n];
        let mut incoming = vec![0u32; n];
        let mut edge = |from: u16, to: u16| {
            edges[usize::from(from)].push(to);
            incoming[usize::from(to)] += 1;
        };
        for (i, bus) in self.buses.iter().enumerate() {
            if let Some(bus) = bus
                && bus.parent != NO_BUS
            {
                edge(i as u16, bus.parent);
            }
        }
        for send in self.sends.iter().flatten() {
            edge(send.from, send.to);
        }
        for effect in self.effects.iter().flatten() {
            if effect.sidechain != NO_BUS {
                edge(effect.sidechain, effect.bus);
            }
        }

        let live = self.buses.iter().filter(|b| b.is_some()).count();
        let mut ready: BinaryHeap<Reverse<u16>> = self
            .buses
            .iter()
            .enumerate()
            .filter(|&(i, b)| b.is_some() && incoming[i] == 0)
            .map(|(i, _)| Reverse(i as u16))
            .collect();
        let mut order = Vec::with_capacity(live);
        while let Some(Reverse(bus)) = ready.pop() {
            order.push(bus);
            for &to in &edges[usize::from(bus)] {
                let slot = &mut incoming[usize::from(to)];
                *slot -= 1;
                if *slot == 0 {
                    ready.push(Reverse(to));
                }
            }
        }
        if order.len() != live {
            return None;
        }

        let mut schedule = Schedule::default();
        for bus in order {
            let node = self.bus(bus)?;
            let effects_start = schedule.effects.len() as u32;
            for &slot in &node.effects {
                let effect = self.effects.get(slot as usize).and_then(Option::as_ref);
                if effect.is_some_and(|e| e.bypassed) {
                    continue;
                }
                let sidechain = effect.map_or(NO_BUS, |e| e.sidechain);
                schedule.effects.push(EffectStep { slot, sidechain });
            }
            let sends_start = schedule.sends.len() as u32;
            for (i, send) in self.sends.iter().enumerate() {
                if let Some(send) = send
                    && send.from == bus
                {
                    schedule.sends.push(SendStep {
                        target: send.to,
                        send: i as u32,
                        pre_fader: send.pre_fader,
                    });
                }
            }
            schedule.steps.push(Step {
                bus,
                parent: node.parent,
                effects: (effects_start, schedule.effects.len() as u32),
                sends: (sends_start, schedule.sends.len() as u32),
            });
        }
        Some(schedule)
    }
}
