//! Animation graphs in the player (ADR 0124's E12-5c): an [`Animator`] whose `graph` names an
//! `.agraph` file plays it in place of a clip.
//!
//! - **A graph file** is read once, the first time an animator names it (or taken from the
//!   editor while it edits one, [`Graphs::hold`]), and turned into the runtime's description
//!   ([`desc_of`]): its parts by index in place of ids.
//! - **Its parameters** go into the world's [`GraphParams`] at each tick's start, before the
//!   scripts run ([`Graphs::know`]), so `animator:set("speed", 3.5)` finds its slot even on
//!   the tick a character appears.
//! - **It is cooked once per rig** that plays it ([`Graphs::cook`]: its clips found by name
//!   among the model's, its masks' joints in the skeleton), shared by every character of
//!   that rig; each character keeps its own [`GraphState`] ([`GraphPlay`]).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use engine_anim::{
    AnimGraph, ClipInfo, GraphDesc, GraphParamKind, GraphState, LayerDesc, NodeDesc, ParamDesc,
    StateDesc, TransitionDesc,
};
use engine_ecs::{QueryState, World};
use engine_runtime::{Animator, GraphParams, MAX_GRAPH_PARAMS};
use engine_scene::{AnimGraphFile, AssetId, AssetRef, LayerBlend, NodeKind, ParamKind};

use crate::skins::Rig;

/// A graph file as the player read it: the runtime's description, its parameters by slot
/// (each one's name and whether it is a trigger; at most [`MAX_GRAPH_PARAMS`]), and where
/// it came from, for messages.
#[derive(Debug)]
pub(crate) struct GraphFile {
    pub(crate) desc: GraphDesc,
    pub(crate) names: Vec<(String, bool)>,
    pub(crate) source: String,
}

impl GraphFile {
    /// `file` as the runtime takes it, from `source` (its path, or the editor).
    pub(crate) fn new(file: &AnimGraphFile, source: String) -> Self {
        let desc = desc_of(file);
        let names = desc
            .params
            .iter()
            .take(MAX_GRAPH_PARAMS)
            .map(|p| (p.name.clone(), p.kind == GraphParamKind::Trigger))
            .collect();
        Self {
            desc,
            names,
            source,
        }
    }
}

/// A graph cooked for one rig, shared by the characters of that rig that play it.
#[derive(Debug)]
pub(crate) struct Cooked {
    pub(crate) graph: AnimGraph,
    /// The rig it was cooked for: kept, so the address it is found by stays this rig's.
    _rig: Arc<Rig>,
    /// The rig's model file: the model read again cooks it again.
    model: AssetId,
}

/// A character's play of its graph: the graph file, the graph cooked for its rig, its own
/// state, and the [`Graphs::generation`] it was last checked at.
#[derive(Debug)]
pub(crate) struct GraphPlay {
    pub(crate) file: AssetId,
    pub(crate) cooked: Arc<Cooked>,
    pub(crate) state: GraphState,
    pub(crate) generation: u64,
}

/// The player's animation graphs (see the module).
#[derive(Default)]
pub(crate) struct Graphs {
    /// Each graph file read (none: it could not be, told once), or held by the editor.
    files: HashMap<AssetId, Option<Arc<GraphFile>>>,
    held: Vec<AssetId>,
    /// Each graph cooked, by its file and its rig's address.
    cooked: HashMap<(AssetId, usize), Arc<Cooked>>,
    /// Moves on whenever a graph file or a cooked graph changes: a character's play made at
    /// another looks again whether its graph changed.
    generation: u64,
    /// Graph files whose parameters [`GraphParams`] has not been told yet.
    untold: Vec<AssetId>,
    /// The animators, and the graph files they name not read yet (room kept).
    animators: Option<QueryState<&'static Animator>>,
    found: Vec<AssetId>,
}

impl core::fmt::Debug for Graphs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Graphs")
            .field("files", &self.files.len())
            .field("cooked", &self.cooked.len())
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl Graphs {
    /// See [`Graphs::generation`](Self#structfield.generation).
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Graph file `id`, read from `path` the first time it is asked for (a file that cannot
    /// be read is told once, then is none until it changes).
    pub(crate) fn read(
        &mut self,
        id: AssetId,
        path: impl FnOnce() -> Option<PathBuf>,
    ) -> Option<&Arc<GraphFile>> {
        if !self.files.contains_key(&id) {
            let read = match path() {
                Some(path) => match AnimGraphFile::load(&path) {
                    Ok(file) => Some(Arc::new(GraphFile::new(&file, path.display().to_string()))),
                    Err(why) => {
                        log::warn!(target: "engine_app", "animation graph: {why}");
                        None
                    }
                },
                None => {
                    log::warn!(
                        target: "engine_app",
                        "animation graph {id}: no such file in the project"
                    );
                    None
                }
            };
            if read.is_some() {
                self.untold.push(id);
            }
            self.files.insert(id, read);
        }
        self.files.get(&id)?.as_ref()
    }

    /// Graph file `id` as the editor holds it while it edits it, in place of the file until
    /// `None` gives the file back: its characters play it from their next tick, cooked again
    /// (their states made anew).
    pub(crate) fn hold(&mut self, id: AssetId, file: Option<&AnimGraphFile>) {
        match file {
            Some(file) => {
                let file = GraphFile::new(file, String::from("the editor's copy"));
                self.files.insert(id, Some(Arc::new(file)));
                if !self.held.contains(&id) {
                    self.held.push(id);
                }
                self.untold.push(id);
            }
            None => {
                self.held.retain(|&h| h != id);
                self.files.remove(&id);
            }
        }
        self.cooked.retain(|&(graph, _), _| graph != id);
        self.generation += 1;
    }

    /// File `file` was written anew (ADR 0124's E12-5c): a graph file read from it is read
    /// again when next asked for (but one the editor holds), and the graphs cooked from it,
    /// or for a rig of a model of it, are cooked again.
    pub(crate) fn forget(&mut self, file: AssetId) {
        let before = (self.files.len(), self.cooked.len());
        if !self.held.contains(&file) {
            self.files.remove(&file);
        }
        self.cooked
            .retain(|&(graph, _), c| graph != file && c.model != file);
        if (self.files.len(), self.cooked.len()) != before {
            self.generation += 1;
        }
    }

    /// Graph file `id` cooked for rig `rig` of model file `model` (once per rig): `clip`
    /// finds a clip the graph names among the rig's. What cannot be cooked is told once (the
    /// rest still plays); none for a file not read.
    pub(crate) fn cook(
        &mut self,
        id: AssetId,
        rig: &Arc<Rig>,
        model: AssetId,
        clip: impl Fn(&str) -> Option<ClipInfo>,
    ) -> Option<Arc<Cooked>> {
        let key = (id, Arc::as_ptr(rig) as usize);
        if let Some(cooked) = self.cooked.get(&key) {
            return Some(Arc::clone(cooked));
        }
        let file = Arc::clone(self.files.get(&id)?.as_ref()?);
        let clips = rig.clips();
        let (mut graph, problems) = AnimGraph::cook(&file.desc, clip, rig.skeleton());
        graph.prepare(
            |k| clips.get(usize::from(k)).map(crate::RigClip::compressed),
            rig.skeleton().rest(),
        );
        for p in &problems {
            log::warn!(
                target: "engine_app",
                "animation graph {}: {}: {}",
                file.source,
                p.what,
                p.why
            );
        }
        let cooked = Arc::new(Cooked {
            graph,
            _rig: Arc::clone(rig),
            model,
        });
        self.cooked.insert(key, Arc::clone(&cooked));
        Some(cooked)
    }

    /// The parameters of every graph an [`Animator`] in `world` names, read if they were not
    /// (`path_of` a graph file's path) and told to the world's [`GraphParams`]: what each
    /// tick does before its scripts run, so they find the slots of a character that appeared
    /// since. A run of animators naming one graph looks it up once.
    pub(crate) fn know(&mut self, world: &mut World, path_of: impl Fn(AssetId) -> Option<PathBuf>) {
        let animators = self.animators.get_or_insert_with(|| QueryState::new(world));
        let mut last = None;
        for animator in animators.iter_readonly(world) {
            let graph = animator.graph;
            if graph == AssetRef::NONE || last == Some(graph.file) {
                continue;
            }
            last = Some(graph.file);
            if !self.files.contains_key(&graph.file) && !self.found.contains(&graph.file) {
                self.found.push(graph.file);
            }
        }
        let mut found = core::mem::take(&mut self.found);
        for id in found.drain(..) {
            self.read(id, || path_of(id));
        }
        self.found = found;
        self.tell(world);
    }

    /// The parameters of the graph files read or held since the last time into the world's
    /// [`GraphParams`].
    pub(crate) fn tell(&mut self, world: &mut World) {
        if self.untold.is_empty() {
            return;
        }
        let Some(params) = world.resource_mut::<GraphParams>() else {
            return;
        };
        for id in self.untold.drain(..) {
            if let Some(Some(file)) = self.files.get(&id) {
                params.insert(id, file.names.iter().map(|(n, t)| (n.as_str(), *t)));
            }
        }
    }
}

/// Graph file `file` as the runtime takes it (ADR 0124's E12-5c): its parts by index in place
/// of ids, the layers in the order they play, the rest in their ids' order (parameters in
/// their names', which are their slots). A machine without an entry starts in its first
/// state; a state without a name is named by its id; an id that names nothing becomes an
/// index out of range, which the cook names as a problem.
pub(crate) fn desc_of(file: &AnimGraphFile) -> GraphDesc {
    fn index_of<'a>(ids: impl Iterator<Item = &'a String>) -> HashMap<&'a str, usize> {
        ids.enumerate().map(|(k, id)| (id.as_str(), k)).collect()
    }
    let nodes = index_of(file.nodes.keys());
    let states = index_of(file.states.keys());
    let node = |id: &str| nodes.get(id).copied().unwrap_or(usize::MAX);
    let state = |id: &str| states.get(id).copied().unwrap_or(usize::MAX);
    let entry = |machine: &str, entry: &str| {
        if entry.is_empty() {
            file.states
                .iter()
                .position(|(_, s)| s.machine == machine)
                .unwrap_or(usize::MAX)
        } else {
            state(entry)
        }
    };
    GraphDesc {
        params: file
            .params
            .iter()
            .map(|(name, p)| ParamDesc {
                name: name.clone(),
                kind: match p.kind {
                    ParamKind::Float => GraphParamKind::Float,
                    ParamKind::Bool => GraphParamKind::Bool,
                    ParamKind::Int => GraphParamKind::Int,
                    ParamKind::Trigger => GraphParamKind::Trigger,
                },
                value: p.value,
            })
            .collect(),
        layers: file
            .layers_in_order()
            .into_iter()
            .map(|(_, l)| LayerDesc {
                name: l.name.clone(),
                root: node(&l.root),
                additive: l.blend == LayerBlend::Additive,
                weight: l.weight,
                weight_param: (!l.weight_param.is_empty()).then(|| l.weight_param.clone()),
                mask: l.mask.iter().map(|(j, &w)| (j.clone(), w)).collect(),
            })
            .collect(),
        nodes: file
            .nodes
            .iter()
            .map(|(id, n)| match n.kind {
                NodeKind::Clip => NodeDesc::Clip {
                    clip: n.clip.clone(),
                    speed: n.speed,
                },
                NodeKind::Blend1d => NodeDesc::Blend1d {
                    param: n.param.clone(),
                    points: n.points.iter().map(|p| (p.at[0], p.clip.clone())).collect(),
                    speed: n.speed,
                },
                NodeKind::Blend2d => NodeDesc::Blend2d {
                    params: [n.param.clone(), n.param_y.clone()],
                    points: n.points.iter().map(|p| (p.at, p.clip.clone())).collect(),
                    speed: n.speed,
                },
                NodeKind::Machine => NodeDesc::Machine {
                    entry: entry(id, &n.entry),
                },
            })
            .collect(),
        states: file
            .states
            .iter()
            .map(|(id, s)| StateDesc {
                name: if s.name.is_empty() {
                    id.clone()
                } else {
                    s.name.clone()
                },
                machine: node(&s.machine),
                pose: node(&s.pose),
            })
            .collect(),
        transitions: file
            .transitions
            .values()
            .map(|t| TransitionDesc {
                from: (!t.from.is_empty()).then(|| state(&t.from)),
                to: state(&t.to),
                when: t.when.clone(),
                blend: t.blend,
                priority: t.priority,
                exit: t.exit,
                offset: t.offset,
                keep_phase: t.keep_phase,
                inertialize: t.inertialize,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use engine_scene::{BlendPoint, LayerFile, NodeFile, ParamFile, StateFile, TransitionFile};

    /// Idle and a walk blend in a machine, an upper layer playing one clip on a spine.
    fn file() -> AnimGraphFile {
        let mut f = AnimGraphFile::default();
        f.params.insert(
            "speed".into(),
            ParamFile {
                kind: ParamKind::Float,
                value: 0.5,
            },
        );
        f.params.insert(
            "jump".into(),
            ParamFile {
                kind: ParamKind::Trigger,
                value: 0.0,
            },
        );
        f.layers.insert(
            "up".into(),
            LayerFile {
                name: "Upper".into(),
                order: 1,
                root: "wave".into(),
                weight_param: "speed".into(),
                mask: [("spine".to_owned(), 1.0)].into(),
                ..LayerFile::default()
            },
        );
        f.layers.insert(
            "base".into(),
            LayerFile {
                name: "Base".into(),
                root: "m".into(),
                ..LayerFile::default()
            },
        );
        f.nodes.insert(
            "m".into(),
            NodeFile {
                kind: NodeKind::Machine,
                ..NodeFile::default()
            },
        );
        f.nodes.insert(
            "idle".into(),
            NodeFile {
                clip: "Survey".into(),
                ..NodeFile::default()
            },
        );
        f.nodes.insert(
            "walk".into(),
            NodeFile {
                kind: NodeKind::Blend1d,
                param: "speed".into(),
                points: vec![
                    BlendPoint {
                        at: [0.0, 0.0],
                        clip: "Walk".into(),
                    },
                    BlendPoint {
                        at: [2.0, 0.0],
                        clip: "Run".into(),
                    },
                ],
                speed: 1.5,
                ..NodeFile::default()
            },
        );
        f.nodes.insert(
            "wave".into(),
            NodeFile {
                clip: "Wave".into(),
                ..NodeFile::default()
            },
        );
        f.states.insert(
            "s1".into(),
            StateFile {
                name: "Idle".into(),
                machine: "m".into(),
                pose: "idle".into(),
                ..StateFile::default()
            },
        );
        f.states.insert(
            "s2".into(),
            StateFile {
                machine: "m".into(),
                pose: "walk".into(),
                ..StateFile::default()
            },
        );
        f.transitions.insert(
            "t1".into(),
            TransitionFile {
                from: "s1".into(),
                to: "s2".into(),
                when: "speed > 0.1".into(),
                ..TransitionFile::default()
            },
        );
        f.transitions.insert(
            "t2".into(),
            TransitionFile {
                to: "gone".into(),
                when: "jump".into(),
                keep_phase: true,
                inertialize: true,
                ..TransitionFile::default()
            },
        );
        f
    }

    /// ADR 0124's E12-5c: a graph file's ids become indices: layers in their order, the rest
    /// in their ids' order; a machine without an entry starts in its first state, a state
    /// without a name is named by its id, an id naming nothing is out of range.
    #[test]
    fn a_graph_files_ids_become_the_runtimes_indices() {
        let desc = desc_of(&file());
        let names: Vec<_> = desc.params.iter().map(|p| (&*p.name, p.kind)).collect();
        assert_eq!(
            names,
            [
                ("jump", GraphParamKind::Trigger),
                ("speed", GraphParamKind::Float)
            ],
            "by name, which is the slot"
        );
        assert_eq!(desc.params[1].value, 0.5);
        // Layers by order: the base (order 0) first.
        assert_eq!(desc.layers[0].name, "Base");
        assert_eq!(desc.layers[1].name, "Upper");
        // Nodes by id: idle, m, walk, wave.
        assert_eq!(desc.layers[0].root, 1);
        assert_eq!(desc.layers[1].root, 3);
        assert_eq!(desc.layers[1].weight_param.as_deref(), Some("speed"));
        assert_eq!(desc.layers[1].mask, [("spine".to_owned(), 1.0)]);
        assert!(desc.layers[0].weight_param.is_none());
        assert!(matches!(desc.nodes[1], NodeDesc::Machine { entry: 0 }));
        let NodeDesc::Blend1d {
            param,
            points,
            speed,
        } = &desc.nodes[2]
        else {
            panic!("{:?}", desc.nodes[2]);
        };
        assert_eq!(param, "speed");
        assert_eq!(points, &[(0.0, "Walk".to_owned()), (2.0, "Run".to_owned())]);
        assert_eq!(*speed, 1.5);
        // States by id; the unnamed one by its id.
        assert_eq!(desc.states[0].name, "Idle");
        assert_eq!(desc.states[1].name, "s2");
        assert_eq!((desc.states[1].machine, desc.states[1].pose), (1, 2));
        // From s1 to s2; from any state to a state that is not there.
        let (t1, t2) = (&desc.transitions[0], &desc.transitions[1]);
        assert_eq!((t1.from, t1.to, &*t1.when), (Some(0), 1, "speed > 0.1"));
        assert_eq!((t2.from, t2.to), (None, usize::MAX));
        assert!(t2.keep_phase && t2.inertialize && !t1.inertialize);
        // The file's parameters for scripts: by slot, triggers marked.
        let graph = GraphFile::new(&file(), String::from("test"));
        assert_eq!(
            graph.names,
            [("jump".to_owned(), true), ("speed".to_owned(), false)]
        );
    }

    /// A graph with more parameters than an animator has slots tells scripts the first 16.
    #[test]
    fn a_graph_tells_scripts_of_at_most_sixteen_parameters() {
        let mut f = AnimGraphFile::default();
        for k in 0..20 {
            f.params.insert(format!("p{k:02}"), ParamFile::default());
        }
        let graph = GraphFile::new(&f, String::from("test"));
        assert_eq!(graph.names.len(), MAX_GRAPH_PARAMS);
        assert_eq!(graph.names[15].0, "p15");
    }
}
