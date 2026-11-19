use gdk4_x11::X11Surface;
use gtk4::{
    cairo, gdk, glib, prelude::*, Application, ApplicationWindow, Box as GBox, CheckButton,
    ColorDialog, ColorDialogButton, CssProvider, DrawingArea, Label, Orientation, SpinButton,
    ToggleButton, Window,
};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    rc::Rc,
    time::Duration,
};
use x11rb::{
    connection::Connection,
    protocol::xfixes::ConnectionExt as _,
    protocol::xproto::*,
    rust_connection::RustConnection,
};

// Points are stored in X root (physical) coordinates, shared by every monitor overlay.
struct Pt {
    x: f64,
    y: f64,
    rgb: (f64, f64, f64),
}

#[derive(Default)]
struct Tail {
    pts: VecDeque<Pt>,
    last: (i16, i16),
    hue: f64,
}

struct Cfg {
    rainbow: Cell<bool>,
    rgb: Cell<(f64, f64, f64)>,
    width: Cell<f64>,
    max_len: Cell<usize>,
    taper: Cell<bool>,
}

struct Run {
    id: glib::SourceId,
    conn: Rc<RustConnection>,
    root: u32,
    wins: Vec<Window>,
}

fn hsv(h: f64) -> (f64, f64, f64) {
    let h = (h % 360.0) / 60.0;
    let x = 1.0 - (h % 2.0 - 1.0).abs();
    match h as u8 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    }
}

// Exact position/size in physical pixels, raised to the top of the stack.
fn place(conn: &RustConnection, xid: u32, g: (i32, i32, u32, u32)) {
    let _ = conn.configure_window(
        xid,
        &ConfigureWindowAux::new()
            .x(g.0)
            .y(g.1)
            .width(g.2)
            .height(g.3)
            .stack_mode(StackMode::ABOVE),
    );
    let _ = conn.flush();
}

fn spin(label: &str, min: f64, max: f64, val: f64, row: &GBox, f: impl Fn(f64) + 'static) {
    row.append(&Label::builder().label(label).xalign(0.0).build());
    let s = SpinButton::with_range(min, max, 1.0);
    s.set_value(val);
    s.set_numeric(true);
    s.connect_value_changed(move |s| f(s.value()));
    row.append(&s);
}

// One click-through, window-manager-bypassing overlay for a single monitor.
// `g` is the monitor rectangle in physical X pixels (x, y, w, h).
fn make_overlay(
    conn: Rc<RustConnection>,
    tail: Rc<RefCell<Tail>>,
    cfg: Rc<Cfg>,
    g: (i32, i32, u32, u32),
    scale: i32,
) -> (Window, DrawingArea, u32) {
    let area = DrawingArea::new();
    let (ox, oy, sf) = (g.0 as f64, g.1 as f64, scale as f64);
    area.set_draw_func(move |_, cr, _, _| {
        let t = tail.borrow();
        let n = t.pts.len();
        if n < 2 {
            return;
        }
        cr.set_line_cap(cairo::LineCap::Round);
        let (lw, taper) = (cfg.width.get(), cfg.taper.get());
        for i in 1..n {
            let (a, b) = (&t.pts[i - 1], &t.pts[i]);
            let f = if taper { i as f64 / (n - 1) as f64 } else { 1.0 };
            cr.set_source_rgba(b.rgb.0, b.rgb.1, b.rgb.2, if taper { 0.15 + 0.85 * f } else { 1.0 });
            cr.set_line_width((lw * f).max(1.0));
            cr.move_to((a.x - ox) / sf, (a.y - oy) / sf);
            cr.line_to((b.x - ox) / sf, (b.y - oy) / sf);
            let _ = cr.stroke();
        }
    });

    let win = Window::builder()
        .decorated(false)
        .focusable(false)
        .resizable(false)
        .default_width(g.2 as i32 / scale)
        .default_height(g.3 as i32 / scale)
        .child(&area)
        .build();
    win.add_css_class("tail-overlay");

    // Realize (creates the X window without showing it) so override-redirect and the exact
    // geometry are set BEFORE it is mapped. No unmap/remap tricks.
    gtk4::prelude::WidgetExt::realize(&win);
    let mut xid = 0u32;
    if let Some(surf) = win.surface() {
        surf.set_input_region(&cairo::Region::create()); // click-through
        if let Ok(xs) = surf.downcast::<X11Surface>() {
            xid = xs.xid() as u32;
            let _ = conn.change_window_attributes(
                xid,
                &ChangeWindowAttributesAux::new().override_redirect(1),
            );
            place(&conn, xid, g);
        }
    }

    // GTK may resize the window when it maps; re-apply the exact monitor rectangle afterwards.
    if xid != 0 {
        let conn = conn.clone();
        win.connect_map(move |_| {
            for ms in [40u64, 200, 600] {
                let conn = conn.clone();
                glib::timeout_add_local_once(Duration::from_millis(ms), move || {
                    place(&conn, xid, g)
                });
            }
        });
    }
    (win, area, xid)
}

fn build(app: &Application) {
    let tail = Rc::new(RefCell::new(Tail::default()));
    let cfg = Rc::new(Cfg {
        rainbow: Cell::new(true),
        rgb: Cell::new((0.0, 0.6, 1.0)),
        width: Cell::new(12.0),
        max_len: Cell::new(2),
        taper: Cell::new(true),
    });

    let css = CssProvider::new();
    css.load_from_data("window.tail-overlay { background: transparent; }");
    gtk4::style_context_add_provider_for_display(
        &gdk::Display::default().unwrap(),
        &css,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // Controls
    let root_box = GBox::new(Orientation::Vertical, 8);
    for m in [
        GBox::set_margin_top as fn(&GBox, i32),
        GBox::set_margin_bottom,
        GBox::set_margin_start,
        GBox::set_margin_end,
    ] {
        m(&root_box, 16);
    }

    let button = ToggleButton::with_label("Start/Stop");
    root_box.append(&button);

    let rainbow = CheckButton::with_label("Rainbow colors");
    rainbow.set_active(true);
    let picker = ColorDialogButton::new(Some(ColorDialog::new()));
    picker.set_rgba(&gdk::RGBA::new(0.0, 0.6, 1.0, 1.0));
    picker.set_sensitive(false);
    {
        let (cfg, picker) = (cfg.clone(), picker.clone());
        rainbow.connect_toggled(move |c| {
            cfg.rainbow.set(c.is_active());
            picker.set_sensitive(!c.is_active());
        });
    }
    {
        let cfg = cfg.clone();
        picker.connect_rgba_notify(move |b| {
            let c = b.rgba();
            cfg.rgb.set((c.red() as f64, c.green() as f64, c.blue() as f64));
        });
    }
    root_box.append(&rainbow);
    root_box.append(&picker);

    {
        let cfg = cfg.clone();
        spin("Width (px)", 1.0, 100.0, 12.0, &root_box, move |v| cfg.width.set(v));
    }
    {
        let cfg = cfg.clone();
        spin("Max length (points)", 2.0, 1000.0, 2.0, &root_box, move |v| {
            cfg.max_len.set(v as usize)
        });
    }
    let taper = CheckButton::with_label("Shrink/fade as it gets older");
    taper.set_active(true);
    {
        let cfg = cfg.clone();
        taper.connect_toggled(move |c| cfg.taper.set(c.is_active()));
    }
    root_box.append(&taper);

    let win = ApplicationWindow::builder()
        .application(app)
        .title("mah-mouse-tail")
        .default_width(300)
        .child(&root_box)
        .build();

    let running: Rc<RefCell<Option<Run>>> = Rc::new(RefCell::new(None));

    let stop = {
        let (running, tail) = (running.clone(), tail.clone());
        Rc::new(move || {
            if let Some(r) = running.borrow_mut().take() {
                r.id.remove();
                let _ = r.conn.xfixes_show_cursor(r.root);
                let _ = r.conn.flush();
                for w in r.wins {
                    w.destroy();
                }
            }
            tail.borrow_mut().pts.clear();
        })
    };

    {
        let stop = stop.clone();
        win.connect_close_request(move |_| {
            stop();
            glib::Propagation::Proceed
        });
    }

    button.connect_toggled(move |b| {
        if !b.is_active() {
            stop();
            return;
        }
        if running.borrow().is_some() {
            return;
        }
        let Ok((conn, screen)) = x11rb::connect(None) else { return };
        let root = conn.setup().roots[screen].root;

        // Hide the system cursor (XFixes); tied to this connection.
        if conn.xfixes_query_version(4, 0).ok().and_then(|c| c.reply().ok()).is_some() {
            let _ = conn.xfixes_hide_cursor(root);
            let _ = conn.flush();
        }
        let conn = Rc::new(conn);

        // One overlay per monitor, each placed in physical X coordinates, regardless of
        // which monitor the app was started on.
        let monitors = gdk::Display::default().unwrap().monitors();
        let scale = monitors
            .item(0)
            .and_downcast::<gdk::Monitor>()
            .map(|m| m.scale_factor().max(1))
            .unwrap_or(1);
        let mut wins = Vec::new();
        let mut areas = Vec::new();
        let mut xids = Vec::new();
        for i in 0..monitors.n_items() {
            let Some(m) = monitors.item(i).and_downcast::<gdk::Monitor>() else { continue };
            let r = m.geometry();
            let g = (
                r.x() * scale,
                r.y() * scale,
                (r.width() * scale) as u32,
                (r.height() * scale) as u32,
            );
            let (ov, area, xid) = make_overlay(conn.clone(), tail.clone(), cfg.clone(), g, scale);
            ov.present();
            wins.push(ov);
            areas.push(area);
            if xid != 0 {
                xids.push(xid);
            }
        }

        let (tail, cfg, c2) = (tail.clone(), cfg.clone(), conn.clone());
        let mut tick: u32 = 0;
        let id = glib::timeout_add_local(Duration::from_millis(16), move || {
            tick = tick.wrapping_add(1);
            // Keep every overlay above all other windows, including newly opened ones.
            if tick % 8 == 0 {
                for &x in &xids {
                    let _ = c2.configure_window(
                        x,
                        &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
                    );
                }
                let _ = c2.flush();
            }
            let mut t = tail.borrow_mut();
            let mut changed = false;
            // Root coordinates span all monitors; the same tail is drawn on each overlay.
            if let Some(p) = c2.query_pointer(root).ok().and_then(|c| c.reply().ok()) {
                let pos = (p.root_x, p.root_y);
                if pos != t.last {
                    t.last = pos;
                    changed = true;
                    let rgb = if cfg.rainbow.get() {
                        t.hue = (t.hue + 4.0) % 360.0;
                        hsv(t.hue)
                    } else {
                        cfg.rgb.get()
                    };
                    t.pts.push_back(Pt { x: pos.0 as f64, y: pos.1 as f64, rgb });
                }
            }
            let max = cfg.max_len.get();
            while t.pts.len() > max {
                t.pts.pop_front();
                changed = true;
            }
            if changed {
                for a in &areas {
                    a.queue_draw();
                }
            }
            glib::ControlFlow::Continue
        });
        *running.borrow_mut() = Some(Run { id, conn, root, wins });
    });

    win.present();
}

fn main() -> glib::ExitCode {
    unsafe { std::env::set_var("GDK_BACKEND", "x11") };
    let app = Application::builder().application_id("dev.mah.mousetail").build();
    app.connect_activate(build);
    app.run()
}
