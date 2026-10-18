//! The library window: VM list, settings, snapshots, New VM, palette.
//!
//! State lives here; the UI only shows it. Every UI intent arrives as a
//! [`Msg`], gets applied (to the VM file, a running VM, the library), and
//! the changed parts are pushed back to the UI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::winit_030::winit::event::{ElementState, WindowEvent};
use slint::winit_030::winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use slint::winit_030::{EventResult, WinitWindowAccessor};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use vroom_config::template::NewVm as Template;
use vroom_config::{Boot, DiskBus, DiskSync, GuestOs, Gpu, NetKind, OpenglHost, Serial, Transport, VmConfig};

use crate::edit;
use crate::library::Library;
use crate::msg::{self, Msg, PaletteKey};
use crate::vm::{self, DiskState, Vm};
use crate::*;

const TICK: Duration = Duration::from_millis(1500);
const TOAST_FOR: Duration = Duration::from_secs(5);
const ERROR_TOAST_FOR: Duration = Duration::from_secs(12);
/// Asked this run whether to set up an old vroom-netd again.
static NETD_OLD_ASKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct ToastEntry {
    toast: Toast,
    until: Instant,
}

#[derive(Clone)]
struct PaletteEntry {
    title: String,
    hint: String,
    icon: &'static str,
    action: String,
}

pub struct App {
    ui: AppWindow,
    lib: Library,
    /// When `library.toml` last changed: another window (a VM opened by
    /// its file) may add to it.
    lib_mtime: Option<std::time::SystemTime>,
    vms: Vec<Vm>,
    current: Option<usize>,
    live_thumbs: HashMap<String, slint::Image>,
    query: String,
    host_ifaces: Vec<String>,
    vm_model: Rc<VecModel<VmItem>>,
    snap_model: Rc<VecModel<SnapRow>>,
    toasts: Vec<ToastEntry>,
    next_toast: i32,
    palette: Vec<PaletteEntry>,
    palette_sel: usize,
    /// The action a confirm dialog runs, and the VM (key) it was asked for.
    confirm: Option<(String, String)>,
    installer: Option<PathBuf>,
    /// A VMware VM read and waiting for "Import".
    vmware: Option<vroom_import::Plan>,
    /// The progress toast of the import that's running.
    import_toast: Option<i32>,
    last_settings: Option<VmSettings>,
    /// Host USB devices as listed when "Add device" was pressed, and
    /// whether that list is open.
    usb_host: Vec<vroom_usb::list::HostDevice>,
    usb_picking: bool,
    _timer: slint::Timer,
}

pub fn launch() -> Result<(), String> {
    let mut app = App::new()?;
    // An explicit size: left alone, the window follows its layout's
    // preferred size, which changes with what's on screen.
    app.ui.window().set_size(slint::LogicalSize::new(1320.0, 840.0));
    app.ui.show().map_err(|e| e.to_string())?;
    crate::shot::TARGET.with(|t| *t.borrow_mut() = Some(app.ui.clone_strong()));
    // Follow the system look unless the user picked one.
    if app.lib.theme.is_none() {
        let dark = app.ui.window().with_winit_window(|w| w.theme()).flatten().map(|t| t == slint::winit_030::winit::window::Theme::Dark);
        app.ui.global::<Theme>().set_dark(dark.unwrap_or(true));
    }
    msg::set_handler(Box::new(move |m| app.update(m)));
    Ok(())
}

fn wire(ui: &AppWindow) {
    ui.on_select(|i| msg::send(Msg::Select(i.max(0) as usize)));
    let bus = ui.global::<Bus>();
    bus.on_set(|k, v| msg::send(Msg::Set(k.into(), v.into())));
    bus.on_act(|a| msg::send(Msg::Act(a.into())));
    bus.on_search(|q| msg::send(Msg::Search(q.into())));
    bus.on_copy(|t| msg::send(Msg::Copy(t.into())));
    ui.window().on_close_requested(|| {
        // VM windows keep running; the app ends with the last window.
        slint::Timer::single_shot(Duration::ZERO, || {
            if console::count() == 0 {
                let _ = slint::quit_event_loop();
            }
        });
        slint::CloseRequestResponse::HideWindow
    });
    let mods = std::cell::Cell::new(ModifiersState::empty());
    let weak = ui.as_weak();
    ui.window().on_winit_window_event(move |_w, ev| {
        let Some(ui) = weak.upgrade() else { return EventResult::Propagate };
        match ev {
            WindowEvent::ModifiersChanged(m) => mods.set(m.state()),
            WindowEvent::HoveredFile(_) => msg::send(Msg::DropHover(true)),
            WindowEvent::HoveredFileCancelled => msg::send(Msg::DropHover(false)),
            WindowEvent::DroppedFile(p) => {
                msg::send(Msg::DropHover(false));
                msg::send(Msg::Drop(p.clone()));
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let PhysicalKey::Code(code) = event.physical_key else { return EventResult::Propagate };
                let m = mods.get();
                let cmd = if cfg!(target_os = "macos") { m.super_key() } else { m.control_key() };
                if ui.get_palette_open() {
                    let k = match code {
                        KeyCode::ArrowUp => Some(PaletteKey::Up),
                        KeyCode::ArrowDown => Some(PaletteKey::Down),
                        KeyCode::Enter | KeyCode::NumpadEnter => Some(PaletteKey::Enter),
                        KeyCode::Escape => Some(PaletteKey::Escape),
                        _ => None,
                    };
                    if let Some(k) = k {
                        msg::send(Msg::PaletteKey(k));
                        return EventResult::PreventDefault;
                    }
                }
                match code {
                    KeyCode::KeyK if cmd => {
                        msg::send(Msg::Act("palette".into()));
                        return EventResult::PreventDefault;
                    }
                    KeyCode::KeyN if cmd => {
                        msg::send(Msg::Act("newvm".into()));
                        return EventResult::PreventDefault;
                    }
                    KeyCode::Escape => {
                        if ui.get_newvm_open() {
                            msg::send(Msg::Act("newvm.cancel".into()));
                        } else if ui.get_confirm_open() {
                            msg::send(Msg::Act("confirm.cancel".into()));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        EventResult::Propagate
    });
}

pub fn os_index(os: GuestOs) -> i32 {
    GuestOs::ALL.iter().position(|o| *o == os).unwrap_or(0) as i32
}

fn s(x: impl Into<SharedString>) -> SharedString {
    x.into()
}

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}

impl App {
    fn new() -> Result<App, String> {
        let ui = AppWindow::new().map_err(|e| e.to_string())?;
        let lib = Library::load(&paths::library_file());
        let vms: Vec<Vm> = lib.vms.iter().map(|p| Vm::open(p)).collect();
        let vm_model = Rc::new(VecModel::default());
        let snap_model = Rc::new(VecModel::default());
        ui.set_vms(ModelRc::from(vm_model.clone()));
        ui.set_snaps(ModelRc::from(snap_model.clone()));
        wire(&ui);
        let timer = slint::Timer::default();
        timer.start(slint::TimerMode::Repeated, TICK, || msg::send(Msg::Tick));
        let lib_mtime = std::fs::metadata(paths::library_file()).and_then(|m| m.modified()).ok();
        let mut app = App {
            lib_mtime,
            ui,
            current: (!vms.is_empty()).then_some(0),
            vms,
            live_thumbs: HashMap::new(),
            query: String::new(),
            host_ifaces: host::interfaces(),
            vm_model,
            snap_model,
            toasts: Vec::new(),
            next_toast: 1,
            palette: Vec::new(),
            palette_sel: 0,
            confirm: None,
            installer: None,
            vmware: None,
            import_toast: None,
            last_settings: None,
            usb_host: Vec::new(),
            usb_picking: false,
            _timer: timer,
            lib,
        };
        if let Some(e) = app.lib.error.clone() {
            app.toast(format!("Couldn't read the VM list: {e}"), 3);
        }
        app.apply_theme();
        app.push_host();
        // VMs whose windows went with a crashed app: show them again.
        for i in 0..app.vms.len() {
            app.attach(i, false);
        }
        app.push_all();
        Ok(app)
    }

    fn update(&mut self, m: Msg) {
        match m {
            Msg::Select(i) => {
                if i < self.vms.len() {
                    self.current = Some(i);
                    self.last_settings = None;
                    self.push_all();
                }
            }
            Msg::Set(k, v) => self.set(&k, &v),
            Msg::Act(a) => self.act(&a),
            Msg::Search(q) => {
                if let Some(q) = q.strip_prefix("palette:") {
                    self.palette_sel = 0;
                    self.build_palette(q);
                } else {
                    self.query = q;
                    self.push_vis();
                }
            }
            Msg::Copy(t) => {
                if let Ok(mut c) = arboard::Clipboard::new() {
                    let _ = c.set_text(t);
                    self.toast("Copied", 1);
                }
            }
            Msg::Drop(p) => self.dropped(&p),
            Msg::Open(p) => self.opened(&p),
            Msg::DropHover(b) => self.ui.set_drop_hover(b),
            Msg::PaletteKey(k) => self.palette_key(k),
            Msg::Tick => self.tick(),
            Msg::ConsoleState { .. } => self.push_all(),
            Msg::Thumb { key, image } => {
                self.live_thumbs.insert(key, image);
                self.push_vms();
            }
            Msg::SnapshotTaken { .. } => self.push_snaps(),
            Msg::ConsoleGone { key, error } => {
                // Library closed earlier and that was the last VM window.
                if console::count() == 0 && !self.ui.window().is_visible() {
                    let _ = slint::quit_event_loop();
                    return;
                }
                self.live_thumbs.remove(&key);
                if let Some(v) = self.vms.iter_mut().find(|v| v.key == key) {
                    v.refresh_state();
                    v.refresh_thumb();
                }
                if let Some(e) = error {
                    self.toasts.push(ToastEntry {
                        toast: Toast { id: self.next_toast, text: s(e), kind: 3, action: s(format!("log:{key}")) },
                        until: Instant::now() + ERROR_TOAST_FOR,
                    });
                    self.next_toast += 1;
                    self.push_toasts();
                }
                self.push_all();
            }
            Msg::ShowLibrary(key) => {
                // From a VM window's settings button: its page, where the
                // settings are.
                if let Some(i) = key.and_then(|k| self.vms.iter().position(|v| v.key == k)) {
                    self.update(Msg::Select(i));
                }
                let _ = self.ui.show();
                self.ui.window().with_winit_window(|w| w.focus_window());
            }
            Msg::Toast { text, kind, action } => {
                self.toasts.push(ToastEntry {
                    toast: Toast { id: self.next_toast, text: s(text), kind, action: s(action) },
                    until: Instant::now() + if kind == 3 { ERROR_TOAST_FOR } else { TOAST_FOR },
                });
                self.next_toast += 1;
                self.push_toasts();
            }
            Msg::Refresh => {
                for v in &mut self.vms {
                    v.refresh_state();
                }
                self.last_settings = None;
                self.push_all();
            }
            Msg::ImportProgress(pct) => {
                if let Some(t) = self.toasts.iter_mut().find(|t| Some(t.toast.id) == self.import_toast) {
                    t.toast.text = s(format!("Importing from VMware: {pct}%"));
                    t.until = Instant::now() + ERROR_TOAST_FOR;
                    self.push_toasts();
                }
            }
            Msg::Imported(r) => {
                let id = self.import_toast.take();
                self.toasts.retain(|t| Some(t.toast.id) != id);
                self.push_toasts();
                match r {
                    Ok(p) => {
                        self.add_vm(&p);
                        self.toast("Imported. Check the notes in the guide before the first start (Windows needs the tools).", 1);
                    }
                    Err(e) => self.toast(format!("Import failed: {e}"), 3),
                }
            }
            Msg::NewVmProgress(t) => self.ui.set_newvm_progress(s(t)),
            Msg::Created(r, start) => {
                self.ui.set_newvm_creating(false);
                self.ui.set_newvm_progress(s(""));
                match r {
                    Ok(p) => {
                        self.ui.set_newvm_open(false);
                        self.add_vm(&p);
                        self.toast("VM created", 1);
                        if start {
                            self.start_current(&[]);
                        }
                    }
                    Err(e) => self.ui.set_newvm_error(s(e)),
                }
            }
        }
    }

    fn toast(&mut self, text: impl Into<String>, kind: i32) {
        self.update(Msg::Toast { text: text.into(), kind, action: String::new() });
    }

    fn cur(&self) -> Option<&Vm> {
        self.current.and_then(|i| self.vms.get(i))
    }

    fn state_of(&self, v: &Vm) -> VmState {
        if let Some(s) = console::state(&v.key) {
            return s;
        }
        match v.disk_state {
            DiskState::Off => VmState::Off,
            DiskState::Suspended => VmState::Suspended,
            DiskState::RunningElsewhere => VmState::Running,
        }
    }

    // ---- pushing state to the UI ----

    fn push_all(&mut self) {
        if let Some(i) = self.current {
            if i >= self.vms.len() {
                self.current = (!self.vms.is_empty()).then_some(self.vms.len() - 1);
            }
        }
        self.ui.set_current(self.current.map_or(-1, |i| i as i32));
        self.push_vms();
        self.push_settings();
        self.push_snaps();
        self.push_vis();
    }

    fn push_host(&self) {
        let free = host::free_bytes(&paths::data_dir()).or_else(|| host::free_bytes(Path::new("/")));
        self.ui.set_host(HostInfo {
            cpus: host::cpus() as i32,
            memory_gib: (host::memory_bytes() >> 30) as i32,
            interfaces: model(self.host_ifaces.iter().map(s).collect()),
            free: s(free.map(|f| format!("{} free on this machine", fmt::bytes(f))).unwrap_or_default()),
            netd: s(rootd::state().word()),
            netd_busy: rootd::busy(),
        });
    }

    /// Set up (`true`) or remove vroom-netd; macOS asks for the password.
    fn netd(&mut self, install: bool) {
        if rootd::busy() {
            return;
        }
        let f: fn() -> Result<(), String> = if install { rootd::install } else { rootd::uninstall };
        rootd::in_background(f, move |r| {
            let (text, kind) = match r {
                Ok(()) if install => ("vroom-netd is set up: bridged and host-only networks and USB devices macOS uses work now".to_string(), 1),
                Ok(()) => ("vroom-netd is removed".to_string(), 1),
                Err(e) if e == "cancelled" => return msg::send(Msg::Refresh),
                Err(e) => (format!("vroom-netd: {e}"), 3),
            };
            msg::send(Msg::Toast { text, kind, action: String::new() });
            msg::send(Msg::Refresh);
        });
        self.push_host();
    }

    fn item(&self, i: usize, v: &Vm) -> VmItem {
        let state = self.state_of(v);
        let (os, specs, warnings) = match &v.raw {
            Some(c) => (
                os_id(c.guest_os()),
                format!("{} CPU{} · {} GB", c.cpus, if c.cpus == 1 { "" } else { "s" }, c.memory_mib.div_ceil(1024)),
                v.resolved().map(|r| r.warnings().len()).unwrap_or(0),
            ),
            None => ("other", "File has an error".to_string(), 1),
        };
        let thumb = self.live_thumbs.get(&v.key).cloned().or_else(|| v.thumb.clone());
        VmItem {
            id: i as i32,
            name: s(v.name()),
            os: s(os),
            state,
            specs: s(specs),
            has_thumb: thumb.is_some(),
            thumb: thumb.unwrap_or_default(),
            // Its window is here, or can come back here (the app crashed).
            ours: console::is_running(&v.key)
                || (v.disk_state == DiskState::RunningElsewhere && console::viewer_socket(&v.state_dir()).is_some()),
            warnings: warnings as i32,
        }
    }

    fn push_vms(&mut self) {
        let items: Vec<VmItem> = self.vms.iter().enumerate().map(|(i, v)| self.item(i, v)).collect();
        if items.len() != self.vm_model.row_count() {
            self.vm_model.set_vec(items);
            return;
        }
        for (i, it) in items.into_iter().enumerate() {
            if self.vm_model.row_data(i).as_ref() != Some(&it) {
                self.vm_model.set_row_data(i, it);
            }
        }
    }

    fn push_snaps(&mut self) {
        let rows: Vec<SnapRow> = match self.cur() {
            Some(v) => {
                let dir = paths::thumbs_dir(&v.key);
                vroom_state::list(&v.state_dir())
                    .unwrap_or_default()
                    .into_iter()
                    .rev()
                    // Dir names the VMM picked: only ones we'd use in a file name.
                    .filter(|sn| vroom_config::valid_name(&sn.name))
                    .map(|sn| {
                        let tp = dir.join(format!("snap-{}.png", sn.name));
                        let thumb = tp.is_file().then(|| slint::Image::load_from_path(&tp).ok()).flatten();
                        SnapRow {
                            name: s(&sn.name),
                            when: s(fmt::ago(sn.created_unix)),
                            size: s(format!("{} of memory and devices", fmt::bytes(sn.bytes))),
                            has_thumb: thumb.is_some(),
                            thumb: thumb.unwrap_or_default(),
                        }
                    })
                    .collect()
            }
            None => Vec::new(),
        };
        let same = rows.len() == self.snap_model.row_count()
            && rows.iter().enumerate().all(|(i, r)| self.snap_model.row_data(i).is_some_and(|o| o.name == r.name && o.when == r.when));
        if !same {
            self.snap_model.set_vec(rows);
        }
    }

    fn push_settings(&mut self) {
        let st = self.settings();
        if self.last_settings.as_ref() != Some(&st) {
            self.ui.set_settings(st.clone());
            self.last_settings = Some(st);
        }
    }

    fn settings(&self) -> VmSettings {
        let Some(v) = self.cur() else { return VmSettings::default() };
        let state = self.state_of(v);
        let Some(c) = &v.raw else {
            return VmSettings {
                loaded: true,
                name: s(v.name()),
                warnings: model(vec![s(format!("This VM file has an error: {}", v.error.clone().unwrap_or_default()))]),
                locked: true,
                lock_reason: s("Fix the file to change settings here."),
                config_path: s(paths::tilde(&v.path)),
                ..Default::default()
            };
        };
        let r = v.resolved().unwrap_or_else(|| c.clone());
        let (locked, lock_short, lock_reason, lock_why) = match state {
            VmState::Off => (false, "", "", ""),
            VmState::Suspended => (
                true,
                "Suspended: hardware is locked",
                "Suspended. Network and USB changes apply when it resumes. Other hardware changes after a fresh boot: resume and shut down, or discard the saved state (⋯ menu).",
                "The VM was saved with this hardware: it changes after a fresh boot.",
            ),
            _ if !console::is_running(&v.key) && console::viewer_socket(&v.state_dir()).is_some() => (
                true,
                "Running without a window",
                "Running without a window. Show brings it back; shut it down to change hardware.",
                "Can't change while the VM runs without a window.",
            ),
            _ if !console::is_running(&v.key) => {
                (
                    true,
                    "Running from a terminal",
                    "Running from a terminal (vroom run). Stop it there to change settings.",
                    "Can't change while the VM runs from a terminal.",
                )
            }
            _ => (
                true,
                "Running: hardware is locked",
                "Running. Network, USB devices, shared folders and the clipboard change right away; the rest needs the VM shut down.",
                "The guest found this hardware when it started: shut the VM down to change it.",
            ),
        };
        let live = self.live_now(v);
        let (net_notes, net_note) = self.net_notes(v, c, state);

        let disks: Vec<DiskRow> = c
            .disks
            .iter()
            .zip(&r.disks)
            .map(|(d, rd)| {
                let missing = !rd.path.exists();
                let size = vm::virtual_size(&rd.path);
                let used = vm::used_bytes(&rd.path);
                let qcow2 = is_qcow2(&rd.path);
                DiskRow {
                    file: s(d.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
                    path: s(paths::tilde(&rd.path)),
                    size: s(size.map(fmt::bytes).unwrap_or_default()),
                    used: s(if qcow2 { used.map(fmt::bytes).unwrap_or_default() } else { String::new() }),
                    bus: (d.bus == DiskBus::Nvme) as i32,
                    sync: match d.sync {
                        DiskSync::Full => 0,
                        DiskSync::Fsync => 1,
                        DiskSync::None => 2,
                    },
                    readonly: d.readonly,
                    cdrom: d.cdrom,
                    missing,
                    qcow2,
                }
            })
            .collect();
        let nets: Vec<NetRow> = c
            .nets
            .iter()
            .zip(net_notes)
            .map(|(n, note)| NetRow {
                mode: match n.mode {
                    NetKind::Nat => 0,
                    NetKind::Bridged => 1,
                    NetKind::HostOnly => 2,
                },
                interface: s(n.interface.clone().unwrap_or_default()),
                iface_index: n.interface.as_ref().and_then(|i| self.host_ifaces.iter().position(|h| h == i)).map_or(-1, |p| p as i32),
                network: s(n.network.clone().unwrap_or_else(|| vroom_config::DEFAULT_HOST_ONLY.into())),
                forwards: model(
                    n.port_forwards
                        .iter()
                        .map(|p| ForwardRow {
                            host: s(p.host_port.to_string()),
                            guest: s(p.guest_port.to_string()),
                            lan: p.host_addr != "127.0.0.1" && p.host_addr != "::1",
                        })
                        .collect(),
                ),
                loopback: n.allow_host_loopback,
                block_private: n.block_private,
                mac: s(n.mac.clone().unwrap_or_default()),
                summary: s(net_summary(n)),
                connected: n.connected,
                note: s(note),
            })
            .collect();
        let shares: Vec<ShareRow> = c
            .shares
            .iter()
            .zip(&r.shares)
            .map(|(sh, rs)| ShareRow {
                path: s(rs.path.display().to_string()),
                short: s(paths::tilde(&rs.path)),
                tag: s(&sh.tag),
                readonly: sh.readonly,
            })
            .collect();
        let usb = usb_rows(c, console::usb_now(&v.key));
        let tools = if state == VmState::Off || state == VmState::Suspended {
            Default::default()
        } else {
            console::tools_info(&v.key).unwrap_or_default()
        };
        let usb_host: Vec<UsbHostRow> = self
            .usb_host
            .iter()
            .map(|d| {
                let mut note = d.id();
                if let Some(sn) = &d.serial {
                    note += &format!(" · serial {sn}");
                }
                if !d.claimed_by.is_empty() {
                    note += &format!(" · held by {}", d.claimed_by.join(", "));
                } else if d.is_host_essential() {
                    note += " · keyboard, mouse, disk or hub";
                }
                let taken = c.usb.iter().any(|u| u.ids() == Some((d.vendor, d.product)) && (u.serial.is_none() || u.serial == d.serial));
                UsbHostRow { label: s(d.label()), note: s(note), taken }
            })
            .collect();
        let warnings: Vec<SharedString> = r.warnings().into_iter().map(|w| s(capitalize(&w))).collect();
        let warn_short: Vec<SharedString> = r.warnings().iter().map(|w| s(warn_short(w))).collect();

        let (boot_kernel, firmware, vars, kernel, initrd, cmdline) = match &c.boot {
            Boot::Uefi { firmware, vars } => (
                false,
                firmware.as_ref().map(|f| f.display().to_string()).unwrap_or_default(),
                vars.display().to_string(),
                String::new(),
                String::new(),
                String::new(),
            ),
            Boot::Kernel { kernel, initrd, cmdline } => (
                true,
                String::new(),
                String::new(),
                kernel.display().to_string(),
                initrd.as_ref().map(|i| i.display().to_string()).unwrap_or_default(),
                cmdline.clone(),
            ),
        };
        let mem_gib = c.memory_mib.div_ceil(1024);
        let gpu = match c.display.gpu {
            Gpu::None => 0,
            Gpu::Virtio2d => 1,
            Gpu::Virtio3d => 2,
        };
        let total: u64 = r.disks.iter().filter(|d| !d.cdrom && !d.readonly).filter_map(|d| vm::virtual_size(&d.path)).sum();
        let n_disks = c.disks.iter().filter(|d| !d.cdrom).count();
        let n_iso = c.disks.len() - n_disks;
        VmSettings {
            loaded: true,
            name: s(&c.name),
            os_index: os_index(c.guest_os()),
            cpus: c.cpus as i32,
            memory_gib: mem_gib as i32,
            nested: c.nested,
            tpm: c.tpm,
            gpu,
            opengl: c.display.opengl,
            opengl_host: match c.display.opengl_host {
                OpenglHost::Apple => 0,
                OpenglHost::Angle => 1,
            },
            width: s(c.display.width.to_string()),
            height: s(c.display.height.to_string()),
            hidpi: c.display.hidpi,
            video_decode: c.display.video_decode,
            video_encode: c.display.video_encode,
            monitors: c.display.monitors as i32,
            disks: model(disks),
            nets: model(nets),
            shares: model(shares),
            usb_picking: self.usb_picking,
            sum_usb: s(match (usb.len(), usb.iter().filter(|u| u.on).count()) {
                (0, _) => "None".to_string(),
                (1, 0) => usb[0].label.to_string(),
                (1, _) => format!("{} · connected", usb[0].label),
                (n, 0) => format!("{n} devices"),
                (n, on) => format!("{n} devices · {on} connected"),
            }),
            usb: model(usb),
            usb_host: model(usb_host),
            audio: c.audio.output,
            audio_in: c.audio.input,
            camera: c.camera,
            clip_in: c.clipboard.host_to_guest,
            clip_out: c.clipboard.guest_to_host,
            clip_max_kib: s((c.clipboard.max_bytes / 1024).to_string()),
            sandbox: c.security.sandbox,
            id_vendor: s(&c.identity.vendor),
            id_product: s(&c.identity.product),
            id_announce: c.identity.announce_vm,
            id_serials: s(format!(
                "System {}, disks {}…",
                c.identity.serial.clone().unwrap_or_else(|| "from the VM's UUID".into()),
                c.identity.disk_serial
            )),
            transport: match c.virtio_transport {
                Transport::Auto => 0,
                Transport::Mmio => 1,
                Transport::Pci => 2,
            },
            serial: match c.serial {
                Serial::Stdio => 0,
                Serial::File { .. } => 1,
                Serial::None => 2,
            },
            serial_path: s(match &c.serial {
                Serial::File { path } => path.display().to_string(),
                _ => String::new(),
            }),
            boot_kernel,
            firmware: s(firmware),
            vars: s(vars),
            kernel: s(kernel),
            initrd: s(initrd),
            cmdline: s(cmdline),
            warnings: model(warnings),
            warn_short: model(warn_short),
            locked,
            lock_short: s(lock_short),
            lock_reason: s(lock_reason),
            lock_why: s(lock_why),
            live,
            net_note: s(net_note),
            usb_note: s(if state == VmState::Suspended && live { "Changes apply when the VM resumes." } else { "" }),
            config_path: s(paths::tilde(&v.path)),
            state_path: s(paths::tilde(&v.state_dir())),
            tools_have: s(tools.0),
            tools_new: s(tools.1),
            tools_action: s(tools.2),
            sum_hw: s(format!(
                "{} CPU{} · {} GB memory{}",
                c.cpus,
                if c.cpus == 1 { "" } else { "s" },
                mem_gib,
                if c.nested { " · nested" } else { "" }
            )),
            sum_display: s(match gpu {
                0 => "No screen".to_string(),
                g => format!(
                    "{} · {}×{}{}",
                    match (g, c.display.opengl, c.display.opengl_host) {
                        (1, ..) => "2D",
                        _ if c.os == Some(GuestOs::Windows) => "3D (Direct3D 11)",
                        (_, true, OpenglHost::Apple) => "3D (Vulkan, OpenGL 4.1)",
                        (_, true, OpenglHost::Angle) => "3D (Vulkan, OpenGL 2.1)",
                        _ => "3D (Vulkan)",
                    },
                    c.display.width,
                    c.display.height,
                    if c.display.monitors > 1 { format!(" · {} monitors", c.display.monitors) } else { String::new() },
                ),
            }),
            sum_storage: s(format!(
                "{} disk{}{}{}",
                n_disks,
                if n_disks == 1 { "" } else { "s" },
                if total > 0 { format!(" · {}", fmt::bytes(total)) } else { String::new() },
                if n_iso > 0 { " · installer attached" } else { "" }
            )),
            sum_net: s(match c.nets.len() {
                0 => "Offline".to_string(),
                1 => net_summary(&c.nets[0]),
                n => format!("{n} adapters: {}", c.nets.iter().map(|n| mode_name(n.mode)).collect::<Vec<_>>().join(", ")),
            }),
            sum_share: s(match (c.shares.len(), c.shares.iter().any(|s| !s.readonly)) {
                (0, _) => "Nothing shared".to_string(),
                (n, w) => format!("{n} folder{}{}", if n == 1 { "" } else { "s" }, if w { ", guest can write" } else { ", read-only" }),
            }),
            sum_sound: s(format!(
                "{}{}",
                match (c.audio.output, c.audio.input) {
                    (false, false) => "Off",
                    (true, false) => "Plays through this machine's speakers",
                    (false, true) => "Microphone only",
                    (true, true) => "Speakers and microphone",
                },
                if c.camera { ", camera" } else { "" }
            )),
            sum_clip: s(match (c.clipboard.host_to_guest, c.clipboard.guest_to_host) {
                (true, true) => "Both ways",
                (true, false) => "To the guest only",
                (false, true) => "From the guest only",
                (false, false) => "Off",
            }),
            sum_security: s(format!(
                "{}{}{}",
                if c.security.sandbox { "Sandboxed" } else { "Sandbox is off" },
                if c.tpm { " · TPM 2.0" } else { "" },
                if !c.identity.announce_vm { " · hides VM flags" } else if !same_names(&c.identity) { " · custom identity" } else { "" }
            )),
            sum_boot: s(match &c.boot {
                Boot::Uefi { firmware: None, .. } => "UEFI".to_string(),
                Boot::Uefi { .. } => "UEFI, custom firmware".to_string(),
                Boot::Kernel { kernel, .. } => {
                    format!("Linux kernel · {}", kernel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
                }
            }),
        }
    }

    fn push_vis(&self) {
        let q = self.query.trim().to_ascii_lowercase();
        let words: Vec<&str> = q.split_whitespace().collect();
        let hit = |kw: &str| words.iter().all(|w| kw.contains(w));
        let st = self.last_settings.clone().unwrap_or_default();
        let any = words.is_empty();
        self.ui.set_vis(CardVis {
            searching: !any,
            hw: any || hit(&format!("processor cpu cpus core memory ram nested virtualization guest system os {}", st.sum_hw.to_lowercase())),
            display: any || hit("display screen gpu 3d 2d graphics vulkan opengl angle apple driver resolution width height size monitor retina hidpi scale scaling sharp blurry video decode decoding h264 hevc videotoolbox"),
            storage: any || hit("storage disk disks iso cdrom installer drive nvme virtio bus sync write safety readonly read-only compact image qcow2 space"),
            net: any || hit("network net nat bridged host-only hostonly port forward forwarding ssh mac address interface internet lan localhost loopback adapter"),
            share: any || hit("shared folders share folder virtiofs mount files tag"),
            usb: any || hit("usb devices device passthrough serial adapter security key phone dongle"),
            sound: any || hit("sound audio speakers music output microphone mic input camera webcam video call"),
            clip: any || hit("clipboard copy paste"),
            security: any || hit("security sandbox tpm transport mmio pci serial console identity fingerprint hardware maker vendor model product smbios acpi hide vm detection"),
            boot: any || hit("boot uefi firmware kernel initrd cmdline command line nvram efi"),
            tools: any || hit("guest tools agent update install reinstall vroom-setup drivers"),
        });
    }

    fn push_toasts(&mut self) {
        let now = Instant::now();
        self.toasts.retain(|t| t.until > now);
        self.ui.set_toasts(model(self.toasts.iter().map(|t| t.toast.clone()).collect()));
    }

    fn apply_theme(&self) {
        if let Some(t) = &self.lib.theme {
            self.ui.global::<Theme>().set_dark(t != "light");
        }
    }

    // ---- periodic ----

    fn tick(&mut self) {
        let mut changed = self.follow_library();
        for v in &mut self.vms {
            if v.changed_on_disk() {
                v.reload();
                // Clipboard rules and HiDPI edited by hand apply right away too.
                if let Some(c) = &v.raw {
                    console::set_clip_policy(&v.key, c.clipboard.clone());
                    console::set_hidpi(&v.key, c.display.hidpi);
                }
                changed = true;
            }
            if !console::is_running(&v.key) {
                let before = v.disk_state;
                v.refresh_state();
                changed |= before != v.disk_state;
            }
            let t = v.thumb_mtime;
            v.refresh_thumb();
            changed |= t != v.thumb_mtime;
        }
        if changed {
            self.push_all();
        } else {
            // Disk sizes grow while a VM runs.
            self.push_settings();
        }
        if !self.toasts.is_empty() {
            let n = self.toasts.len();
            let now = Instant::now();
            if self.toasts.iter().any(|t| t.until <= now) {
                self.push_toasts();
            }
            debug_assert!(self.toasts.len() <= n);
        }
        self.push_host();
    }

    // ---- changes ----

    /// Change the selected VM's file through `f`; validate and save. False
    /// if nothing was saved.
    fn change(&mut self, f: impl FnOnce(&mut VmConfig) -> Result<(), String>) -> bool {
        let Some(v) = self.current.and_then(|i| self.vms.get_mut(i)) else { return false };
        // Edited by hand since the last tick: change what's there now.
        if v.changed_on_disk() {
            v.reload();
        }
        let Some(before) = v.raw.clone() else { return false };
        let mut after = before.clone();
        let r = f(&mut after).and_then(|_| after.validate().map_err(|e| e.to_string()));
        let saved = match r {
            Ok(()) if after == before => false,
            Ok(()) => match edit::save(&v.path, &before, &after) {
                Ok(()) => {
                    v.reload();
                    console::set_clip_policy(&v.key, after.clipboard.clone());
                    console::set_hidpi(&v.key, after.display.hidpi);
                    true
                }
                Err(e) => {
                    v.reload();
                    self.toast(format!("Couldn't save: {e}"), 3);
                    false
                }
            },
            Err(e) => {
                self.toast(plain_error(&e), 3);
                false
            }
        };
        // Push even on error: the UI shows the value that really applies.
        self.last_settings = None;
        self.push_all();
        saved
    }

    /// A folder just added to the file: the running VM gets it now if it
    /// can; otherwise say when it comes.
    fn share_now(&mut self, path: &std::path::Path) {
        let Some(v) = self.cur() else { return };
        let (key, state) = (v.key.clone(), self.state_of(v));
        let Some(sh) = v.raw.as_ref().and_then(|c| c.shares.iter().rev().find(|s| s.path == path).cloned()) else { return };
        match state {
            VmState::Off => {}
            VmState::Suspended => self.toast("Applies after a restart", 0),
            _ => {
                if let Err(e) = console::share_folder(&key, path, &sh.tag, sh.readonly) {
                    self.toast(format!("Applies after a restart: {e}"), 0);
                }
            }
        }
        self.last_settings = None;
        self.push_all();
    }

    /// A folder removed from the file: unshare it now if it was shared
    /// this run; one the VM started with stays until it restarts.
    fn share_gone(&mut self, tag: &str) {
        let Some(v) = self.cur() else { return };
        let (key, state) = (v.key.clone(), self.state_of(v));
        if console::live_shared(&key, tag) {
            console::unshare_folder(&key, tag);
        } else if state != VmState::Off {
            self.toast("Applies after a restart", 0);
        }
    }

    /// Share `i` changed (writable, tag): a folder shared this run is
    /// shared again that way now; the others when the VM restarts.
    fn share_changed(&mut self, i: usize, old: &vroom_config::Share) {
        let Some(v) = self.cur() else { return };
        let (key, state) = (v.key.clone(), self.state_of(v));
        let Some(new) = v.raw.as_ref().and_then(|c| c.shares.get(i).cloned()) else { return };
        if matches!(state, VmState::Off) {
            return;
        }
        if console::live_shared(&key, &old.tag) {
            console::unshare_folder(&key, &old.tag);
            let path = v.resolved().and_then(|r| r.shares.get(i).map(|s| s.path.clone())).unwrap_or(new.path.clone());
            if let Err(e) = console::share_folder(&key, &path, &new.tag, new.readonly) {
                self.toast(format!("Applies after a restart: {e}"), 0);
            }
        } else {
            self.toast("Applies after a restart", 0);
        }
    }

    /// Hardware is locked while the VM runs or is suspended (it resumes on
    /// the hardware it was saved with). The UI greys those controls out,
    /// but the palette and keys can still ask.
    fn hw_locked(&mut self) -> bool {
        let locked = self.cur().is_some_and(|v| self.state_of(v) != VmState::Off);
        if locked {
            self.toast("Shut the VM down first: its hardware can't change while it runs or is suspended.", 0);
        }
        locked
    }

    /// Network and USB devices can change now: the VM is off, suspended
    /// (they apply when it resumes) or runs in a window of ours (they
    /// apply right away, as far as they can).
    fn live_now(&self, v: &Vm) -> bool {
        match self.state_of(v) {
            VmState::Off | VmState::Suspended => true,
            _ => console::is_running(&v.key),
        }
    }

    /// `live_now` for the selected VM; says why not if not.
    fn live_ok(&mut self) -> bool {
        let ok = self.cur().is_none_or(|v| self.live_now(v));
        if !ok {
            self.toast("This VM runs without a window here: show it (or stop it) first.", 0);
        }
        ok
    }

    /// Per adapter of `c` (the VM file), what waits and why; and a line
    /// for the whole card.
    fn net_notes(&self, v: &Vm, c: &VmConfig, state: VmState) -> (Vec<String>, String) {
        let mut notes = vec![String::new(); c.nets.len()];
        let mac = |n: &vroom_config::Net| n.mac.as_deref().map(|m| m.to_ascii_lowercase());
        match state {
            VmState::Suspended => {
                let Some(saved) = vm::suspended_config(&v.state_dir()) else { return (notes, String::new()) };
                for (i, n) in c.nets.iter().enumerate() {
                    notes[i] = match saved.nets.get(i) {
                        None => "Added: it comes when the VM next starts (a resumed VM keeps the adapters it was saved with).".into(),
                        Some(o) if mac(o) != mac(n) => "The new MAC address applies when the VM next starts.".into(),
                        Some(o) if o != n => "Applies when the VM resumes.".into(),
                        Some(_) => String::new(),
                    };
                }
                let gone = saved.nets.len().saturating_sub(c.nets.len());
                let card = match gone {
                    0 => String::new(),
                    1 => "1 adapter removed: the VM keeps it, unplugged, until it next starts.".into(),
                    n => format!("{n} adapters removed: the VM keeps them, unplugged, until it next starts."),
                };
                (notes, card)
            }
            VmState::Off => (notes, String::new()),
            _ => {
                let Some(view) = console::net_view(&v.key) else { return (notes, String::new()) };
                for (i, n) in c.nets.iter().enumerate() {
                    let Some(Some(nic)) = view.map.get(i) else {
                        notes[i] = if view.adding == Some(i) { "Adding...".into() } else { "Added: it comes when the VM next starts.".into() };
                        continue;
                    };
                    let (Some(p), Some(want)) = (view.ports.iter().find(|p| p.nic == *nic), console::net_live_of(n)) else { continue };
                    let started = view.start.get(*nic as usize);
                    notes[i] = if want.kind != p.live.kind || want.name != p.live.name {
                        if want.kind == vroom_display::NetKind::Nat && !view.nat_ok {
                            "NAT applies when the VM next starts: it started without a NAT adapter, so its sandbox allows no connections out.".into()
                        } else {
                            format!("Still {}: switching, or it couldn't (see the VM window).", console::net_mode_text(&p.live))
                        }
                    } else if want.kind == vroom_display::NetKind::Nat && want.loopback && !p.live.loopback {
                        "Reaching this Mac's localhost applies when the VM next starts (its sandbox allows it only then).".into()
                    } else if want.kind == vroom_display::NetKind::Nat && want.forwards != p.live.forwards {
                        "Port forward changes apply when the VM next starts.".into()
                    } else if started.is_some_and(|o| mac(o) != mac(n)) {
                        "The new MAC address applies when the VM next starts.".into()
                    } else {
                        String::new()
                    };
                }
                let gone = view.ports.iter().filter(|p| !view.map.contains(&Some(p.nic))).count();
                let card = match gone {
                    0 => String::new(),
                    1 => "1 adapter removed: unplugged now, gone when the VM next starts.".into(),
                    n => format!("{n} adapters removed: unplugged now, gone when the VM next starts."),
                };
                (notes, card)
            }
        }
    }

    fn set(&mut self, key: &str, v: &str) {
        if let Some(k) = key.strip_prefix("newvm.") {
            return self.newvm_set(k, v);
        }
        if key.starts_with("usbnow.") {
            return self.usb_now_act(key, Some(v));
        }
        let locked = self.cur().is_some_and(|vm| self.state_of(vm) != VmState::Off);
        // Not hardware: these apply to a running VM's window. Network
        // adapters apply to it too (or when it resumes).
        let net = key.strip_prefix("net.").and_then(|r| r.split('.').next()).and_then(|i| i.parse::<usize>().ok());
        // How a USB disk connects applies at its next connect.
        let usb = key.starts_with("usb.") && self.live_ok();
        // A folder shared this run is shared again the new way.
        let share = key.strip_prefix("share.").and_then(|r| r.split('.').next()).and_then(|i| i.parse::<usize>().ok());
        let share_ok = share.is_some() && self.live_ok();
        if locked && !key.starts_with("clip.") && key != "hidpi" && !(net.is_some() && self.live_ok()) && !usb && !share_ok {
            return;
        }
        let old_share = share.and_then(|i| self.cur().and_then(|v| v.raw.as_ref()).and_then(|c| c.shares.get(i).cloned()));
        let iface = self.host_ifaces.first().cloned();
        let vm = self.cur().map(|v| v.key.clone()).unwrap_or_default();
        let saved = match (net, key.ends_with(".mode"), v.parse::<usize>()) {
            // Port forwards set aside while on another network come back.
            (Some(i), true, Ok(m)) => self.change(|c| console::net_set_mode(&vm, c, i, m, iface.as_deref())),
            _ => self.change(|c| edit::set(c, key, v, iface.as_deref())),
        };
        if let (true, Some(i)) = (saved, net) {
            console::net_apply(&vm, i);
        }
        if let (true, Some(i), Some(old)) = (saved, share, old_share) {
            self.share_changed(i, &old);
        }
        // A USB disk connected now another way: again, this way.
        if let (true, Some(i)) = (saved && key.ends_with(".mode"), key.strip_prefix("usb.").and_then(|r| r.split('.').next()).and_then(|i| i.parse::<usize>().ok())) {
            if let Some(u) = self.cur().and_then(|v| v.raw.as_ref()).and_then(|c| c.usb.get(i).cloned()) {
                let d = vroom_display::UsbDev { id: u.id.clone(), serial: u.serial.clone().unwrap_or_default(), name: u.name.clone().unwrap_or_default() };
                console::usb_set_mode(&vm, d, u.mode, u.readonly);
            }
        }
    }

    /// A device connected for this run only, by its place in that list.
    fn run_only_dev(&self, j: &str) -> Option<vroom_display::UsbPort> {
        let v = self.cur()?;
        let (ports, _) = console::usb_now(&v.key)?;
        run_only(v.raw.as_ref()?, &ports).into_iter().nth(j.parse().ok()?)
    }

    /// The USB card's rows for this run's devices: how one connects (it
    /// connects again that way), "always connect", let go.
    fn usb_now_act(&mut self, a: &str, v: Option<&str>) {
        let Some((j, what)) = a.strip_prefix("usbnow.").and_then(|r| r.split_once('.')) else { return };
        let Some(p) = self.run_only_dev(j) else { return };
        let Some(key) = self.cur().map(|v| v.key.clone()) else { return };
        match (what, v) {
            ("mode", Some(m)) => {
                let mode = match m {
                    "0" => vroom_config::UsbMode::Files,
                    "1" => vroom_config::UsbMode::Disk,
                    _ => vroom_config::UsbMode::Device,
                };
                console::usb_set_mode(&key, p.dev, mode, p.readonly);
            }
            ("pin", None) => {
                if self.change(|c| crate::hostusb::apply_always(c, &p.dev, true, how_mode(p.how))) {
                    self.toast("Saved: it connects at every start", 1);
                }
            }
            ("disconnect", None) => console::usb_disconnect(&key, &p.dev),
            _ => {}
        }
    }

    fn act(&mut self, a: &str) {
        if let Some(i) = a.strip_prefix("palette.run:") {
            if let Ok(i) = i.parse::<usize>() {
                self.palette_sel = i;
                self.palette_key(PaletteKey::Enter);
            }
            return;
        }
        if let Some(id) = a.strip_prefix("toast.close:") {
            let id: i32 = id.parse().unwrap_or(-1);
            self.toasts.retain(|t| t.toast.id != id);
            self.push_toasts();
            return;
        }
        if let Some(k) = a.strip_prefix("log:") {
            return open_text(&paths::log_file(k));
        }
        if let Some((_, n)) = a.strip_prefix("do.").unwrap_or(a).strip_prefix("snap.").and_then(|r| r.split_once(':')) {
            // Names come from the state dir (VMM-writable); the list only
            // shows valid ones.
            if !vroom_config::valid_name(n) {
                return;
            }
        }
        if let Some(n) = a.strip_prefix("snap.restore:") {
            return self.ask(
                "Restore this snapshot?",
                "The VM starts from the moment the snapshot was taken. Anything after that is lost, including a suspended state.",
                "Restore",
                false,
                &format!("do.snap.restore:{n}"),
            );
        }
        if let Some(n) = a.strip_prefix("snap.delete:") {
            return self.ask("Delete this snapshot?", "Its disk space is freed. This can't be undone.", "Delete", true, &format!("do.snap.delete:{n}"));
        }
        if let Some(n) = a.strip_prefix("do.snap.restore:") {
            return self.restore_snapshot(n);
        }
        if let Some(n) = a.strip_prefix("do.snap.delete:") {
            return self.housekeeping("Snapshot deleted", Some(n.to_string()));
        }
        if let Some(q) = a.strip_prefix("search:") {
            self.query = q.to_string();
            self.ui.set_query(s(q));
            self.push_vis();
            return;
        }
        if let Some(rest) = a.strip_prefix("vm:") {
            // From the palette: "vm:<index>:<action>".
            let mut it = rest.splitn(2, ':');
            if let (Some(i), Some(what)) = (it.next().and_then(|i| i.parse::<usize>().ok()), it.next()) {
                // Not some other VM if that one is gone meanwhile.
                if i < self.vms.len() {
                    self.update(Msg::Select(i));
                    if what != "select" {
                        self.act(what);
                    }
                }
            }
            return;
        }
        match a {
            "start" => self.start_current(&[]),
            "tools" => {
                if let Some(v) = self.cur() {
                    console::tools_act(&v.key.clone());
                    self.last_settings = None;
                    self.push_all();
                }
            }
            "show" | "pause" | "resume" | "snapshot" | "suspend" | "shutdown" | "reset" => {
                if !self.ensure_window() {
                    return;
                }
                if let Some(v) = self.cur() {
                    if a == "show" {
                        console::show(&v.key);
                    } else {
                        console::command(&v.key, a);
                    }
                }
            }
            "poweroff" => self.ask(
                "Force power off?",
                "Like pulling the plug: unsaved work in the guest is lost. Shut down asks the guest nicely instead.",
                "Power off",
                true,
                "do.poweroff",
            ),
            "do.poweroff" => {
                if let Some(v) = self.cur() {
                    if console::is_running(&v.key) {
                        console::command(&v.key, "poweroff");
                    } else if v.disk_state == DiskState::RunningElsewhere {
                        let (name, dir) = (v.name(), v.state_dir());
                        match host::force_stop(&dir) {
                            Ok(()) => self.toast(format!("{name} was stopped."), 0),
                            Err(e) => self.toast(format!("Couldn't stop {name}: {e}"), 3),
                        }
                    }
                }
            }
            "newvm" => self.newvm_open(None),
            "newvm.cancel" => self.ui.set_newvm_open(false),
            "newvm.pick" => {
                let windows = self.ui.get_draft().os_index == 1;
                let mut d = rfd::FileDialog::new().set_title(if windows { "Windows 11 ARM64 ISO" } else { "Installer ISO or cloud image" });
                d = if windows { d.add_filter("ISO or install stick", &["iso", "raw", "img"]) } else { d.add_filter("ISO or cloud image", &["iso", "img"]) };
                if let Some(p) = d.pick_file() {
                    self.set_installer(Some(p));
                }
            }
            "newvm.clear-installer" => self.set_installer(None),
            "newvm.create" => self.newvm_create(),
            "vmware" => {
                let mut d = rfd::FileDialog::new().set_title("Import from VMware").add_filter("VMware VM", &["vmwarevm", "vmx"]);
                if let Some(h) = std::env::var_os("HOME") {
                    let lib = PathBuf::from(h).join("Virtual Machines.localized");
                    if lib.is_dir() {
                        d = d.set_directory(lib);
                    }
                }
                if let Some(p) = d.pick_file() {
                    self.vmware_plan(&p);
                }
            }
            "do.vmware" => self.vmware_run(),
            // vroom-netd, the root helper (bridged, host-only, USB devices
            // macOS holds).
            "netd.setup" => self.netd(true),
            "netd.remove" => self.ask(
                "Remove vroom-netd?",
                "Bridged and host-only networks, and USB devices macOS uses, stop working until you set it up again. macOS asks for an admin password.",
                "Remove",
                true,
                "do.netd.remove",
            ),
            "do.netd.remove" => self.netd(false),
            "import" => {
                if let Some(p) = rfd::FileDialog::new().set_title("Add a VM file").add_filter("VM file", &["toml"]).pick_file() {
                    self.import(&p);
                }
            }
            "theme" => {
                let dark = !self.ui.global::<Theme>().get_dark();
                self.ui.global::<Theme>().set_dark(dark);
                self.lib.theme = Some(if dark { "dark" } else { "light" }.into());
                let _ = self.lib.save();
            }
            "palette" => {
                self.ui.set_palette_query(s(""));
                self.palette_sel = 0;
                self.build_palette("");
                self.ui.set_palette_open(true);
            }
            "palette.close" => self.ui.set_palette_open(false),
            "confirm.cancel" => {
                self.confirm = None;
                self.ui.set_confirm_open(false);
            }
            "confirm.ok" => {
                self.ui.set_confirm_open(false);
                // On the VM it was asked for, whatever is selected now.
                if let Some((key, a)) = self.confirm.take() {
                    match self.vms.iter().position(|v| v.key == key) {
                        Some(i) => {
                            if self.current != Some(i) {
                                self.update(Msg::Select(i));
                            }
                            self.act(&a);
                        }
                        None if key.is_empty() => self.act(&a),
                        None => {}
                    }
                }
            }
            "edit-file" => {
                if let Some(v) = self.cur() {
                    open_text(&v.path);
                }
            }
            "reveal" => {
                if let Some(v) = self.cur() {
                    reveal(&v.path);
                }
            }
            "log" => {
                if let Some(v) = self.cur() {
                    let p = paths::log_file(&v.key);
                    if p.exists() {
                        open_text(&p);
                    } else {
                        self.toast("No log yet: the VM hasn't run from this app.", 0);
                    }
                }
            }
            "prune" => self.housekeeping("Snapshot layers cleaned up", None),
            "discard" => {
                if self.cur().is_some_and(|v| self.state_of(v) == VmState::Suspended) {
                    self.ask(
                        "Discard the saved state?",
                        "The VM boots fresh next time, as if the power had been cut when it was suspended.",
                        "Discard",
                        true,
                        "do.discard",
                    );
                } else {
                    self.toast("Nothing to discard: the VM isn't suspended.", 0);
                }
            }
            "do.discard" => self.housekeeping("Saved state discarded", Some(String::new())),
            "remove" => self.ask(
                "Remove from the list?",
                "Only the entry goes. The VM file and its disks stay where they are; add them back any time.",
                "Remove",
                true,
                "do.remove",
            ),
            "do.remove" => {
                if let Some(i) = self.current {
                    let p = self.vms[i].path.clone();
                    self.lib.remove(&p);
                    if let Err(e) = self.lib.save() {
                        self.toast(format!("Couldn't save the list: {e}"), 3);
                    }
                    self.vms.remove(i);
                    self.current = if self.vms.is_empty() { None } else { Some(i.min(self.vms.len() - 1)) };
                    self.last_settings = None;
                    self.push_all();
                }
            }
            "disk.add-iso" | "disk.add-existing" | "disk.add-new" if self.hw_locked() => {}
            "share.add" if !self.live_ok() => {}
            _ if a.starts_with("usbnow.") => self.usb_now_act(a, None),
            // Out of the VM file, but connected still (it was for this run).
            _ if a.starts_with("usb.") && a.ends_with(".unpin") => {
                let i = &a["usb.".len()..a.len() - ".unpin".len()];
                if self.live_ok() {
                    self.change(|c| edit::act(c, &format!("usb.{i}.remove")));
                }
            }
            "usb.add" if !self.live_ok() => {}
            "usb.add" => {
                // Open or close the list of what's plugged in.
                self.usb_picking = !self.usb_picking;
                self.usb_host = if self.usb_picking { crate::hostusb::host_list() } else { Vec::new() };
                self.push_all();
            }
            // The same by id ("usb.pick-id.1209:00aa"; scripts, VROOM_SHOT).
            _ if a.starts_with("usb.pick-id.") => {
                let id = &a["usb.pick-id.".len()..];
                if let Some(j) = self.usb_host.iter().position(|d| d.id() == id) {
                    self.act(&format!("usb.pick.{j}"));
                }
            }
            _ if a.starts_with("usb.pick.") => {
                let j: usize = a["usb.pick.".len()..].parse().unwrap_or(usize::MAX);
                if !self.live_ok() {
                    return;
                }
                if let Some(d) = self.usb_host.get(j).cloned() {
                    // A running VM gets it now too, as from its USB menu.
                    if self.change(|c| edit::add_usb(c, &d.id(), d.serial.as_deref(), Some(&d.label()))) {
                        if let Some(v) = self.cur() {
                            console::usb_plug(&v.key, crate::hostusb::dev_of(&d));
                        }
                    }
                }
            }
            "share.add" => {
                if let Some(p) = rfd::FileDialog::new().set_title("Share a folder").pick_folder() {
                    let added = self.change(|c| {
                        edit::add_share(c, &p);
                        Ok(())
                    });
                    if added {
                        self.share_now(&p);
                    }
                }
            }
            "disk.add-iso" => {
                if let Some(p) = rfd::FileDialog::new().set_title("Attach an ISO").add_filter("ISO image", &["iso", "img"]).pick_file() {
                    self.change(|c| {
                        edit::add_disk(c, &p, edit::NewDisk::Iso);
                        Ok(())
                    });
                }
            }
            "disk.add-existing" => {
                if let Some(p) =
                    rfd::FileDialog::new().set_title("Attach a disk image").add_filter("Disk image", &["qcow2", "raw", "img"]).pick_file()
                {
                    self.change(|c| {
                        edit::add_disk(c, &p, edit::NewDisk::Image);
                        Ok(())
                    });
                }
            }
            "disk.add-new" => self.new_disk(),
            _ => {
                if let Some(rest) = a.strip_prefix("disk.") {
                    let (i, what) = rest.split_once('.').unwrap_or((rest, ""));
                    let i: usize = i.parse().unwrap_or(usize::MAX);
                    match what {
                        "reveal" => {
                            if let Some(p) = self.cur().and_then(|v| v.resolved()).and_then(|c| c.disks.get(i).map(|d| d.path.clone())) {
                                reveal(&p);
                            }
                            return;
                        }
                        "compact" if !self.hw_locked() => return self.compact(i),
                        "compact" => return,
                        _ => {}
                    }
                }
                // Network adapters, USB devices and shared folders change
                // while the VM runs (or is suspended) too.
                let live = a.starts_with("net.") || a.starts_with("usb.") || a.starts_with("share.");
                if if live { !self.live_ok() } else { self.hw_locked() } {
                    return;
                }
                let gone_usb = a
                    .strip_prefix("usb.")
                    .and_then(|r| r.strip_suffix(".remove"))
                    .and_then(|i| i.parse::<usize>().ok())
                    .and_then(|i| self.cur().and_then(|v| v.raw.as_ref()).and_then(|c| c.usb.get(i).cloned()));
                let gone_share = a
                    .strip_prefix("share.")
                    .and_then(|r| r.strip_suffix(".remove"))
                    .and_then(|i| i.parse::<usize>().ok())
                    .and_then(|i| self.cur().and_then(|v| v.raw.as_ref()).and_then(|c| c.shares.get(i).map(|s| s.tag.clone())));
                let a = a.to_string();
                if !self.change(|c| edit::act(c, &a)) {
                    return;
                }
                if let Some(tag) = gone_share {
                    self.share_gone(&tag);
                }
                let Some(key) = self.cur().map(|v| v.key.clone()) else { return };
                let parts: Vec<&str> = a.split('.').collect();
                match parts.as_slice() {
                    ["net", "add"] => console::net_added(&key),
                    ["net", i, "remove"] => console::net_removed(&key, i.parse().unwrap_or(usize::MAX)),
                    ["net", i, ..] => console::net_apply(&key, i.parse().unwrap_or(usize::MAX)),
                    _ => {}
                }
                if let Some(u) = gone_usb {
                    console::usb_unplug(&key, &u.id, u.serial.as_deref());
                }
            }
        }
    }

    /// Read a VMware VM and ask before copying anything.
    fn vmware_plan(&mut self, p: &Path) {
        if self.import_toast.is_some() {
            return self.toast("An import is already running.", 0);
        }
        match vroom_import::plan(p) {
            Err(e) => self.toast(e.to_string(), 3),
            Ok(plan) => {
                let disks = plan.disks.iter().filter(|d| !d.cdrom).count();
                let mut text = format!(
                    "{} CPUs, {} GB, {} disk{} to copy into {}. VMware's copy is only read.",
                    plan.cpus,
                    plan.memory_mib.div_ceil(1024),
                    disks,
                    if disks == 1 { "" } else { "s" },
                    paths::tilde(&paths::vms_dir())
                );
                for n in &plan.notes {
                    text += "\n\n";
                    text += n;
                }
                let title = format!("Import {}?", plan.name);
                let (ok, danger) = if plan.suspended { ("Import anyway", true) } else { ("Import", false) };
                self.vmware = Some(plan);
                self.confirm = Some((String::new(), "do.vmware".into()));
                self.ui.set_confirm(ConfirmInfo { title: s(title), text: s(text), ok: s(ok), danger, action: s("do.vmware") });
                self.ui.set_confirm_open(true);
            }
        }
    }

    fn vmware_run(&mut self) {
        let Some(plan) = self.vmware.take() else { return };
        let opts = vroom_import::Options { name: Some(self.unique_name(&plan.name)), keep_hardware_ids: false, accept_suspended: true };
        self.update(Msg::Toast { text: "Importing from VMware: 0%".into(), kind: 0, action: String::new() });
        self.import_toast = self.toasts.last().map(|t| t.toast.id);
        let dir = paths::vms_dir();
        std::thread::spawn(move || {
            let _ = std::fs::create_dir_all(&dir);
            let mut last = 0u32;
            let r = vroom_import::run(&plan, &dir, &opts, |p| {
                // Several disks count as one bar.
                let per = 100.0 / p.disks.max(1) as f64;
                let pct = (per * p.disk as f64 + per * p.done as f64 / p.total.max(1) as f64) as u32;
                if pct != last {
                    last = pct;
                    msg::post(move || Msg::ImportProgress(pct));
                }
            });
            msg::post(move || Msg::Imported(r.map_err(|e| e.to_string())));
        });
    }

    fn ask(&mut self, title: &str, text: &str, ok: &str, danger: bool, action: &str) {
        let key = self.cur().map(|v| v.key.clone()).unwrap_or_default();
        self.confirm = Some((key, action.to_string()));
        self.ui.set_confirm(ConfirmInfo { title: s(title), text: s(text), ok: s(ok), danger, action: s(action) });
        self.ui.set_confirm_open(true);
    }

    fn start_current(&mut self, extra: &[String]) {
        let Some(v) = self.cur() else { return };
        let st = self.state_of(v);
        if console::is_running(&v.key) {
            return console::show(&v.key);
        }
        if st == VmState::Running {
            self.ensure_window();
            return;
        }
        let Some(cfg) = v.resolved() else {
            return self.toast("Fix the VM file first: it has an error.", 3);
        };
        // Bridged and host-only need the root helper: offer to set it up
        // rather than fail to start.
        let helper = if cfg.nets.iter().any(|n| n.mode != vroom_config::NetKind::Nat) { rootd::state_now() } else { rootd::Helper::Unknown };
        if matches!(helper, rootd::Helper::Missing | rootd::Helper::Stopped) {
            return self.ask(
                "Set up vroom-netd?",
                "This VM has a bridged or host-only network adapter. Those go through vroom-netd, a small helper that runs as root. It's set up once; macOS asks for an admin password. Start the VM again once it's done.",
                "Set up",
                false,
                "netd.setup",
            );
        }
        // One from before host-only DHCP: the guest would get no address
        // there. Asked once a run; after that the VM starts anyway.
        let host_only = cfg.nets.iter().any(|n| n.mode == vroom_config::NetKind::HostOnly);
        if host_only && helper.host_only_old() && !NETD_OLD_ASKED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return self.ask(
                "Set up vroom-netd again?",
                "This VM has a host-only network adapter. The vroom-netd set up on this Mac is from an older vroom: its host-only networks give no addresses and the Mac isn't on them. The new one gives each VM an address by DHCP and puts the Mac on the network (192.168.77.1 on lab). macOS asks for an admin password. Start the VM again once it's done.",
                "Set up again",
                false,
                "netd.setup",
            );
        }
        let (path, name) = (v.path.clone(), cfg.name.clone());
        match console::start(&path, &cfg, extra) {
            Ok(_) => {}
            Err(e) => self.toast(format!("Couldn't start {name}: {e}"), 3),
        }
        self.push_all();
    }

    /// The current VM has a window here, or gets one: a VM running
    /// without one (its app crashed) is shown again. False if it can't be.
    fn ensure_window(&mut self) -> bool {
        let Some(i) = self.current else { return false };
        self.attach(i, true)
    }

    /// Reattach to VM `i` if it runs without a window. `say`: explain
    /// when that can't be done.
    fn attach(&mut self, i: usize, say: bool) -> bool {
        let Some(v) = self.vms.get(i) else { return false };
        if console::is_running(&v.key) {
            return true;
        }
        if v.disk_state != DiskState::RunningElsewhere {
            return false;
        }
        let (name, dir) = (v.name(), v.state_dir());
        let Some(sock) = console::viewer_socket(&dir) else {
            if say {
                self.toast(format!("{name} runs from a terminal (vroom run): its window is there."), 0);
            }
            return false;
        };
        let Some(cfg) = v.resolved() else { return false };
        let pid = vroom_state::StateLock::holder(&dir).unwrap_or(0);
        match console::attach(&v.path.clone(), &cfg, &sock, pid) {
            Ok(_) => true,
            Err(e) => {
                if say {
                    self.toast(format!("{name} runs, but {e}. Force power off (power menu) stops it."), 3);
                }
                false
            }
        }
    }

    fn restore_snapshot(&mut self, name: &str) {
        let Some(v) = self.cur() else { return };
        if console::is_running(&v.key) || v.disk_state == DiskState::RunningElsewhere {
            return self.toast("Shut the VM down first.", 2);
        }
        let state = v.state_dir();
        let snap = state.join("snapshots").join(name);
        if !vroom_config::valid_name(name) || !snap.is_dir() {
            return self.toast("That snapshot is gone.", 3);
        }
        if vroom_state::is_suspended(&state) {
            // The suspend image would win over the snapshot next time.
            let r = vroom_state::StateLock::acquire(&state)
                .map_err(|e| e.to_string())
                .and_then(|_l| vroom_state::discard_suspend(&state, &v.base_dirs()).map_err(|e| e.to_string()));
            if let Err(e) = r {
                return self.toast(format!("Couldn't drop the saved state: {e}"), 3);
            }
        }
        self.start_current(&["--restore".into(), snap.display().to_string()]);
    }

    /// Snapshot delete (`Some(name)`), discard suspend (`Some("")`) or
    /// prune (`None`), in the background.
    fn housekeeping(&mut self, done: &'static str, what: Option<String>) {
        let Some(v) = self.cur() else { return };
        let (state, bases) = (v.state_dir(), v.base_dirs());
        std::thread::spawn(move || {
            let r = vroom_state::StateLock::acquire(&state).map_err(|e| match e {
                vroom_state::Error::Busy(_) => "The VM is running. Stop it first.".to_string(),
                e => e.to_string(),
            });
            let r = r.and_then(|_lock| {
                match what.as_deref() {
                    Some("") => vroom_state::discard_suspend(&state, &bases),
                    Some(n) => vroom_state::delete(&state, n, &bases),
                    None => vroom_state::prune(&state, &bases),
                }
                .map_err(|e| e.to_string())
            });
            msg::post(move || match r {
                Ok(rep) => {
                    let freed = rep.deleted.len() + rep.folded.len();
                    let text = if freed > 0 { format!("{done} ({freed} layer{} merged or removed)", if freed == 1 { "" } else { "s" }) } else { done.to_string() };
                    Msg::Toast { text, kind: 1, action: String::new() }
                }
                Err(e) => Msg::Toast { text: e, kind: 3, action: String::new() },
            });
            msg::post(|| Msg::Refresh);
        });
    }

    fn compact(&mut self, i: usize) {
        let Some(p) = self.cur().and_then(|v| v.resolved()).and_then(|c| c.disks.get(i).map(|d| d.path.clone())) else { return };
        self.toast(format!("Compacting {}...", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()), 0);
        std::thread::spawn(move || {
            let r = vroom_block::qcow2::compact(&p, &vroom_block::qcow2::OpenPolicy::default());
            msg::post(move || match r {
                Ok((before, after)) => Msg::Toast {
                    text: format!("Compacted: {} → {} on disk", fmt::bytes(before), fmt::bytes(after)),
                    kind: 1,
                    action: String::new(),
                },
                Err(e) => Msg::Toast { text: format!("Compact failed: {e}"), kind: 3, action: String::new() },
            });
            msg::post(|| Msg::Refresh);
        });
    }

    fn new_disk(&mut self) {
        let Some(v) = self.cur() else { return };
        let Some(c) = &v.raw else { return };
        let dir = v.path.parent().map(Path::to_path_buf).unwrap_or_default();
        let mut n = 2;
        let file = loop {
            let f = format!("{}-disk{n}.qcow2", c.name);
            if !dir.join(&f).exists() {
                break f;
            }
            n += 1;
        };
        if let Err(e) = vroom_block::qcow2::Qcow2Image::create(&dir.join(&file), 64 << 30, None) {
            return self.toast(format!("Couldn't create the disk: {e}"), 3);
        }
        let f = PathBuf::from(&file);
        let saved = self.change(|c| {
            edit::add_disk(c, &f, edit::NewDisk::Image);
            Ok(())
        });
        if saved {
            self.toast(format!("Added {file} (64 GB, takes space as it fills)"), 1);
        } else {
            // Not in the file (too many disks...): don't leave it lying around.
            let _ = std::fs::remove_file(dir.join(&file));
        }
    }

    /// Pick up VMs another window added to the list (and drop ones it
    /// took out, unless they run here).
    fn follow_library(&mut self) -> bool {
        let m = std::fs::metadata(paths::library_file()).and_then(|m| m.modified()).ok();
        if m == self.lib_mtime {
            return false;
        }
        self.lib_mtime = m;
        let lib = Library::load(&paths::library_file());
        if lib.error.is_some() {
            return false;
        }
        let before = self.vms.len();
        let cur = self.current.and_then(|i| self.vms.get(i)).map(|v| v.path.clone());
        self.vms.retain(|v| lib.vms.contains(&v.path) || console::is_running(&v.key));
        let removed = self.vms.len() != before;
        for p in &lib.vms {
            if !self.vms.iter().any(|v| &v.path == p) {
                self.vms.push(Vm::open(p));
            }
        }
        let theme = self.lib.theme.clone();
        self.lib = lib;
        self.lib.theme = theme;
        self.current = cur.and_then(|c| self.vms.iter().position(|v| v.path == c)).or((!self.vms.is_empty()).then_some(0));
        self.last_settings = None;
        removed || self.vms.len() != before
    }

    fn add_vm(&mut self, p: &Path) {
        let p = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        if let Some(i) = self.vms.iter().position(|v| v.path == p) {
            self.current = Some(i);
        } else {
            self.lib.add(&p);
            if let Err(e) = self.lib.save() {
                self.toast(format!("Couldn't save the list: {e}"), 3);
            }
            self.vms.push(Vm::open(&p));
            self.current = Some(self.vms.len() - 1);
        }
        self.last_settings = None;
        self.push_all();
    }

    fn import(&mut self, p: &Path) {
        match VmConfig::load_raw(p) {
            Ok(_) => self.add_vm(p),
            Err(e) => self.toast(format!("Not a VM file: {}", plain_error(&e.to_string())), 3),
        }
    }

    /// From Finder: a VM file starts (as `vroom-gui vm.toml` does), the
    /// rest is a drop on the library.
    fn opened(&mut self, p: &Path) {
        let vm = p.extension().is_some_and(|e| e.eq_ignore_ascii_case("toml")) && VmConfig::load_raw(p).is_ok();
        if vm {
            self.add_vm(p);
            self.start_current(&[]);
        } else {
            self.update(Msg::ShowLibrary(None));
            self.dropped(p);
        }
    }

    fn dropped(&mut self, p: &Path) {
        let ext = p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        match ext.as_str() {
            "toml" => self.import(p),
            "vmwarevm" | "vmx" => self.vmware_plan(p),
            "iso" | "img" | "raw" => self.newvm_open(Some(p.to_path_buf())),
            _ => self.toast("Drop an ISO to make a VM, a .toml VM file to add it, or a VMware VM to import it.", 0),
        }
    }

    // ---- New VM ----

    fn unique_name(&self, base: &str) -> String {
        let taken = |n: &str| self.vms.iter().any(|v| v.name() == n) || paths::vms_dir().join(n).exists();
        if !taken(base) {
            return base.to_string();
        }
        (2..).map(|i| format!("{base}-{i}")).find(|n| !taken(n)).unwrap_or_else(|| base.to_string())
    }

    fn newvm_open(&mut self, installer: Option<PathBuf>) {
        let os = installer
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| GuestOs::guess(&n.to_string_lossy()))
            .unwrap_or(GuestOs::Ubuntu);
        self.installer = installer;
        self.ui.set_newvm_error(s(""));
        self.ui.set_newvm_creating(false);
        self.ui.set_newvm_progress(s(""));
        // Ready-made VMs: the Mac user's name, their SSH keys.
        let keys = ssh_keys();
        self.ui.set_newvm_keys(model(keys.iter().map(|p| s(paths::tilde(p))).collect()));
        let mut d = self.ui.get_draft();
        if d.user.is_empty() {
            d.user = s(std::env::var("USER").unwrap_or_default().to_ascii_lowercase());
        }
        d.ssh_key = 0;
        d.ssh = false;
        self.ui.set_draft(d);
        self.newvm_set("os", &os_index(os).to_string());
        self.ui.set_newvm_open(true);
    }

    fn newvm_set(&mut self, k: &str, v: &str) {
        match k {
            "installer" => return self.set_installer(Some(PathBuf::from(v))),
            "start" | "ssh" | "desktop" => {
                let mut d = self.ui.get_draft();
                match k {
                    "start" => d.start = v == "true",
                    "ssh" => d.ssh = v == "true",
                    _ => d.desktop = v == "true",
                }
                return self.ui.set_draft(d);
            }

            "os" => {}
            _ => return,
        }
        let os = GuestOs::ALL.get(v.parse::<usize>().unwrap_or(0)).copied().unwrap_or(GuestOs::Ubuntu);
        let base = match os {
            GuestOs::Other | GuestOs::Linux => "linux",
            GuestOs::Windows => "win11",
            o => os_id(o),
        };
        let name = self.unique_name(base);
        let t = Template::defaults(&name, os, "");
        let mut d = self.ui.get_draft();
        d.os_index = os_index(os);
        d.name = s(&name);
        d.cpus = t.cpus as i32;
        d.memory_gib = (t.memory_mib / 1024) as i32;
        d.disk_gib = Template::default_disk_gib(os) as i32;
        if os == GuestOs::Windows {
            d.gpu3d = false;
        }
        if d.installer.is_empty() {
            d.start = true;
        }
        self.ui.set_draft(d);
        self.set_installer(self.installer.clone());
    }

    fn set_installer(&mut self, p: Option<PathBuf>) {
        let mut d = self.ui.get_draft();
        d.installer = s(p.as_ref().map(|p| paths::tilde(p)).unwrap_or_default());
        d.ready = ready_kind(GuestOs::ALL.get(d.os_index.max(0) as usize).copied().unwrap_or(GuestOs::Linux), p.as_deref()).is_some();
        self.installer = p;
        self.ui.set_draft(d.clone());
        self.ui.set_newvm_dest(s(format!("Goes in {}", paths::tilde(&paths::vms_dir().join(d.name.as_str())))));
    }

    fn newvm_create(&mut self) {
        let d = self.ui.get_draft();
        let name = d.name.trim().to_string();
        if !vroom_config::valid_name(&name) {
            return self.ui.set_newvm_error(s("Use only letters, digits, '.', '_' and '-' in the name."));
        }
        if self.vms.iter().any(|v| v.name() == name) {
            return self.ui.set_newvm_error(s("There's already a VM with that name."));
        }
        let os = GuestOs::ALL.get(d.os_index.max(0) as usize).copied().unwrap_or(GuestOs::Linux);
        let installer = self.installer.clone();
        if d.ready {
            return self.newvm_ready(&name, os, installer);
        }
        let (cpus, mem, disk, gpu3d, start) = (d.cpus.max(1) as u32, d.memory_gib.max(1) as u64, d.disk_gib.max(1) as u64, d.gpu3d, d.start);
        self.ui.set_newvm_error(s(""));
        self.ui.set_newvm_creating(true);
        std::thread::spawn(move || {
            let r = create_vm(&name, os, installer.as_deref(), cpus, mem, disk, gpu3d);
            msg::post(move || Msg::Created(r, start));
        });
    }

    /// A ready-made VM (vroom_tools::ready), made off the UI thread.
    fn newvm_ready(&mut self, name: &str, os: GuestOs, installer: Option<PathBuf>) {
        use vroom_tools::ready::{Account, Spec};
        let d = self.ui.get_draft();
        let ssh_key = if d.ssh {
            let keys = ssh_keys();
            let Some(k) = keys.get(d.ssh_key.max(0) as usize) else {
                return self.ui.set_newvm_error(s("No SSH key in ~/.ssh (make one: ssh-keygen -t ed25519)."));
            };
            match std::fs::read_to_string(k) {
                Ok(t) => Some(t.trim().to_string()),
                Err(e) => return self.ui.set_newvm_error(s(format!("{}: {e}", paths::tilde(k)))),
            }
        } else {
            None
        };
        // Ports other VMs forward: SSH gets another one.
        let taken_ports: Vec<u16> = self.vms.iter().filter_map(|v| v.raw.as_ref()).flat_map(|c| c.nets.iter().flat_map(|n| n.port_forwards.iter().map(|f| f.host_port))).collect();
        let spec = Spec {
            name: name.to_string(),
            dir: paths::vms_dir().join(name),
            cpus: d.cpus.max(1) as u32,
            memory_mib: d.memory_gib.max(1) as u64 * 1024,
            disk_gib: d.disk_gib.max(8) as u64,
            gpu3d: d.gpu3d && os != GuestOs::Windows,
            account: Account { user: d.user.trim().to_string(), password: d.password.to_string(), ssh_key },
            taken_ports,
        };
        // Checked now, so mistakes show before a download.
        if let Some(Err(e)) = ready_kind(os, installer.as_deref()).map(|k| vroom_tools::ready::check(&spec, &k)) {
            return self.ui.set_newvm_error(s(e));
        }
        let (start, desktop) = (d.start, d.desktop);
        self.ui.set_newvm_error(s(""));
        self.ui.set_newvm_creating(true);
        std::thread::spawn(move || {
            let mut say = |t: &str| {
                let t = t.to_string();
                msg::post(move || Msg::NewVmProgress(t));
            };
            let r = (|| {
                let kind = match ready_kind(os, installer.as_deref()) {
                    Some(vroom_tools::ready::Kind::Ubuntu { image, .. }) if image.as_os_str().is_empty() => {
                        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
                        let image = vroom_tools::ready::ubuntu_image(&home.join("Library/Caches/vroom"), &mut say)?;
                        vroom_tools::ready::Kind::Ubuntu { image, desktop }
                    }
                    Some(vroom_tools::ready::Kind::Ubuntu { image, .. }) => vroom_tools::ready::Kind::Ubuntu { image, desktop },
                    Some(k) => k,
                    None => return Err("not a ready-made VM".to_string()),
                };
                vroom_tools::ready::create(&spec, &kind, &mut say)
            })();
            msg::post(move || Msg::Created(r, start));
        });
    }

    // ---- palette ----

    fn build_palette(&mut self, q: &str) {
        let mut all: Vec<PaletteEntry> = Vec::new();
        let e = |title: String, hint: &str, icon: &'static str, action: String| PaletteEntry { title, hint: hint.into(), icon, action };
        for (i, v) in self.vms.iter().enumerate() {
            let n = v.name();
            match self.state_of(v) {
                VmState::Off => all.push(e(format!("Start {n}"), "VM", "play", format!("vm:{i}:start"))),
                VmState::Suspended => all.push(e(format!("Resume {n}"), "VM", "play", format!("vm:{i}:start"))),
                _ if console::is_running(&v.key) => {
                    all.push(e(format!("Show {n}"), "VM", "window", format!("vm:{i}:show")));
                    all.push(e(format!("Snapshot {n}"), "VM", "camera", format!("vm:{i}:snapshot")));
                    if let Some((_, _, act @ ("Update" | "Install" | "Reinstall"))) = console::tools_info(&v.key) {
                        all.push(e(format!("{act} guest tools in {n}"), "VM", "package", format!("vm:{i}:tools")));
                    }
                    all.push(e(format!("Suspend {n}"), "VM", "moon", format!("vm:{i}:suspend")));
                }
                _ => {}
            }
        }
        all.push(e("New VM".into(), "⌘N", "plus", "newvm".into()));
        all.push(e("Add a VM file".into(), "", "file", "import".into()));
        all.push(e("Import from VMware".into(), "", "upload", "vmware".into()));
        // vroom-netd, the root helper: bridged, host-only, USB devices macOS uses.
        match rootd::state() {
            h @ rootd::Helper::Ready(_) => {
                if h.host_only_old() {
                    all.push(e("Set up vroom-netd again (DHCP on host-only networks)".into(), "", "shield", "netd.setup".into()));
                }
                all.push(e("Remove vroom-netd (root helper)".into(), "", "shield-off", "netd.remove".into()))
            }
            rootd::Helper::Unknown => {}
            _ => all.push(e("Set up vroom-netd (root helper for bridged networking and USB)".into(), "", "shield", "netd.setup".into())),
        }
        for (i, v) in self.vms.iter().enumerate() {
            all.push(e(format!("Go to {}", v.name()), "", "arrow-right", format!("vm:{i}:select")));
        }
        if self.cur().is_some() {
            for (t, icon, a) in [
                ("Open config file", "file", "edit-file"),
                ("Show in Finder", "reveal", "reveal"),
                ("Show log", "terminal", "log"),
                ("Add a network adapter", "network", "net.add"),
                ("Share a folder", "folder", "share.add"),
                ("Attach an ISO", "disc", "disk.add-iso"),
                ("Clean up snapshot layers", "broom", "prune"),
            ] {
                all.push(e(t.into(), "this VM", icon, a.into()));
            }
            for (t, icon, word) in [
                ("Settings: processor and memory", "cpu", "cpu"),
                ("Settings: display", "display", "display"),
                ("Settings: storage", "disk", "disk"),
                ("Settings: network and ports", "globe", "network"),
                ("Settings: shared folders", "folder", "share"),
                ("Settings: sound", "speaker", "sound"),
                ("Settings: clipboard", "clipboard", "clipboard"),
                ("Settings: security", "shield", "security"),
                ("Settings: boot", "rocket", "boot"),
            ] {
                all.push(e(t.into(), "", icon, format!("search:{word}")));
            }
        }
        let dark = self.ui.global::<Theme>().get_dark();
        all.push(e(if dark { "Light theme" } else { "Dark theme" }.into(), "", "sun", "theme".into()));
        let q = q.to_ascii_lowercase();
        let words: Vec<&str> = q.split_whitespace().collect();
        self.palette = all.into_iter().filter(|p| words.iter().all(|w| p.title.to_ascii_lowercase().contains(w))).take(9).collect();
        self.palette_sel = self.palette_sel.min(self.palette.len().saturating_sub(1));
        self.push_palette();
    }

    fn push_palette(&self) {
        self.ui.set_palette_items(model(
            self.palette.iter().map(|p| PaletteItem { title: s(&p.title), hint: s(&p.hint), icon: s(p.icon), action: s(&p.action) }).collect(),
        ));
        self.ui.set_palette_selected(self.palette_sel as i32);
    }

    fn palette_key(&mut self, k: PaletteKey) {
        match k {
            PaletteKey::Up => self.palette_sel = self.palette_sel.saturating_sub(1),
            PaletteKey::Down => self.palette_sel = (self.palette_sel + 1).min(self.palette.len().saturating_sub(1)),
            PaletteKey::Escape => {
                self.ui.set_palette_open(false);
                return;
            }
            PaletteKey::Enter => {
                self.ui.set_palette_open(false);
                if let Some(p) = self.palette.get(self.palette_sel).cloned() {
                    self.act(&p.action);
                }
                return;
            }
        }
        self.push_palette();
    }
}

/// What a ready-made VM is made from, if this is one: Ubuntu with no
/// installer (the cloud image, downloaded: an empty path) or a cloud
/// image (.img); Windows with its ISO.
fn ready_kind(os: GuestOs, installer: Option<&Path>) -> Option<vroom_tools::ready::Kind> {
    use vroom_tools::ready::Kind;
    let ext = installer.and_then(|p| p.extension()).map(|e| e.to_string_lossy().to_ascii_lowercase());
    match (os, ext.as_deref()) {
        (GuestOs::Ubuntu, None) => Some(Kind::Ubuntu { image: PathBuf::new(), desktop: false }),
        (GuestOs::Ubuntu, Some("img")) => Some(Kind::Ubuntu { image: installer?.to_path_buf(), desktop: false }),
        (GuestOs::Windows, Some("iso")) => Some(Kind::Windows { iso: installer?.to_path_buf() }),
        _ => None,
    }
}

/// The Mac user's SSH public keys (~/.ssh/*.pub), ed25519 first.
fn ssh_keys() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else { return Vec::new() };
    let mut keys: Vec<PathBuf> = std::fs::read_dir(PathBuf::from(home).join(".ssh"))
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "pub")).collect())
        .unwrap_or_default();
    keys.sort_by_key(|p| (!p.to_string_lossy().contains("ed25519"), p.clone()));
    keys
}

/// Identity changed by hand? Random serials (every new VM has them) don't count.
fn same_names(id: &vroom_config::Identity) -> bool {
    let mut id = id.clone();
    let d = vroom_config::Identity::default();
    id.serial = d.serial.clone();
    id.disk_serial = d.disk_serial.clone();
    id == d
}

fn is_qcow2(p: &Path) -> bool {
    use std::io::Read;
    let mut m = [0u8; 4];
    std::fs::File::open(p).and_then(|mut f| f.read_exact(&mut m)).is_ok() && &m == b"QFI\xfb"
}

fn mode_name(k: NetKind) -> &'static str {
    match k {
        NetKind::Nat => "NAT",
        NetKind::Bridged => "Bridged",
        NetKind::HostOnly => "Host-only",
    }
}

fn net_summary(n: &vroom_config::Net) -> String {
    match n.mode {
        NetKind::Nat => {
            let f: Vec<String> = n
                .port_forwards
                .iter()
                .take(2)
                .map(|p| if p.guest_port == 22 { format!("SSH on {}", p.host_port) } else { format!("{}→{}", p.host_port, p.guest_port) })
                .collect();
            if f.is_empty() {
                "NAT".into()
            } else {
                format!("NAT · {}{}", f.join(", "), if n.port_forwards.len() > 2 { "..." } else { "" })
            }
        }
        NetKind::Bridged => format!("Bridged · {}", n.interface.as_deref().unwrap_or("?")),
        NetKind::HostOnly => format!("Host-only · {}", n.network.as_deref().unwrap_or(vroom_config::DEFAULT_HOST_ONLY)),
    }
}

fn capitalize(w: &str) -> String {
    let mut c = w.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// A config warning in a few words, for its banner (the whole of it goes
/// in the tooltip).
fn warn_short(w: &str) -> String {
    let short = if w.starts_with("security.sandbox") {
        "Sandbox is off"
    } else if w.starts_with("net.allow_host_loopback") {
        "Guest reaches this Mac's localhost"
    } else if w.starts_with("port forward") {
        "Port forward open to your network"
    } else if w.starts_with("audio.input") {
        "Microphone on"
    } else if w.starts_with("share") && w.contains("home folder") {
        "Share exposes your home folder"
    } else if w.starts_with("share") && w.contains("writable") {
        "Writable share"
    } else {
        return capitalize(w.split_once(": ").map_or(w, |(a, _)| a));
    };
    short.to_string()
}

/// The VM file's entry for `d`? One without a serial number matches any.
fn in_file(c: &VmConfig, d: &vroom_display::UsbDev) -> Option<usize> {
    c.usb.iter().position(|u| u.id == d.id && !u.serial.as_deref().is_some_and(|s| s != d.serial))
}

/// The devices the running VM has connected for this run only (not in
/// its file), once each, in the order the VMM lists them.
fn run_only(c: &VmConfig, ports: &[vroom_display::UsbPort]) -> Vec<vroom_display::UsbPort> {
    let mut out: Vec<vroom_display::UsbPort> = Vec::new();
    for p in ports.iter().filter(|p| in_file(c, &p.dev).is_none()) {
        if !out.iter().any(|o| o.dev.id == p.dev.id && o.dev.serial == p.dev.serial) {
            out.push(p.clone());
        }
    }
    out
}

fn usb_mode_index(m: vroom_config::UsbMode) -> i32 {
    match m {
        vroom_config::UsbMode::Files => 0,
        vroom_config::UsbMode::Disk => 1,
        vroom_config::UsbMode::Device => 2,
    }
}

fn how_mode(h: vroom_display::UsbHow) -> vroom_config::UsbMode {
    match h {
        vroom_display::UsbHow::Files => vroom_config::UsbMode::Files,
        vroom_display::UsbHow::Disk => vroom_config::UsbMode::Disk,
        vroom_display::UsbHow::Device => vroom_config::UsbMode::Device,
    }
}

/// The USB card: the VM file's devices ("always connect"), then those
/// connected now for this run only. `now`: what the running VM has
/// connected, and whether its guest tools answer.
fn usb_rows(c: &VmConfig, now: Option<(Vec<vroom_display::UsbPort>, bool)>) -> Vec<UsbRow> {
    let running = now.is_some();
    let (ports, agent) = now.unwrap_or_default();
    let host = if c.usb.is_empty() && ports.is_empty() { Vec::new() } else { crate::hostusb::host_list() };
    usb_rows_with(c, &ports, agent, running, &host)
}

fn usb_rows_with(
    c: &VmConfig,
    ports: &[vroom_display::UsbPort],
    agent: bool,
    running: bool,
    host: &[vroom_usb::list::HostDevice],
) -> Vec<UsbRow> {
    use crate::console::usbmenu::{connected_note, CAPTURED};
    use vroom_config::UsbMode;
    let ids = |id: &str, serial: &str| format!("{id}{}", if serial.is_empty() { String::new() } else { format!(" · serial {serial}") });
    let same = |d: &vroom_display::UsbDev, id: &str, serial: Option<&str>| d.id == id && serial.is_none_or(|s| s == d.serial || d.serial.is_empty());
    let files_of = |h: Option<&vroom_usb::list::HostDevice>| -> Vec<String> {
        h.map(|h| crate::usbdisk::volumes(h).into_iter().map(|(_, p)| p.display().to_string()).collect()).unwrap_or_default()
    };
    let mut rows = Vec::new();
    for (i, u) in c.usb.iter().enumerate() {
        let here = host.iter().find(|d| Some((d.vendor, d.product)) == u.ids() && (u.serial.is_none() || d.serial == u.serial));
        let mine: Vec<vroom_display::UsbPort> = ports.iter().filter(|p| same(&p.dev, &u.id, u.serial.as_deref())).cloned().collect();
        let on = !mine.is_empty();
        let storage = cfg!(target_os = "macos")
            && (here.is_some_and(crate::hostusb::is_disk) || u.mode != UsbMode::Device || mine.iter().any(|p| p.how != vroom_display::UsbHow::Device));
        let id_line = ids(&u.id, u.serial.as_deref().unwrap_or(""));
        let (note, tip, warn) = if on {
            connected_note(&mine, &files_of(here), agent)
        } else {
            match here {
                None => ("Not plugged in".to_string(), String::new(), false),
                Some(h) if storage && h.captured && u.mode != UsbMode::Device => ("Unplug it and plug it in again".into(), CAPTURED.into(), true),
                Some(h) if u.mode == UsbMode::Device && h.needs_root() => (
                    "Used by macOS".into(),
                    "macOS holds it: the VM takes it through vroom-netd, the root helper (USB menu of the VM window).".into(),
                    true,
                ),
                Some(_) if running => ("Not connected".into(), "Plugged in. Connect it from the VM window's USB menu.".into(), false),
                Some(_) => ("Plugged in".into(), "Connects when the VM starts.".into(), false),
            }
        };
        let label = u.name.clone().or_else(|| here.map(|d| d.label())).unwrap_or_else(|| u.id.clone());
        let mode = mine.first().map(|p| how_mode(p.how)).unwrap_or(u.mode);
        rows.push(UsbRow {
            label: s(label),
            note: s(note),
            tip: s(if tip.is_empty() { id_line } else { format!("{tip}\n{id_line}") }),
            storage,
            mode: usb_mode_index(mode),
            warn,
            pinned: true,
            index: i as i32,
            on,
        });
    }
    for (j, p) in run_only(c, ports).into_iter().enumerate() {
        let here = host.iter().find(|h| h.id() == p.dev.id && (p.dev.serial.is_empty() || h.serial.as_deref() == Some(p.dev.serial.as_str())));
        let mine: Vec<vroom_display::UsbPort> = ports.iter().filter(|q| q.dev == p.dev).cloned().collect();
        let (note, tip, warn) = connected_note(&mine, &files_of(here), agent);
        let storage = cfg!(target_os = "macos") && (p.how != vroom_display::UsbHow::Device || here.is_some_and(crate::hostusb::is_disk));
        rows.push(UsbRow {
            label: s(if p.dev.name.is_empty() { p.dev.id.clone() } else { p.dev.name.clone() }),
            note: s(format!("{note} · this run")),
            tip: s(format!("{tip}\nConnected for this run only.\n{}", ids(&p.dev.id, &p.dev.serial))),
            storage,
            mode: usb_mode_index(how_mode(p.how)),
            warn,
            pinned: false,
            index: j as i32,
            on: true,
        });
    }
    rows
}

/// Config errors as people would say them.
fn plain_error(e: &str) -> String {
    let e = e.strip_prefix("invalid config: ").unwrap_or(e);
    capitalize(e)
}

fn open_text(p: &Path) {
    let r = if cfg!(target_os = "macos") {
        std::process::Command::new("/usr/bin/open").arg("-t").arg(p).status()
    } else {
        std::process::Command::new("xdg-open").arg(p).status()
    };
    if let Err(e) = r {
        msg::toast(format!("Couldn't open {}: {e}", p.display()), 3);
    }
}

fn reveal(p: &Path) {
    let r = if cfg!(target_os = "macos") {
        std::process::Command::new("/usr/bin/open").arg("-R").arg(p).status()
    } else {
        std::process::Command::new("xdg-open").arg(p.parent().unwrap_or(p)).status()
    };
    if let Err(e) = r {
        msg::toast(format!("Couldn't show {}: {e}", p.display()), 3);
    }
}

/// Write `<vms dir>/<name>/<name>.toml` and its disk.
fn create_vm(name: &str, os: GuestOs, installer: Option<&Path>, cpus: u32, mem_gib: u64, disk_gib: u64, gpu3d: bool) -> Result<PathBuf, String> {
    let dir = paths::vms_dir().join(name);
    if dir.exists() {
        return Err(format!("{} already exists", paths::tilde(&dir)));
    }
    if let Some(i) = installer {
        if !i.is_file() {
            return Err(format!("{}: no such file", paths::tilde(i)));
        }
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let disk_file = format!("{name}.qcow2");
    let mut t = Template::defaults(name, os, &disk_file);
    t.installer = installer;
    t.cpus = cpus;
    t.memory_mib = mem_gib * 1024;
    t.gpu = if gpu3d && os != GuestOs::Windows { Gpu::Virtio3d } else { Gpu::Virtio2d };
    let cfg = dir.join(format!("{name}.toml"));
    let text = t.render();
    VmConfig::from_toml(&text).map_err(|e| format!("template: {e}"))?;
    let undo = |e: String| {
        let _ = std::fs::remove_dir_all(&dir);
        e
    };
    std::fs::write(&cfg, text).map_err(|e| undo(e.to_string()))?;
    vroom_block::qcow2::Qcow2Image::create(&dir.join(&disk_file), disk_gib << 30, None).map_err(|e| undo(e.to_string()))?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VM: &str = "name = \"NAME\"\n[boot]\ntype = \"uefi\"\nvars = \"NAME.state/efivars.fd\"\n\n[[net]]\nmode = \"nat\"\n";

    /// The app over a fresh data dir, with VMs of these names in its list.
    fn app(names: &[&str]) -> (tempfile::TempDir, App) {
        i_slint_backend_testing::init_no_event_loop();
        let t = tempfile::tempdir().unwrap();
        paths::TEST_HOME.with(|h| *h.borrow_mut() = Some(t.path().join("home")));
        let mut lib = Library::load(&paths::library_file());
        for n in names {
            let p = t.path().join(format!("{n}.toml"));
            std::fs::write(&p, VM.replace("NAME", n)).unwrap();
            lib.add(&p);
        }
        lib.save().unwrap();
        (t, App::new().unwrap())
    }

    fn names(a: &App) -> Vec<String> {
        a.vms.iter().map(Vm::name).collect()
    }

    #[test]
    fn a_vm_another_window_adds_shows_up() {
        let (t, mut a) = app(&["one"]);
        let p = t.path().join("two.toml");
        std::fs::write(&p, VM.replace("NAME", "two")).unwrap();
        // What `vroom-gui two.toml` does in its own process.
        let mut lib = Library::load(&paths::library_file());
        assert!(lib.add(&p));
        std::thread::sleep(std::time::Duration::from_millis(20));
        lib.save().unwrap();
        a.tick();
        assert_eq!(names(&a), vec!["one", "two"]);
        // The window's settings button finds its page.
        let key = a.vms[1].key.clone();
        a.update(Msg::ShowLibrary(Some(key)));
        assert_eq!(a.current, Some(1));
    }

    fn act(a: &mut App, s: &str) {
        a.update(Msg::Act(s.into()));
    }

    #[test]
    fn confirm_acts_on_the_vm_it_was_asked_for() {
        let (_t, mut a) = app(&["a", "b"]);
        act(&mut a, "remove");
        // Something else got selected while the dialog was up (the palette).
        a.update(Msg::Select(1));
        act(&mut a, "confirm.ok");
        assert_eq!(names(&a), ["b"]);
        assert_eq!(a.ui.get_settings().name, "b");
    }

    #[test]
    fn palette_entry_of_a_removed_vm() {
        let (_t, mut a) = app(&["a", "b", "c"]);
        a.update(Msg::Select(2));
        act(&mut a, "do.remove");
        assert_eq!(a.current, Some(1));
        assert_eq!(a.ui.get_settings().name, "b");
        // Built while "c" was there: must not hit whatever is selected now.
        act(&mut a, "vm:2:do.remove");
        assert_eq!(names(&a), ["a", "b"]);
    }

    #[test]
    fn suspended_hardware_is_locked() {
        let (t, mut a) = app(&["a"]);
        let file = t.path().join("a.toml");
        let suspend = t.path().join("a.state/suspend");
        std::fs::create_dir_all(&suspend).unwrap();
        std::fs::write(suspend.join("manifest.json"), "{}").unwrap();
        a.update(Msg::Refresh);
        assert!(a.ui.get_settings().locked);
        let before = std::fs::read_to_string(&file).unwrap();
        for k in ["identity.random", "disk.add-new", "disk.0.compact"] {
            act(&mut a, k);
            assert_eq!(std::fs::read_to_string(&file).unwrap(), before, "{k}");
        }
        a.update(Msg::Set("cpus".into(), "4".into()));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
        // Network adapters apply when it resumes (or wait for the next
        // start, which the card says).
        assert!(a.ui.get_settings().live);
        act(&mut a, "net.add");
        assert_ne!(std::fs::read_to_string(&file).unwrap(), before);
        // Clipboard rules aren't hardware.
        a.update(Msg::Set("clip.out".into(), "false".into()));
        assert!(std::fs::read_to_string(&file).unwrap().contains("guest_to_host = false"));
    }

    #[test]
    fn hand_edits_between_ticks_are_kept() {
        let (t, mut a) = app(&["a"]);
        let file = t.path().join("a.toml");
        let text = std::fs::read_to_string(&file).unwrap().replace("name = \"a\"", "name = \"a\"\nmemory_mib = 8192 # by hand");
        std::fs::write(&file, text).unwrap();
        a.update(Msg::Set("cpus".into(), "3".into()));
        let now = std::fs::read_to_string(&file).unwrap();
        assert!(now.contains("memory_mib = 8192 # by hand") && now.contains("cpus = 3"), "{now}");
    }

    #[test]
    fn monitors_from_the_display_card() {
        let (t, mut a) = app(&["a"]);
        a.update(Msg::Set("gpu".into(), "1".into()));
        a.update(Msg::Set("monitors".into(), "3".into()));
        assert!(std::fs::read_to_string(t.path().join("a.toml")).unwrap().contains("monitors = 3"));
        assert_eq!(a.ui.get_settings().monitors, 3);
        // Out of range: refused, the file keeps 3.
        a.update(Msg::Set("monitors".into(), "9".into()));
        assert_eq!(a.ui.get_settings().monitors, 3);
    }

    #[test]
    fn warnings_in_a_few_words() {
        assert_eq!(warn_short("bridged network: the guest sits on your LAN like a physical machine"), "Bridged network");
        assert_eq!(warn_short("audio.input: the guest hears your microphone"), "Microphone on");
        assert_eq!(warn_short("port forward 0.0.0.0:2222 -> guest 22: reachable from other machines"), "Port forward open to your network");
        assert_eq!(warn_short("clipboard both ways with no sandbox"), "Clipboard both ways with no sandbox");
    }

    #[test]
    fn usb_card_has_the_file_and_this_run() {
        use vroom_display::{UsbDev, UsbHow, UsbPort};
        let c = VmConfig::from_toml(
            "name = \"a\"\n[boot]\ntype = \"uefi\"\nvars = \"a.state/v.fd\"\n\n[[usb]]\nid = \"18a5:0302\"\nserial = \"V1\"\nname = \"Verbatim\"\n\n[[usb]]\nid = \"0951:1666\"\nname = \"Kingston\"\nmode = \"files\"\n",
        )
        .unwrap();
        let dev = |id: &str, serial: &str, name: &str| UsbDev { id: id.into(), serial: serial.into(), name: name.into() };
        let port = |port, d: UsbDev, how| UsbPort { port, dev: d, here: true, always: false, how, readonly: false };
        let ports = vec![
            // The Kingston (in the file, by id only) as files, a stick for
            // this run only as a whole disk.
            port(64, dev("0951:1666", "K9", "Kingston"), UsbHow::Files),
            port(3, dev("1209:00ac", "disk8", "Test disk"), UsbHow::Disk),
        ];
        let rows = usb_rows_with(&c, &ports, false, true, &[]);
        assert_eq!(rows.len(), 3);
        let (v, k, run) = (&rows[0], &rows[1], &rows[2]);
        assert!(v.pinned && !v.on && v.note == "Not plugged in" && v.tip.contains("18a5:0302 · serial V1"));
        assert!(k.pinned && k.on && k.mode == 0 && k.warn, "files, no guest tools: a warning");
        assert!(k.tip.contains("sudo mount -t virtiofs vroom-usb0 /mnt"), "{}", k.tip);
        assert!(!run.pinned && run.on && run.index == 0 && run.mode == 1);
        assert!(run.note.ends_with("this run"));
        // A disk connected some way is a disk, whatever macOS says now.
        assert_eq!(run.storage, cfg!(target_os = "macos"));
        assert_eq!(run_only(&c, &ports).len(), 1);
    }

    #[test]
    fn snapshot_list_only_has_usable_names() {
        let (t, mut a) = app(&["a"]);
        let m = r#"{"format":1,"created_unix":1,"description":"","arch":"","backend":"","config":null,"memory":[],"files":{},"meta":{}}"#;
        for n in ["snap-1", "bad name", "x\u{202e}y", "-"] {
            let d = t.path().join("a.state/snapshots").join(n);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("manifest.json"), m).unwrap();
        }
        a.update(Msg::Refresh);
        let mut rows: Vec<String> = a.snap_model.iter().map(|r| r.name.to_string()).collect();
        rows.sort();
        assert_eq!(rows, ["-", "snap-1"]);
    }
}
