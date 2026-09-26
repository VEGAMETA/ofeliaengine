//! Layout of a frame's node tree (ADR 0030). Nodes are stored in pre-order, so every
//! parent comes before its children: bottom-up passes walk the array backwards,
//! top-down passes forwards. Each axis is sized in two passes (content sizes up, then
//! grow and percent sizes down); text is wrapped once widths are known, then heights
//! are sized the same way, then positions are assigned.

use crate::{NONE, Node, Rect, Size, TextMeasure};

/// Main-axis direction of a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    /// Children left to right.
    #[default]
    Row,
    /// Children top to bottom.
    Column,
}

impl Direction {
    fn main(self) -> usize {
        match self {
            Self::Row => 0,
            Self::Column => 1,
        }
    }
}

/// Alignment of children inside the free space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    /// Left or top.
    #[default]
    Start,
    /// Centered.
    Center,
    /// Right or bottom.
    End,
}

impl Align {
    fn offset(self, free: f32) -> f32 {
        match self {
            Self::Start => 0.0,
            Self::Center => (free * 0.5).max(0.0),
            Self::End => free.max(0.0),
        }
    }
}

fn padding_along(n: &Node, axis: usize) -> f32 {
    let p = n.style.padding;
    if axis == 0 { p[0] + p[2] } else { p[1] + p[3] }
}

fn clamp(n: &Node, axis: usize, v: f32) -> f32 {
    v.max(n.style.min[axis]).min(n.style.max[axis])
}

fn children(nodes: &[Node], i: usize) -> impl Iterator<Item = usize> + '_ {
    let mut c = nodes[i].first_child;
    core::iter::from_fn(move || {
        (c != NONE).then(|| {
            let cur = c as usize;
            c = nodes[cur].next_sibling;
            cur
        })
    })
}

/// Size along `axis` that a child contributes to its parent's content size.
fn contribution(n: &Node, axis: usize) -> f32 {
    match n.style.size[axis] {
        Size::Fixed(v) => clamp(n, axis, v),
        Size::Fit => n.rect.size[axis],
        Size::Grow(_) | Size::Percent(_) => n.style.min[axis],
    }
}

fn fit_pass(nodes: &mut [Node], axis: usize) {
    for i in (0..nodes.len()).rev() {
        let n = &nodes[i];
        let pad = padding_along(n, axis);
        let mut content = n.content[axis];
        let main = n.style.direction.main();
        let mut count = 0;
        let mut sum = 0.0f32;
        let mut max = 0.0f32;
        for c in children(nodes, i) {
            if nodes[c].style.floating.is_some() {
                continue;
            }
            let v = contribution(&nodes[c], axis);
            sum += v;
            max = max.max(v);
            count += 1;
        }
        if count > 0 {
            let gaps = n.style.gap * (count - 1) as f32;
            content = content.max(if axis == main { sum + gaps } else { max });
        }
        let size = match n.style.size[axis] {
            Size::Fixed(v) => v,
            _ => content + pad,
        };
        let n = &mut nodes[i];
        n.fit[axis] = content + pad;
        n.rect.size[axis] = clamp(n, axis, size);
    }
}

fn grow_pass(nodes: &mut [Node], axis: usize) {
    for i in 0..nodes.len() {
        let parent = &nodes[i];
        let inner = (parent.rect.size[axis] - padding_along(parent, axis)).max(0.0);
        let main = parent.style.direction.main();
        let gap = parent.style.gap;
        if axis == main {
            let mut used = 0.0f32;
            let mut weights = 0.0f32;
            let mut count = 0;
            for c in children(nodes, i) {
                let cn = &nodes[c];
                if cn.style.floating.is_some() {
                    continue;
                }
                count += 1;
                match cn.style.size[axis] {
                    Size::Grow(w) => weights += w.max(0.0),
                    Size::Percent(p) => used += clamp(cn, axis, inner * p),
                    _ => used += cn.rect.size[axis],
                }
            }
            let gaps = if count > 1 {
                gap * (count - 1) as f32
            } else {
                0.0
            };
            let free = (inner - used - gaps).max(0.0);
            let mut c = nodes[i].first_child;
            while c != NONE {
                let cn = &mut nodes[c as usize];
                match cn.style.size[axis] {
                    Size::Grow(w) if weights > 0.0 => {
                        cn.rect.size[axis] = clamp(cn, axis, free * w.max(0.0) / weights);
                    }
                    Size::Percent(p) => cn.rect.size[axis] = clamp(cn, axis, inner * p),
                    _ => {}
                }
                c = cn.next_sibling;
            }
        } else {
            let mut c = nodes[i].first_child;
            while c != NONE {
                let cn = &mut nodes[c as usize];
                match cn.style.size[axis] {
                    Size::Grow(_) => cn.rect.size[axis] = clamp(cn, axis, inner),
                    Size::Percent(p) => cn.rect.size[axis] = clamp(cn, axis, inner * p),
                    _ => {}
                }
                c = cn.next_sibling;
            }
        }
    }
}

fn position_pass(nodes: &mut [Node], scroll: &dyn Fn(u64) -> f32) {
    for i in 0..nodes.len() {
        let p = &nodes[i];
        let main = p.style.direction.main();
        let cross = 1 - main;
        let pad = p.style.padding;
        let start = [p.rect.pos[0] + pad[0], p.rect.pos[1] + pad[1]];
        let inner = [
            (p.rect.size[0] - pad[0] - pad[2]).max(0.0),
            (p.rect.size[1] - pad[1] - pad[3]).max(0.0),
        ];
        let gap = p.style.gap;
        let (align_main, align_cross) = (p.style.align_main, p.style.align_cross);
        let scrolled = if p.style.scroll { scroll(p.id) } else { 0.0 };
        let origin = p.rect.pos;
        let mut total = 0.0f32;
        let mut count = 0;
        for c in children(nodes, i) {
            if nodes[c].style.floating.is_none() {
                total += nodes[c].rect.size[main];
                count += 1;
            }
        }
        if count > 1 {
            total += gap * (count - 1) as f32;
        }
        let mut cursor = start[main] + align_main.offset(inner[main] - total) - scrolled;
        let mut c = nodes[i].first_child;
        while c != NONE {
            let cn = &mut nodes[c as usize];
            if let Some(offset) = cn.style.floating {
                cn.rect.pos = [origin[0] + offset[0], origin[1] + offset[1]];
            } else {
                cn.rect.pos[main] = cursor;
                cn.rect.pos[cross] =
                    start[cross] + align_cross.offset(inner[cross] - cn.rect.size[cross]);
                cursor += cn.rect.size[main] + gap;
            }
            c = cn.next_sibling;
        }
    }
}

/// Lays out `nodes` inside `root` (the viewport). `text` holds the frame's strings;
/// `scroll` gives a scroll container's offset by id.
pub(crate) fn layout(
    nodes: &mut [Node],
    text: &str,
    root: Rect,
    measure: &mut dyn TextMeasure,
    scroll: &dyn Fn(u64) -> f32,
) {
    engine_profiler::zone!("ui::layout");
    if nodes.is_empty() {
        return;
    }
    // Natural (unwrapped) text sizes feed the width passes.
    for n in nodes.iter_mut() {
        if let Some(t) = &n.text {
            let s = &text[t.range.clone()];
            n.content = measure.measure(s, t.size, None);
        }
    }
    nodes[0].style.size = [Size::Fixed(root.size[0]), Size::Fixed(root.size[1])];
    fit_pass(nodes, 0);
    grow_pass(nodes, 0);
    // Wrapped text: its height depends on the width it received.
    for n in nodes.iter_mut() {
        if let Some(t) = &n.text
            && t.wrap
        {
            let width = (n.rect.size[0] - padding_along(n, 0)).max(0.0);
            let s = &text[t.range.clone()];
            n.content[1] = measure.measure(s, t.size, Some(width))[1];
        }
    }
    fit_pass(nodes, 1);
    grow_pass(nodes, 1);
    nodes[0].rect.pos = root.pos;
    position_pass(nodes, scroll);
}
