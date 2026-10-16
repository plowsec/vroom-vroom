//! The Mac side of a drag out of a VM window: an AppKit drag session of
//! file promises, started from the window while the mouse button is still
//! down. Finder (or the app it lands on) asks for each item where it
//! wants it; we fetch it from the guest then (dragout.rs). Nothing is
//! copied for a drag that is given up.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession, NSDraggingSource, NSEvent, NSEventType,
    NSFilePromiseProvider, NSFilePromiseProviderDelegate, NSView, NSWorkspace,
};
use objc2_foundation::{NSArray, NSError, NSOperationQueue, NSPoint, NSRect, NSSize, NSString, NSURL};

use crate::dnd::Progress;

/// What the promises need from the window, from any thread.
pub struct Job {
    pub id: u32,
    /// Ask the VMM for item `n`: the pipe it comes through.
    pub get: Box<dyn Fn(u32) -> std::io::Result<UnixStream> + Send + Sync>,
    /// Item `n` is written (where), or why not.
    pub done: Box<dyn Fn(u32, Result<PathBuf, String>) + Send + Sync>,
    pub progress: Arc<Progress>,
}

pub struct PromiseIvars {
    item: u32,
    name: String,
    dir: bool,
    job: Arc<Job>,
    queue: Retained<NSOperationQueue>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "VroomFilePromise"]
    #[ivars = PromiseIvars]
    struct Promise;

    unsafe impl NSObjectProtocol for Promise {}

    unsafe impl NSFilePromiseProviderDelegate for Promise {
        #[unsafe(method_id(filePromiseProvider:fileNameForType:))]
        fn file_name(&self, _p: &NSFilePromiseProvider, _t: &NSString) -> Retained<NSString> {
            NSString::from_str(&self.ivars().name)
        }

        #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
        fn write(&self, _p: &NSFilePromiseProvider, url: &NSURL, done: &block2::DynBlock<dyn Fn(*mut NSError)>) {
            let r = self.fetch(url);
            let job = &self.ivars().job;
            let err = r.as_ref().err().map(|why| {
                log::info!("drag {} out: {}: {why}", job.id, self.ivars().name);
                NSError::new(1, &NSString::from_str("vroom"))
            });
            (job.done)(self.ivars().item, r);
            done.call((err.as_ref().map_or(std::ptr::null_mut(), |e| Retained::as_ptr(e) as *mut NSError),));
        }

        #[unsafe(method_id(operationQueueForFilePromiseProvider:))]
        fn queue(&self, _p: &NSFilePromiseProvider) -> Retained<NSOperationQueue> {
            self.ivars().queue.clone()
        }
    }
);

impl Promise {
    /// On the promise queue: fetch the item and write it where `url` says.
    fn fetch(&self, url: &NSURL) -> Result<PathBuf, String> {
        let v = self.ivars();
        let to = url.path().map(|p| PathBuf::from(p.to_string())).ok_or("no place to write to")?;
        let dest = to.parent().ok_or("no place to write to")?;
        let mut pipe = (v.job.get)(v.item).map_err(|e| e.to_string())?;
        crate::dragout::receive(&mut pipe, v.job.id, &v.name, v.dir, dest, &v.job.progress, crate::dragout::KEEP_FREE)
    }
}

/// Called once when the drag ends: whether something took it.
type Ended = Box<dyn FnOnce(bool)>;

pub struct SourceIvars {
    ended: RefCell<Option<Ended>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "VroomDragSource"]
    #[ivars = SourceIvars]
    struct Source;

    unsafe impl NSObjectProtocol for Source {}

    unsafe impl NSDraggingSource for Source {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn mask(&self, _s: &NSDraggingSession, _c: NSDraggingContext) -> NSDragOperation {
            NSDragOperation::Copy
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, _s: &NSDraggingSession, _p: NSPoint, op: NSDragOperation) {
            let f = self.ivars().ended.borrow_mut().take();
            if let Some(f) = f {
                f(op != NSDragOperation::None);
            }
        }
    }
);

/// A drag underway: keeps its objects alive (a promise's delegate is
/// only weakly held by AppKit).
pub struct Session {
    _source: Retained<Source>,
    _promises: Vec<Retained<Promise>>,
}

/// Whether the left mouse button is down now.
pub fn button_down() -> bool {
    NSEvent::pressedMouseButtons() & 1 != 0
}

/// Start dragging `items` from `view` (the VM window's), under the mouse.
pub fn start(view: *mut std::ffi::c_void, items: &[(String, bool)], job: Arc<Job>, ended: Ended) -> Result<Session, String> {
    let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
    if view.is_null() || items.is_empty() {
        return Err("nothing to drag".into());
    }
    // SAFETY: the window's NSView, alive while its window is (we are
    // called from that window's code, on the main thread).
    let view: Retained<NSView> = unsafe { Retained::retain(view.cast::<NSView>()) }.ok_or("no view")?;
    let window = view.window().ok_or("no window")?;
    let at_window = window.mouseLocationOutsideOfEventStream();
    let at = view.convertPoint_fromView(at_window, None);

    // The event the drag starts from: the current one if it is the
    // mouse's (it is, while it moves with the button down), else one like it.
    let app = NSApplication::sharedApplication(mtm);
    let event = app
        .currentEvent()
        .filter(|e| matches!(e.r#type(), NSEventType::LeftMouseDragged | NSEventType::LeftMouseDown))
        .or_else(|| {
            // SAFETY: the plain NSEvent factory; a nil context is allowed.
            unsafe {
                let none: *const AnyObject = std::ptr::null();
                msg_send![
                    NSEvent::class(),
                    mouseEventWithType: NSEventType::LeftMouseDragged,
                    location: at_window,
                    modifierFlags: 0usize,
                    timestamp: 0.0f64,
                    windowNumber: window.windowNumber(),
                    context: none,
                    eventNumber: 0isize,
                    clickCount: 1isize,
                    pressure: 1.0f32
                ]
            }
        })
        .ok_or("no mouse event to start from")?;

    let queue = NSOperationQueue::new();
    queue.setMaxConcurrentOperationCount(1);
    let ws = NSWorkspace::sharedWorkspace();
    let mut promises = Vec::new();
    let mut drag_items = Vec::new();
    for (i, (name, dir)) in items.iter().enumerate() {
        let p = Promise::alloc().set_ivars(PromiseIvars { item: i as u32, name: name.clone(), dir: *dir, job: job.clone(), queue: queue.clone() });
        // SAFETY: NSObject's init on a freshly allocated object.
        let p: Retained<Promise> = unsafe { msg_send![super(p), init] };
        let ty = NSString::from_str(if *dir { "public.folder" } else { "public.data" });
        let provider = NSFilePromiseProvider::initWithFileType_delegate(NSFilePromiseProvider::alloc(), &ty, ProtocolObject::from_ref(&*p));
        let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), ProtocolObject::from_ref(&*provider));
        let ext = match name.rsplit_once('.') {
            _ if *dir => "public.folder".to_string(),
            Some((s, e)) if !s.is_empty() => e.to_string(),
            _ => "public.data".to_string(),
        };
        // iconForContentType wants a UTType: another framework for an icon.
        #[allow(deprecated)]
        let icon = ws.iconForFileType(&NSString::from_str(&ext));
        let k = i.min(8) as f64 * 6.0;
        let frame = NSRect::new(NSPoint::new(at.x - 24.0 + k, at.y - 24.0 - k), NSSize::new(48.0, 48.0));
        // SAFETY: an NSImage is what a dragging frame's contents may be.
        unsafe { item.setDraggingFrame_contents(frame, Some(&icon)) };
        drag_items.push(item);
        promises.push(p);
    }
    let source = Source::alloc(mtm).set_ivars(SourceIvars { ended: RefCell::new(Some(ended)) });
    // SAFETY: NSObject's init on a freshly allocated object.
    let source: Retained<Source> = unsafe { msg_send![super(source), init] };
    let session = view.beginDraggingSessionWithItems_event_source(&NSArray::from_retained_slice(&drag_items), &event, ProtocolObject::from_ref(&*source));
    session.setAnimatesToStartingPositionsOnCancelOrFail(true);
    Ok(Session { _source: source, _promises: promises })
}
