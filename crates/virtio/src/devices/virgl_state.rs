//! What a GL (virgl) context has set up on the host, kept so a snapshot
//! can build it again.
//!
//! GL state lives in host GL objects that can't be saved. But it all came
//! from the guest's command stream, so we follow that stream: the objects
//! the guest created and hasn't destroyed (shaders, blend states, sampler
//! views...), and the latest of each setter (bindings, viewports,
//! framebuffer...). Draws, clears, copies and transfers only change
//! resource contents, which a snapshot reads back separately. Replaying
//! [`GlState::replay`] into a fresh context gives the same state.
//!
//! Each setter is kept until a later one writes everything it wrote (its
//! "footprint", modelled on vrend's semantics), so the list stays short.
//! Commands we can't model mark the context as not saveable.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

// virgl_protocol.h, enum virgl_context_cmd.
const NOP: u8 = 0;
const CREATE_OBJECT: u8 = 1;
const BIND_OBJECT: u8 = 2;
const DESTROY_OBJECT: u8 = 3;
const SET_VIEWPORT_STATE: u8 = 4;
const SET_FRAMEBUFFER_STATE: u8 = 5;
const SET_VERTEX_BUFFERS: u8 = 6;
const CLEAR: u8 = 7;
const DRAW_VBO: u8 = 8;
const RESOURCE_INLINE_WRITE: u8 = 9;
const SET_SAMPLER_VIEWS: u8 = 10;
const SET_INDEX_BUFFER: u8 = 11;
const SET_CONSTANT_BUFFER: u8 = 12;
const SET_STENCIL_REF: u8 = 13;
const SET_BLEND_COLOR: u8 = 14;
const SET_SCISSOR_STATE: u8 = 15;
const BLIT: u8 = 16;
const RESOURCE_COPY_REGION: u8 = 17;
const BIND_SAMPLER_STATES: u8 = 18;
const BEGIN_QUERY: u8 = 19;
const END_QUERY: u8 = 20;
const GET_QUERY_RESULT: u8 = 21;
const SET_POLYGON_STIPPLE: u8 = 22;
const SET_CLIP_STATE: u8 = 23;
const SET_SAMPLE_MASK: u8 = 24;
const SET_STREAMOUT_TARGETS: u8 = 25;
const SET_RENDER_CONDITION: u8 = 26;
const SET_UNIFORM_BUFFER: u8 = 27;
const SET_SUB_CTX: u8 = 28;
const CREATE_SUB_CTX: u8 = 29;
const DESTROY_SUB_CTX: u8 = 30;
const BIND_SHADER: u8 = 31;
const SET_TESS_STATE: u8 = 32;
const SET_MIN_SAMPLES: u8 = 33;
const SET_SHADER_BUFFERS: u8 = 34;
const SET_SHADER_IMAGES: u8 = 35;
const MEMORY_BARRIER: u8 = 36;
const LAUNCH_GRID: u8 = 37;
const SET_FRAMEBUFFER_STATE_NO_ATTACH: u8 = 38;
const TEXTURE_BARRIER: u8 = 39;
const SET_ATOMIC_BUFFERS: u8 = 40;
const SET_DEBUG_FLAGS: u8 = 41;
const GET_QUERY_RESULT_QBO: u8 = 42;
const TRANSFER3D: u8 = 43;
const END_TRANSFERS: u8 = 44;
const COPY_TRANSFER3D: u8 = 45;
const SET_TWEAKS: u8 = 46;
const CLEAR_TEXTURE: u8 = 47;
const PIPE_RESOURCE_SET_TYPE: u8 = 49;
const GET_MEMORY_INFO: u8 = 50;
const SEND_STRING_MARKER: u8 = 51;
const LINK_SHADER: u8 = 52;

// enum virgl_object_type.
const OBJ_SHADER: u8 = 4;
const OBJ_SAMPLER_VIEW: u8 = 6;
const OBJ_SURFACE: u8 = 8;
const OBJ_QUERY: u8 = 9;
const OBJ_STREAMOUT_TARGET: u8 = 10;
const OBJ_MSAA_SURFACE: u8 = 11;

/// A shader's first chunk holds the full length; the rest say "continue".
const SHADER_OFFSET_CONT: u32 = 1 << 31;
/// No context makes more objects than this; past it we stop tracking.
const MAX_OBJECTS: usize = 1 << 20;
/// Nor keeps this many words of object and setter commands.
const MAX_WORDS: usize = 64 << 20;

fn header(cmd: u8, obj: u8, len: usize) -> u32 {
    cmd as u32 | (obj as u32) << 8 | (len as u32) << 16
}

/// What a setter writes, so a later one can replace it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Key {
    /// One whole piece of state (`which`: object type, shader stage,
    /// buffer index... per command).
    Whole(u8, u32),
    /// Slots `start..end` of a per-stage array.
    Range { cmd: u8, stage: u32, start: u32, end: u32 },
    /// Every slot from `start` on (sampler views: the slots after the
    /// last one set are cleared).
    From { cmd: u8, stage: u32, start: u32 },
}

impl Key {
    /// `self` (later) writes everything `old` wrote.
    fn covers(&self, old: &Key) -> bool {
        match (self, old) {
            (Key::Whole(a, x), Key::Whole(b, y)) => a == b && x == y,
            (Key::Range { cmd: a, stage: s, start: s2, end: e2 }, Key::Range { cmd: b, stage: t, start: s1, end: e1 }) => {
                a == b && s == t && s2 <= s1 && e1 <= e2
            }
            (Key::From { cmd: a, stage: s, start: s2 }, Key::From { cmd: b, stage: t, start: s1 }) => a == b && s == t && s2 <= s1,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Obj {
    /// Creation order.
    order: u64,
    ty: u8,
    /// The creating command(s), headers included.
    chunks: Vec<Vec<u32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Setter {
    key: Key,
    cmd: Vec<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Sub {
    id: u32,
    objects: BTreeMap<u32, Obj>,
    setters: Vec<Setter>,
    /// Queries begun and not ended.
    active: BTreeSet<u32>,
    /// Queries ended since they last began: a snapshot fetches their
    /// results first (the guest may still be waiting for one).
    ended: BTreeSet<u32>,
}

/// One context's state, as its command stream built it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlState {
    /// Sub-contexts, oldest first. Sub-context 0 always exists.
    subs: Vec<Sub>,
    current: u32,
    next_order: u64,
    /// Live objects and kept words, counted as they come and go.
    #[serde(default)]
    objects: usize,
    words: usize,
    /// Why a snapshot can't rebuild this context (None: it can).
    broken: Option<String>,
    /// What the context said each blob it shows holds (format, size:
    /// PIPE_RESOURCE_SET_TYPE), by resource: Vulkan windows a compositor
    /// imports. Made again before any object that uses them.
    #[serde(default)]
    set_types: BTreeMap<u32, Vec<u32>>,
}

impl Default for GlState {
    fn default() -> Self {
        GlState { subs: vec![Sub::default()], current: 0, next_order: 0, objects: 0, words: 0, broken: None, set_types: BTreeMap::new() }
    }
}

impl GlState {
    /// Why this context can't be saved, if it can't.
    pub fn broken(&self) -> Option<&str> {
        self.broken.as_deref()
    }

    fn give_up(&mut self, why: String) {
        if self.broken.is_none() {
            log::debug!("gpu: GL state not tracked any more: {why}");
            self.broken = Some(why);
            // Nothing kept is of use now.
            self.subs = vec![Sub::default()];
            self.set_types.clear();
            self.objects = 0;
            self.words = 0;
        }
    }

    fn sub(&mut self) -> &mut Sub {
        let cur = self.current;
        let i = match self.subs.iter().position(|s| s.id == cur) {
            Some(i) => i,
            None => {
                self.subs.push(Sub { id: cur, ..Default::default() });
                self.subs.len() - 1
            }
        };
        &mut self.subs[i]
    }

    /// Follow one submitted buffer (whole dwords). Call only for buffers
    /// the renderer accepted.
    pub fn follow(&mut self, buf: &[u32]) {
        let mut i = 0;
        while i < buf.len() && self.broken.is_none() {
            let h = buf[i];
            let len = (h >> 16) as usize;
            let Some(cmd) = buf.get(i..=i + len) else {
                self.give_up("command runs past the buffer".into());
                return;
            };
            self.command(cmd);
            i += len + 1;
        }
    }

    fn command(&mut self, c: &[u32]) {
        let (cmd, obj) = ((c[0] & 0xff) as u8, ((c[0] >> 8) & 0xff) as u8);
        let len = c.len() - 1;
        let at = |n: usize| c.get(n).copied().unwrap_or(0);
        let key = match cmd {
            NOP | CLEAR | DRAW_VBO | RESOURCE_INLINE_WRITE | BLIT | RESOURCE_COPY_REGION | GET_QUERY_RESULT | MEMORY_BARRIER
            | LAUNCH_GRID | TEXTURE_BARRIER | GET_QUERY_RESULT_QBO | TRANSFER3D | END_TRANSFERS | COPY_TRANSFER3D | CLEAR_TEXTURE
            | GET_MEMORY_INFO | SEND_STRING_MARKER | SET_DEBUG_FLAGS | LINK_SHADER => return,
            CREATE_OBJECT => return self.create(obj, c),
            DESTROY_OBJECT => {
                let s = self.sub();
                let gone = s.objects.remove(&at(1)).map(|o| obj_words(&o));
                s.active.remove(&at(1));
                s.ended.remove(&at(1));
                if let Some(w) = gone {
                    self.objects -= 1;
                    self.words -= w;
                }
                return;
            }
            BEGIN_QUERY => {
                let s = self.sub();
                s.active.insert(at(1));
                s.ended.remove(&at(1));
                return;
            }
            END_QUERY => {
                let s = self.sub();
                if s.active.remove(&at(1)) {
                    s.ended.insert(at(1));
                }
                return;
            }
            CREATE_SUB_CTX => {
                let id = at(1);
                if !self.subs.iter().any(|s| s.id == id) {
                    self.subs.push(Sub { id, ..Default::default() });
                }
                return;
            }
            DESTROY_SUB_CTX => {
                let id = at(1);
                if let Some(i) = self.subs.iter().position(|s| s.id == id && id != 0) {
                    let gone = self.subs.remove(i);
                    self.objects -= gone.objects.len();
                    self.words -= gone.objects.values().map(obj_words).sum::<usize>() + gone.setters.iter().map(|x| x.cmd.len()).sum::<usize>();
                }
                return;
            }
            SET_SUB_CTX => {
                self.current = at(1);
                return;
            }
            PIPE_RESOURCE_SET_TYPE => {
                if self.set_types.len() >= MAX_OBJECTS {
                    return self.give_up("too many blob types".into());
                }
                let old = self.set_types.insert(at(1), c.to_vec()).map_or(0, |o| o.len());
                self.words = self.words - old + c.len();
                return self.check_limits();
            }
            BIND_OBJECT => Key::Whole(cmd, obj as u32),
            BIND_SHADER => Key::Whole(cmd, at(2)),
            SET_VERTEX_BUFFERS | SET_INDEX_BUFFER | SET_STENCIL_REF | SET_BLEND_COLOR | SET_POLYGON_STIPPLE | SET_CLIP_STATE
            | SET_SAMPLE_MASK | SET_STREAMOUT_TARGETS | SET_RENDER_CONDITION | SET_TESS_STATE | SET_MIN_SAMPLES => Key::Whole(cmd, 0),
            // Both set the whole framebuffer.
            SET_FRAMEBUFFER_STATE | SET_FRAMEBUFFER_STATE_NO_ATTACH => Key::Whole(SET_FRAMEBUFFER_STATE, 0),
            SET_CONSTANT_BUFFER | SET_UNIFORM_BUFFER => Key::Whole(cmd, at(1) << 8 | (at(2) & 0xff)),
            SET_TWEAKS => Key::Whole(cmd, at(1)),
            SET_VIEWPORT_STATE => range(cmd, 0, at(1), len.saturating_sub(1) / 6),
            SET_SCISSOR_STATE => range(cmd, 0, at(1), len.saturating_sub(1) / 2),
            SET_SAMPLER_VIEWS => Key::From { cmd, stage: at(1), start: at(2) },
            BIND_SAMPLER_STATES => range(cmd, at(1), at(2), len.saturating_sub(2)),
            SET_SHADER_BUFFERS => range(cmd, at(1), at(2), len.saturating_sub(2) / 3),
            SET_SHADER_IMAGES => range(cmd, at(1), at(2), len.saturating_sub(2) / 5),
            SET_ATOMIC_BUFFERS => range(cmd, 0, at(1), len.saturating_sub(1) / 3),
            other => return self.give_up(format!("command {other} isn't tracked")),
        };
        let s = self.sub();
        let mut freed = 0;
        s.setters.retain(|old| {
            let keep = !key.covers(&old.key);
            if !keep {
                freed += old.cmd.len();
            }
            keep
        });
        s.setters.push(Setter { key, cmd: c.to_vec() });
        self.words = self.words - freed + c.len();
        self.check_limits();
    }

    fn create(&mut self, ty: u8, c: &[u32]) {
        let handle = c.get(1).copied().unwrap_or(0);
        let order = self.next_order;
        self.next_order += 1;
        let s = self.sub();
        if ty == OBJ_SHADER && c.get(3).is_some_and(|o| o & SHADER_OFFSET_CONT != 0) {
            match s.objects.get_mut(&handle) {
                Some(o) if o.ty == OBJ_SHADER => o.chunks.push(c.to_vec()),
                _ => return self.give_up("shader continuation without a start".into()),
            }
        } else {
            let old = s.objects.insert(handle, Obj { order, ty, chunks: vec![c.to_vec()] });
            if ty == OBJ_QUERY {
                s.active.remove(&handle);
                s.ended.remove(&handle);
            }
            match old {
                Some(o) => self.words -= obj_words(&o),
                None => self.objects += 1,
            }
        }
        self.words += c.len();
        self.check_limits();
    }

    fn check_limits(&mut self) {
        if self.objects > MAX_OBJECTS || self.words > MAX_WORDS {
            self.give_up(format!("{} objects, {} MiB of state", self.objects, (self.words * 4) >> 20));
        }
    }

    /// Commands that fetch the results of ended queries into their
    /// buffers (before a snapshot reads resources back), then put the
    /// current sub-context back.
    pub fn fetch_query_results(&self) -> Vec<u32> {
        let mut out = vec![];
        for s in &self.subs {
            if s.ended.is_empty() {
                continue;
            }
            out.extend([header(SET_SUB_CTX, 0, 1), s.id]);
            for &q in &s.ended {
                // Wait for it: we read the results back right after.
                out.extend([header(GET_QUERY_RESULT, 0, 2), q, 1]);
            }
        }
        if !out.is_empty() {
            out.extend([header(SET_SUB_CTX, 0, 1), self.current]);
        }
        out
    }

    /// The command stream that builds this state in a fresh context.
    /// `resource(handle)`: whether that resource exists there; references
    /// to ones that don't (or to objects that are gone) become 0.
    pub fn replay(&self, resource: &dyn Fn(u32) -> bool) -> Vec<u32> {
        let mut out = vec![];
        for (&r, c) in &self.set_types {
            if resource(r) {
                out.extend_from_slice(c);
            }
        }
        for s in &self.subs {
            if s.id != 0 {
                out.extend([header(CREATE_SUB_CTX, 0, 1), s.id]);
            }
            out.extend([header(SET_SUB_CTX, 0, 1), s.id]);
            let mut objs: Vec<(&u32, &Obj)> = s.objects.iter().collect();
            objs.sort_by_key(|(_, o)| o.order);
            let mut made = BTreeSet::new();
            for (&h, o) in objs {
                // An object on a resource we don't have would fail to
                // create, and take the context down with it.
                let res_at = match o.ty {
                    OBJ_SAMPLER_VIEW | OBJ_SURFACE | OBJ_MSAA_SURFACE | OBJ_STREAMOUT_TARGET => Some(2),
                    OBJ_QUERY => Some(4),
                    _ => None,
                };
                if res_at.is_some_and(|i| o.chunks[0].get(i).is_some_and(|&r| r != 0 && !resource(r))) {
                    continue;
                }
                for c in &o.chunks {
                    out.extend_from_slice(c);
                }
                made.insert(h);
            }
            for st in &s.setters {
                let mut c = st.cmd.clone();
                fix_refs(&mut c, &|h| made.contains(&h), resource);
                out.extend(c);
            }
            for &q in s.active.iter().filter(|q| made.contains(q)) {
                out.extend([header(BEGIN_QUERY, 0, 1), q]);
            }
        }
        out.extend([header(SET_SUB_CTX, 0, 1), self.current]);
        out
    }
}

fn obj_words(o: &Obj) -> usize {
    o.chunks.iter().map(Vec::len).sum()
}

fn range(cmd: u8, stage: u32, start: u32, n: usize) -> Key {
    Key::Range { cmd, stage, start, end: start.saturating_add(n as u32) }
}

/// Zero handles in a setter that point at objects (`obj`) or resources
/// (`res`) a fresh context won't have.
fn fix_refs(c: &mut [u32], obj: &dyn Fn(u32) -> bool, res: &dyn Fn(u32) -> bool) {
    let cmd = (c[0] & 0xff) as u8;
    let len = c.len();
    let (objs, ress): (Vec<usize>, Vec<usize>) = match cmd {
        BIND_OBJECT | BIND_SHADER | SET_RENDER_CONDITION => (vec![1], vec![]),
        SET_SAMPLER_VIEWS | BIND_SAMPLER_STATES => ((3..len).collect(), vec![]),
        SET_FRAMEBUFFER_STATE => ((2..len).collect(), vec![]),
        SET_STREAMOUT_TARGETS => ((2..len).collect(), vec![]),
        SET_VERTEX_BUFFERS => (vec![], (3..len).step_by(3).collect()),
        SET_INDEX_BUFFER => (vec![], vec![1]),
        SET_UNIFORM_BUFFER => (vec![], vec![5]),
        SET_SHADER_BUFFERS => (vec![], (5..len).step_by(3).collect()),
        SET_SHADER_IMAGES => (vec![], (7..len).step_by(5).collect()),
        SET_ATOMIC_BUFFERS => (vec![], (4..len).step_by(3).collect()),
        _ => (vec![], vec![]),
    };
    // Short commands (the renderer refuses them anyway) have fewer words.
    for i in objs.into_iter().filter(|&i| i < len) {
        if c[i] != 0 && !obj(c[i]) {
            c[i] = 0;
        }
    }
    for i in ress.into_iter().filter(|&i| i < len) {
        if c[i] != 0 && !res(c[i]) {
            c[i] = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(cmd: u8, obj: u8, args: &[u32]) -> Vec<u32> {
        let mut v = vec![header(cmd, obj, args.len())];
        v.extend_from_slice(args);
        v
    }

    /// The commands of `stream`, split at headers.
    fn cmds(stream: &[u32]) -> Vec<Vec<u32>> {
        let mut out = vec![];
        let mut i = 0;
        while i < stream.len() {
            let n = (stream[i] >> 16) as usize + 1;
            out.push(stream[i..i + n].to_vec());
            i += n;
        }
        out
    }

    #[test]
    fn blob_types_come_first() {
        let mut s = GlState::default();
        let mut buf = vec![];
        buf.extend(cmd(CREATE_OBJECT, OBJ_SAMPLER_VIEW, &[20, 5, 0, 0, 0, 0]));
        buf.extend(cmd(PIPE_RESOURCE_SET_TYPE, 0, &[5, 1, 2, 640, 480, 0, 0, 0]));
        buf.extend(cmd(PIPE_RESOURCE_SET_TYPE, 0, &[6, 1, 2, 64, 64, 0, 0, 0]));
        buf.extend(cmd(PIPE_RESOURCE_SET_TYPE, 0, &[5, 1, 2, 800, 600, 0, 0, 0]));
        s.follow(&buf);
        assert!(s.broken().is_none());
        // The latest type of each blob still there, before the view on it.
        let out = cmds(&s.replay(&|r| r == 5));
        assert_eq!(out[0], cmd(PIPE_RESOURCE_SET_TYPE, 0, &[5, 1, 2, 800, 600, 0, 0, 0]));
        assert_eq!(out[2], cmd(CREATE_OBJECT, OBJ_SAMPLER_VIEW, &[20, 5, 0, 0, 0, 0]));
    }

    #[test]
    fn keeps_objects_and_latest_setters() {
        let mut s = GlState::default();
        let mut buf = vec![];
        buf.extend(cmd(CREATE_SUB_CTX, 0, &[7]));
        buf.extend(cmd(SET_SUB_CTX, 0, &[7]));
        buf.extend(cmd(CREATE_OBJECT, 1, &[10, 0, 0, 0])); // blend 10
        buf.extend(cmd(CREATE_OBJECT, 1, &[11, 0, 0, 0])); // blend 11
        buf.extend(cmd(BIND_OBJECT, 1, &[10]));
        buf.extend(cmd(DRAW_VBO, 0, &[0; 12]));
        buf.extend(cmd(BIND_OBJECT, 1, &[11]));
        buf.extend(cmd(DESTROY_OBJECT, 1, &[10]));
        buf.extend(cmd(SET_VIEWPORT_STATE, 0, &[0, 1, 2, 3, 4, 5, 6]));
        buf.extend(cmd(SET_VIEWPORT_STATE, 0, &[0, 9, 9, 9, 9, 9, 9]));
        s.follow(&buf);
        assert!(s.broken().is_none());
        let out = cmds(&s.replay(&|_| true));
        assert_eq!(
            out,
            [
                cmd(SET_SUB_CTX, 0, &[0]),
                cmd(CREATE_SUB_CTX, 0, &[7]),
                cmd(SET_SUB_CTX, 0, &[7]),
                cmd(CREATE_OBJECT, 1, &[11, 0, 0, 0]),
                cmd(BIND_OBJECT, 1, &[11]),
                cmd(SET_VIEWPORT_STATE, 0, &[0, 9, 9, 9, 9, 9, 9]),
                cmd(SET_SUB_CTX, 0, &[7]),
            ]
        );
    }

    #[test]
    fn partial_overwrites_stay() {
        let mut s = GlState::default();
        let mut buf = vec![];
        // Sampler states 0..4, then 1..2: the first one still sets 0, 2, 3.
        buf.extend(cmd(BIND_SAMPLER_STATES, 0, &[1, 0, 5, 6, 7, 8]));
        buf.extend(cmd(BIND_SAMPLER_STATES, 0, &[1, 1, 9]));
        // Views from slot 2, then from slot 0: the second clears the rest.
        buf.extend(cmd(SET_SAMPLER_VIEWS, 0, &[1, 2, 3]));
        buf.extend(cmd(SET_SAMPLER_VIEWS, 0, &[1, 0, 4]));
        s.follow(&buf);
        let out = cmds(&s.replay(&|_| true));
        assert_eq!(out.len(), 1 + 3 + 1);
        assert_eq!(out[1], cmd(BIND_SAMPLER_STATES, 0, &[1, 0, 0, 0, 0, 0]), "objects 5..8 are gone: zeroed");
        assert_eq!(out[3], cmd(SET_SAMPLER_VIEWS, 0, &[1, 0, 0]));
    }

    #[test]
    fn shaders_in_pieces_and_missing_resources() {
        let mut s = GlState::default();
        let mut buf = vec![];
        buf.extend(cmd(CREATE_OBJECT, OBJ_SHADER, &[3, 1, 100, 50, 0, 0xaa]));
        buf.extend(cmd(CREATE_OBJECT, OBJ_SHADER, &[3, 1, SHADER_OFFSET_CONT | 4, 0xbb]));
        // A view on resource 42, a view on resource 43 (gone at replay).
        buf.extend(cmd(CREATE_OBJECT, OBJ_SAMPLER_VIEW, &[20, 42, 0, 0, 0, 0]));
        buf.extend(cmd(CREATE_OBJECT, OBJ_SAMPLER_VIEW, &[21, 43, 0, 0, 0, 0]));
        buf.extend(cmd(SET_SAMPLER_VIEWS, 0, &[1, 0, 20, 21]));
        buf.extend(cmd(SET_VERTEX_BUFFERS, 0, &[16, 0, 42, 16, 0, 43]));
        buf.extend(cmd(BIND_SHADER, 0, &[3, 1]));
        s.follow(&buf);
        let out = cmds(&s.replay(&|r| r == 42));
        assert_eq!(&out[1..3], &[cmd(CREATE_OBJECT, OBJ_SHADER, &[3, 1, 100, 50, 0, 0xaa]), cmd(CREATE_OBJECT, OBJ_SHADER, &[3, 1, SHADER_OFFSET_CONT | 4, 0xbb])]);
        assert_eq!(out[3], cmd(CREATE_OBJECT, OBJ_SAMPLER_VIEW, &[20, 42, 0, 0, 0, 0]));
        assert_eq!(out[4], cmd(SET_SAMPLER_VIEWS, 0, &[1, 0, 20, 0]));
        assert_eq!(out[5], cmd(SET_VERTEX_BUFFERS, 0, &[16, 0, 42, 16, 0, 0]));
        assert_eq!(out[6], cmd(BIND_SHADER, 0, &[3, 1]));
    }

    #[test]
    fn queries() {
        let mut s = GlState::default();
        let mut buf = vec![];
        buf.extend(cmd(CREATE_OBJECT, OBJ_QUERY, &[5, 0, 0, 42]));
        buf.extend(cmd(CREATE_OBJECT, OBJ_QUERY, &[6, 0, 0, 42]));
        buf.extend(cmd(BEGIN_QUERY, 0, &[5]));
        buf.extend(cmd(BEGIN_QUERY, 0, &[6]));
        buf.extend(cmd(END_QUERY, 0, &[6]));
        s.follow(&buf);
        assert_eq!(cmds(&s.fetch_query_results()), [cmd(SET_SUB_CTX, 0, &[0]), cmd(GET_QUERY_RESULT, 0, &[6, 1]), cmd(SET_SUB_CTX, 0, &[0])]);
        let out = cmds(&s.replay(&|_| true));
        assert_eq!(out[out.len() - 2], cmd(BEGIN_QUERY, 0, &[5]));
    }

    /// The running counts match counting from scratch.
    fn counted(s: &GlState) -> (usize, usize) {
        let objects = s.subs.iter().map(|x| x.objects.len()).sum();
        let words = s.subs.iter().map(|x| x.objects.values().map(obj_words).sum::<usize>() + x.setters.iter().map(|y| y.cmd.len()).sum::<usize>()).sum();
        (objects, words)
    }

    #[test]
    fn counts_stay_right() {
        let mut s = GlState::default();
        let mut buf = vec![];
        buf.extend(cmd(CREATE_SUB_CTX, 0, &[2]));
        buf.extend(cmd(SET_SUB_CTX, 0, &[2]));
        buf.extend(cmd(CREATE_OBJECT, OBJ_SHADER, &[3, 1, 100, 50, 0, 0xaa]));
        buf.extend(cmd(CREATE_OBJECT, OBJ_SHADER, &[3, 1, SHADER_OFFSET_CONT | 4, 0xbb]));
        buf.extend(cmd(CREATE_OBJECT, 1, &[4, 0, 0]));
        buf.extend(cmd(CREATE_OBJECT, 1, &[4, 0, 0, 0, 0])); // same handle again
        buf.extend(cmd(SET_VIEWPORT_STATE, 0, &[0, 1, 2, 3, 4, 5, 6]));
        buf.extend(cmd(SET_VIEWPORT_STATE, 0, &[0, 9, 9, 9, 9, 9, 9]));
        buf.extend(cmd(DESTROY_OBJECT, 1, &[4]));
        s.follow(&buf);
        assert_eq!((s.objects, s.words), counted(&s));
        s.follow(&cmd(DESTROY_SUB_CTX, 0, &[2]));
        assert_eq!((s.objects, s.words), counted(&s));
        assert_eq!((s.objects, s.words), (0, 0));
    }

    #[test]
    fn short_commands_dont_panic() {
        // Fuzzer find: SET_INDEX_BUFFER with no words after its header.
        let mut s = GlState::default();
        s.follow(&[11]);
        s.follow(&cmd(SET_UNIFORM_BUFFER, 0, &[1, 0]));
        let _ = s.replay(&|_| false);
    }

    #[test]
    fn gives_up_on_what_it_cant_model() {
        let mut s = GlState::default();
        s.follow(&cmd(48, 0, &[1, 2, 3])); // PIPE_RESOURCE_CREATE
        assert!(s.broken().unwrap().contains("48"));
        let mut s = GlState::default();
        s.follow(&[header(DRAW_VBO, 0, 12), 0]); // runs past the end
        assert!(s.broken().is_some());
    }
}
