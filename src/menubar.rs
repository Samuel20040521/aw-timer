//! Menu-bar item: animated Sisyphus icon plus focus time.
//!
//! Top line: minutes of the current focus streak. Bottom line: today's total
//! focus in hours with one decimal.
//!
//! Rendering: every animation frame is rasterized once into a `CGImage`.
//! Core Animation then plays them with a discrete `CAKeyframeAnimation` on a
//! mask layer, so the frames are composited by the render server and this
//! process stays idle between the 1 Hz state updates. Masks (alpha only)
//! over a solid-colour layer give template-image behaviour: the colour
//! follows the menu bar's light/dark appearance.
//!
//! Threading: AppKit runs on the main thread; the tracker loop runs on a
//! worker thread and publishes into a `Mutex<MenuState>`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::process::Command;
use std::ptr::NonNull;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2::{AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSControlStateValueOff, NSControlStateValueOn, NSEventMask,
    NSEventModifierFlags, NSEventType, NSAlertSecondButtonReturn, NSAppearance, NSTextField, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
    NSApplicationActivationPolicy, NSBezierPath, NSBitmapImageFileType, NSBitmapImageRep, NSColor,
    NSDeviceRGBColorSpace, NSFont, NSFontAttributeName, NSFontWeightSemibold,
    NSForegroundColorAttributeName, NSGraphicsContext, NSImage, NSLineCapStyle, NSLineJoinStyle,
    NSMenu, NSMenuItem, NSStatusBar, NSStatusBarButton, NSStatusItem, NSStringDrawing,
    NSVariableStatusItemLength,
};
use objc2_core_foundation::{CFType, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColor, CGImage};
use objc2_foundation::{
    NSObjectProtocol,
    NSArray, NSAttributedStringKey, NSDictionary, NSObject, NSPoint, NSRect, NSSize, NSString,
    NSTimer, ns_string,
};
use objc2_quartz_core::{
    CACurrentMediaTime, CAKeyframeAnimation, CALayer, CAMediaTiming, CATransaction, kCAAnimationDiscrete,
    kCAFillModeForwards,
};

use crate::categorize::{Categorizer, Kind};
use crate::client::AwClient;
use crate::config::Settings;
use crate::journal::{self, Entry, Rest};
use crate::reportwin::ReportController;
use crate::sound::{self, Choice, Player};
use crate::sisyphus::{self, ICON_H, ICON_W, Prim, Scene};
use crate::tracker::{self, Snapshot};

/// Rasterization scale; 2x is crisp on Retina and fine when downsampled.
const SCALE: f64 = 2.0;
const PAD: f64 = 3.0;
const TEXT_GAP: f64 = 2.0;
const FONT_SIZE: f64 = 9.5;
/// Text baselines inside the `ICON_H`-high box.
const TOP_Y: f64 = 10.2;
const BOTTOM_Y: f64 = -0.6;

/// What the tracker thread last observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Working,
    Resting,
    Offline,
}

#[derive(Debug, Clone)]
struct MenuState {
    phase: Phase,
    /// Current focus streak (0 while resting), seconds at `at`.
    streak_secs: f64,
    /// Today's total focus, seconds at `at`.
    total_secs: f64,
    /// Whether both numbers keep counting up between polls.
    live: bool,
    at: Instant,
    lines: [String; 2],
    rest: Option<Rest>,
    goal: Option<String>,
    manual_project: Option<String>,
    /// "📁 CVSD 1.2h · LSV 0.3h" (today's focus per project).
    projects_line: String,
}

impl MenuState {
    fn from_snapshot(snap: &Snapshot) -> Self {
        let working = snap.block.is_some();
        let streak = snap.streak.as_ref().map_or(0.0, |s| s.active_seconds);
        let last_is_focus = snap.activities.last().is_some_and(|a| a.kind == Kind::Focus);
        let mut status = snap.status_line().lines().map(str::to_owned).collect::<Vec<_>>();
        status.resize(2, String::new());
        let mut per: Vec<(String, f64)> = Vec::new();
        for a in snap.activities.iter().filter(|a| a.kind == Kind::Focus) {
            if let Some(p) = &a.project {
                match per.iter_mut().find(|(n, _)| n == p) {
                    Some(e) => e.1 += a.seconds(),
                    None => per.push((p.clone(), a.seconds())),
                }
            }
        }
        per.sort_by(|a, b| b.1.total_cmp(&a.1));
        let projects_line = per
            .iter()
            .map(|(n, sec)| format!("{n} {:.1}h", (sec / 360.0).floor() / 10.0))
            .collect::<Vec<_>>()
            .join(" · ");
        Self {
            rest: snap.rest,
            goal: snap.goal.clone(),
            manual_project: snap.manual_project.clone(),
            projects_line,
            phase: if working { Phase::Working } else { Phase::Resting },
            streak_secs: if working { streak } else { 0.0 },
            total_secs: snap.total(Some(Kind::Focus)),
            live: working && last_is_focus && snap.streak.is_some(),
            at: Instant::now(),
            lines: [status[0].clone(), status[1].clone()],
        }
    }

    fn offline() -> Self {
        Self {
            phase: Phase::Offline,
            streak_secs: 0.0,
            total_secs: 0.0,
            live: false,
            at: Instant::now(),
            lines: ["連不到 ActivityWatch".into(), "會自動重試".into()],
            rest: None,
            goal: None,
            manual_project: None,
            projects_line: String::new(),
        }
    }

    /// `("25m", "3.2h")`, extrapolated between polls.
    fn time_text(&self) -> (String, String) {
        if self.phase == Phase::Offline {
            return ("--".into(), "--".into());
        }
        let extra = if self.live { self.at.elapsed().as_secs_f64() } else { 0.0 };
        let streak_min = ((self.streak_secs + extra) / 60.0).floor();
        // Truncate (not round) so "1.0h" only appears after a full hour.
        let total_h = ((self.total_secs + extra) / 360.0).floor() / 10.0;
        (format!("{streak_min}m"), format!("{total_h:.1}h"))
    }
}

/// State the menu actions need.
struct TargetIvars {
    /// Wakes the tracker thread so the UI reflects an action immediately.
    poke: Sender<()>,
    /// Goal currently in effect (pre-fills the dialog).
    goal: RefCell<Option<String>>,
    /// Project names; a menu item's tag indexes this (-1 = none).
    projects: Vec<String>,
    /// Background sound catalog; a menu item's tag indexes this (-1 = off).
    choices: Vec<Choice>,
    player: RefCell<Player>,
    /// Set after construction; used to pop the menu up on right-click.
    status_item: RefCell<Option<Retained<NSStatusItem>>>,
    menu: RefCell<Option<Retained<NSMenu>>>,
    report: Retained<ReportController>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and we add no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AwTimerMenuTarget"]
    #[ivars = TargetIvars]
    struct MenuTarget;

    impl MenuTarget {
        /// Rest submenu: the item's tag is the duration in minutes
        /// (0 = until ended manually).
        #[unsafe(method(startRest:))]
        fn start_rest_action(&self, sender: Option<&AnyObject>) {
            let tag = sender.and_then(|s| s.downcast_ref::<NSMenuItem>()).map_or(0, |m| m.tag());
            self.start_rest((tag > 0).then_some(tag as f64));
        }

        #[unsafe(method(endRest:))]
        fn end_rest(&self, _sender: Option<&AnyObject>) {
            self.record(Entry::RestEnd { t: chrono::Utc::now() });
        }

        #[unsafe(method(setProject:))]
        fn set_project(&self, sender: Option<&AnyObject>) {
            let tag = sender.and_then(|s| s.downcast_ref::<NSMenuItem>()).map_or(-1, |m| m.tag());
            let name = usize::try_from(tag).ok().and_then(|i| self.ivars().projects.get(i)).cloned();
            self.record(Entry::Project { t: chrono::Utc::now(), name: name.unwrap_or_default() });
        }

        #[unsafe(method(playSound:))]
        fn play_sound(&self, sender: Option<&AnyObject>) {
            let tag = sender.and_then(|s| s.downcast_ref::<NSMenuItem>()).map_or(-1, |m| m.tag());
            let iv = self.ivars();
            let mut player = iv.player.borrow_mut();
            match usize::try_from(tag).ok().and_then(|i| iv.choices.get(i)) {
                Some(choice) => {
                    if let Err(e) = player.play(&choice.source) {
                        crate::notify::notify("aw-timer 背景音", &e, "");
                    }
                }
                None => player.stop(),
            }
        }

        /// Left click: open the menu. Right click (or Control-click):
        /// play/pause background sound.
        #[unsafe(method(statusClicked:))]
        fn status_clicked(&self, _sender: Option<&AnyObject>) {
            let mtm = MainThreadMarker::from(self);
            let event = NSApplication::sharedApplication(mtm).currentEvent();
            let wants_toggle = event.is_some_and(|e| {
                e.r#type() == NSEventType::RightMouseUp || e.modifierFlags().contains(NSEventModifierFlags::Control)
            });
            if !wants_toggle {
                let (item, menu) = (self.ivars().status_item.borrow(), self.ivars().menu.borrow());
                if let (Some(item), Some(menu)) = (item.as_ref(), menu.as_ref()) {
                    // Attach the menu just for this click so that a right
                    // click keeps reaching this action.
                    item.setMenu(Some(menu));
                    if let Some(button) = item.button(mtm) {
                        // SAFETY: NSButton responds to performClick:.
                        let _: () = unsafe { msg_send![&*button, performClick: std::ptr::null::<AnyObject>()] };
                    }
                    item.setMenu(None);
                }
                return;
            }
            if let Err(e) = self.ivars().player.borrow_mut().toggle() {
                crate::notify::notify("aw-timer 背景音", &e, "");
            }
        }

        #[unsafe(method(setGoal:))]
        fn set_goal(&self, _sender: Option<&AnyObject>) {
            let current = self.ivars().goal.borrow().clone().unwrap_or_default();
            if let Some(text) = ask_goal(MainThreadMarker::from(self), &current) {
                self.record(Entry::Goal { t: chrono::Utc::now(), text });
            }
        }

        #[unsafe(method(openDashboard:))]
        fn open_dashboard(&self, _sender: Option<&AnyObject>) {
            let _ = Command::new("open").arg("http://localhost:5600").status();
        }

        #[unsafe(method(openReport:))]
        fn open_report(&self, _sender: Option<&AnyObject>) {
            self.ivars().report.show();
        }
    }
);

impl MenuTarget {
    fn new(
        mtm: MainThreadMarker,
        poke: Sender<()>,
        projects: Vec<String>,
        sound: &sound::SoundSettings,
        config_path: Option<String>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetIvars {
            poke,
            goal: RefCell::new(None),
            projects,
            choices: sound::catalog(sound),
            player: RefCell::new(Player::new(sound.clone())),
            status_item: RefCell::new(None),
            menu: RefCell::new(None),
            report: ReportController::new(mtm, config_path),
        });
        // SAFETY: plain NSObject initializer.
        unsafe { msg_send![super(this), init] }
    }

    fn start_rest(&self, minutes: Option<f64>) {
        self.record(Entry::RestStart { t: chrono::Utc::now(), minutes });
    }

    fn record(&self, entry: Entry) {
        if let Err(e) = journal::append(&entry) {
            eprintln!("journal write failed: {e}");
        }
        let _ = self.ivars().poke.send(());
    }
}

/// Bring this (accessory) app to the front.
pub(crate) fn activate(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    // `activate` is macOS 14+; older systems only have the (since
    // deprecated) `activateIgnoringOtherApps:`.
    if app.respondsToSelector(sel!(activate)) {
        app.activate();
    } else {
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
}

/// Modal text prompt for the focus goal.
///
/// Returns `Some(text)` to set (empty text clears), `None` if cancelled.
fn ask_goal(mtm: MainThreadMarker, current: &str) -> Option<String> {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(ns_string!("這次專注的目標是？"));
    alert.setInformativeText(ns_string!("會顯示在選單、專注時段通知與今日報表。"));
    alert.addButtonWithTitle(ns_string!("設定"));
    alert.addButtonWithTitle(ns_string!("清除目標"));
    alert.addButtonWithTitle(ns_string!("取消"));
    let field = NSTextField::initWithFrame(
        NSTextField::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(280.0, 24.0)),
    );
    field.setStringValue(&NSString::from_str(current));
    field.setPlaceholderString(Some(ns_string!("例如：寫完 CVSD HW3 的 testbench")));
    alert.setAccessoryView(Some(&field));
    alert.window().setInitialFirstResponder(Some(&field));
    // An accessory app is never frontmost on its own; bring the dialog up.
    activate(mtm);
    match alert.runModal() {
        r if r == NSAlertFirstButtonReturn => Some(field.stringValue().to_string().trim().to_owned()),
        r if r == NSAlertSecondButtonReturn => Some(String::new()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Drawing (AppKit, into whatever NSGraphicsContext is current)
// ---------------------------------------------------------------------------

/// Draw primitives in black, placed at `origin` and scaled by `k`.
fn draw_prims(prims: &[Prim], origin: NSPoint, k: f64) {
    let p = |(x, y): (f64, f64)| NSPoint::new(origin.x + x * k, origin.y + y * k);
    let black = NSColor::blackColor();
    for prim in prims {
        match prim {
            Prim::Line { pts, width, alpha } => {
                let path = NSBezierPath::bezierPath();
                path.moveToPoint(p(pts[0]));
                for &q in &pts[1..] {
                    path.lineToPoint(p(q));
                }
                path.setLineWidth(width * k);
                path.setLineCapStyle(NSLineCapStyle::Round);
                path.setLineJoinStyle(NSLineJoinStyle::Round);
                black.colorWithAlphaComponent(*alpha).setStroke();
                path.stroke();
            }
            Prim::Circle { c, r, width, alpha } => {
                let ctr = p(*c);
                let rect = NSRect::new(
                    NSPoint::new(ctr.x - r * k, ctr.y - r * k),
                    NSSize::new(2.0 * r * k, 2.0 * r * k),
                );
                let path = NSBezierPath::bezierPathWithOvalInRect(rect);
                let color = black.colorWithAlphaComponent(*alpha);
                if *width > 0.0 {
                    path.setLineWidth(width * k);
                    color.setStroke();
                    path.stroke();
                } else {
                    color.setFill();
                    path.fill();
                }
            }
        }
    }
}

fn text_attrs(k: f64) -> Retained<NSDictionary<NSAttributedStringKey, AnyObject>> {
    // SAFETY: reading AppKit's exported constant.
    let font = NSFont::monospacedDigitSystemFontOfSize_weight(FONT_SIZE * k, unsafe { NSFontWeightSemibold });
    let color = NSColor::blackColor();
    let (font_obj, color_obj): (&AnyObject, &AnyObject) = (&font, &color);
    // SAFETY: reading AppKit's exported constants.
    let keys = unsafe { [NSFontAttributeName, NSForegroundColorAttributeName] };
    NSDictionary::from_slices(&keys, &[font_obj, color_obj])
}

fn text_width(attrs: &NSDictionary<NSAttributedStringKey, AnyObject>, text: &str) -> f64 {
    // SAFETY: valid attribute dictionary.
    unsafe { NSString::from_str(text).sizeWithAttributes(Some(attrs)) }.width
}

/// Width in points of the two-line text column at scale 1.
fn column_width(top: &str, bottom: &str) -> f64 {
    let attrs = text_attrs(1.0);
    text_width(&attrs, top).max(text_width(&attrs, bottom)).ceil()
}

/// Draw the two right-aligned lines inside a column starting at `origin`.
fn draw_text(top: &str, bottom: &str, origin: NSPoint, col_w: f64, k: f64) {
    let attrs = text_attrs(k);
    for (text, y) in [(top, TOP_Y), (bottom, BOTTOM_Y)] {
        let w = text_width(&attrs, text);
        let at = NSPoint::new(origin.x + col_w * k - w, origin.y + y * k);
        // SAFETY: valid attribute dictionary.
        unsafe { NSString::from_str(text).drawAtPoint_withAttributes(at, Some(&attrs)) };
    }
}

/// Rasterize `draw` (given scale `SCALE`) into a `w` x `h` point CGImage.
fn rasterize(w: f64, h: f64, draw: impl FnOnce(f64)) -> Option<Retained<CGImage>> {
    let (pw, ph) = ((w * SCALE).ceil() as isize, (h * SCALE).ceil() as isize);
    // SAFETY: null planes ask AppKit to allocate the buffer; parameters
    // describe 8-bit RGBA with alpha, meshed.
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(), std::ptr::null_mut(), pw, ph, 8, 4, true, false,
            NSDeviceRGBColorSpace, 0, 0)
    }?;
    let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)?;
    let previous = NSGraphicsContext::currentContext();
    NSGraphicsContext::setCurrentContext(Some(&ctx));
    draw(SCALE);
    ctx.flushGraphics();
    NSGraphicsContext::setCurrentContext(previous.as_deref());
    rep.CGImage()
}

/// Whether the figure is working or resting (the menu bar's logical state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pose {
    Push,
    Rest,
}

/// All frames of one scene, `n` evenly spaced phases. A one-shot scene
/// includes its end state (phase 1) as the last frame.
fn render_frames(scene: Scene, music: bool, n: usize) -> Vec<Retained<CGImage>> {
    let denom = if scene == Scene::RestIntro { (n - 1).max(1) } else { n } as f64;
    (0..n)
        .filter_map(|i| {
            let prims = sisyphus::frame(scene, i as f64 / denom, music);
            rasterize(ICON_W, ICON_H, |k| draw_prims(&prims, NSPoint::new(0.0, 0.0), k))
        })
        .collect()
}

fn as_object(img: &CGImage) -> &AnyObject {
    let cf: &CFType = img;
    cf.as_ref()
}

// ---------------------------------------------------------------------------
// Layer tree on the status bar button
// ---------------------------------------------------------------------------

/// Solid colour layer shaped by a mask layer (template-image behaviour).
struct MaskedLayer {
    color: Retained<CALayer>,
    mask: Retained<CALayer>,
}

impl MaskedLayer {
    fn new(parent: &CALayer) -> Self {
        let color = CALayer::layer();
        let mask = CALayer::layer();
        mask.setContentsScale(SCALE);
        // SAFETY: `mask` is a fresh layer not in any other tree.
        unsafe { color.setMask(Some(&mask)) };
        parent.addSublayer(&color);
        Self { color, mask }
    }

    fn set_frame(&self, x: f64, y: f64, w: f64, h: f64) {
        self.color.setFrame(CGRect::new(CGPoint::new(x, y), CGSize::new(w, h)));
        self.mask.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w, h)));
    }

    fn set_image(&self, img: &CGImage) {
        // SAFETY: CALayer.contents accepts a CGImageRef.
        unsafe { self.mask.setContents(Some(as_object(img))) };
    }
}

struct LayerUi {
    icon: MaskedLayer,
    text: MaskedLayer,
    fps: f64,
    /// Rendered lazily per (scene, music) and kept for reuse.
    frames: HashMap<(Scene, bool), Vec<Retained<CGImage>>>,
    /// (pose, music, animate) currently on screen.
    shown_pose: Option<(Pose, bool, bool)>,
    shown_text: Option<(String, String)>,
    text_w: f64,
    dark: Option<bool>,
    height: f64,
}

impl LayerUi {
    fn new(button: &NSStatusBarButton, fps: f64) -> Self {
        button.setWantsLayer(true);
        let root = button.layer().expect("layer-backed button");
        Self {
            icon: MaskedLayer::new(&root),
            text: MaskedLayer::new(&root),
            fps,
            frames: HashMap::new(),
            shown_pose: None,
            shown_text: None,
            text_w: 0.0,
            dark: None,
            height: 0.0,
        }
    }

    fn frames(&mut self, scene: Scene, music: bool) -> Vec<Retained<CGImage>> {
        let n = ((self.fps * scene.duration()).round() as usize).max(2);
        self.frames.entry((scene, music)).or_insert_with(|| render_frames(scene, music, n)).clone()
    }

    /// Discrete flip-book animation of `frames` on the icon mask.
    fn flipbook(frames: &[Retained<CGImage>], duration: f64, repeat: bool, begin: f64) -> Retained<CAKeyframeAnimation> {
        let objs: Vec<&AnyObject> = frames.iter().map(|f| as_object(f)).collect();
        let anim = CAKeyframeAnimation::animationWithKeyPath(Some(ns_string!("contents")));
        // SAFETY: an array of CGImageRefs is valid for the `contents` key path.
        unsafe { anim.setValues(Some(&NSArray::from_slice(&objs))) };
        // SAFETY: reading QuartzCore's exported constants.
        unsafe {
            anim.setCalculationMode(kCAAnimationDiscrete);
            anim.setFillMode(kCAFillModeForwards);
        }
        anim.setDuration(duration);
        anim.setBeginTime(begin);
        anim.setRepeatCount(if repeat { f32::INFINITY } else { 1.0 });
        anim.setRemovedOnCompletion(false);
        anim
    }

    /// Show `pose` (with or without music). Entering rest plays the
    /// boulder-rolls-away intro once, then loops the night sky; toggling
    /// music while resting goes straight to the sky (the boulder is gone).
    fn set_pose(&mut self, pose: Pose, music: bool, animate: bool) {
        if self.shown_pose == Some((pose, music, animate)) {
            return;
        }
        let entering_rest = pose == Pose::Rest && self.shown_pose.is_none_or(|(p, ..)| p != Pose::Rest);
        self.shown_pose = Some((pose, music, animate));
        let mask = &self.icon.mask;
        mask.removeAllAnimations();
        let loop_scene = if pose == Pose::Push { Scene::Push } else { Scene::RestSky };
        let looped = self.frames(loop_scene, music);
        // Static contents: what remains if animations are off (fps = 0 or
        // offline): a mid-stride push, or the starry sky.
        let still = if pose == Pose::Push { &looped[looped.len() * 3 / 10] } else { &looped[0] };
        self.icon.set_image(still);
        if !animate {
            return;
        }
        let mask = &self.icon.mask;
        let now = mask.convertTime_fromLayer(CACurrentMediaTime(), None);
        let mut loop_begin = now;
        if entering_rest {
            let intro = self.frames(Scene::RestIntro, music);
            let d = Scene::RestIntro.duration();
            let mask = &self.icon.mask;
            mask.addAnimation_forKey(&Self::flipbook(&intro, d, false, now), Some(ns_string!("intro")));
            loop_begin = now + d;
        }
        // Added last, so once it begins it takes precedence over the intro.
        let anim = Self::flipbook(&looped, loop_scene.duration(), true, loop_begin);
        self.icon.mask.addAnimation_forKey(&anim, Some(ns_string!("loop")));
    }

    fn set_text(&mut self, top: String, bottom: String) {
        let key = (top, bottom);
        if self.shown_text.as_ref() == Some(&key) {
            return;
        }
        let w = column_width(&key.0, &key.1);
        if let Some(img) = rasterize(w, ICON_H, |k| draw_text(&key.0, &key.1, NSPoint::new(0.0, 0.0), w, k)) {
            self.text.set_image(&img);
        }
        self.text_w = w;
        self.shown_text = Some(key);
        self.height = 0.0; // force re-layout
    }

    /// Item width in points for the current text.
    fn width(&self) -> f64 {
        PAD + ICON_W + TEXT_GAP + self.text_w + PAD
    }

    /// Centre both layers vertically in a button of height `h`.
    fn layout(&mut self, h: f64) {
        if (h - self.height).abs() < 0.5 {
            return;
        }
        self.height = h;
        let y = ((h - ICON_H) / 2.0).round();
        self.icon.set_frame(PAD, y, ICON_W, ICON_H);
        self.text.set_frame(PAD + ICON_W + TEXT_GAP, y, self.text_w, ICON_H);
    }

    /// Ink colour matching the menu bar: white on dark, near-black on light.
    fn set_dark(&mut self, dark: bool) {
        if self.dark == Some(dark) {
            return;
        }
        self.dark = Some(dark);
        let color = if dark { CGColor::new_generic_gray(1.0, 1.0) } else { CGColor::new_generic_gray(0.0, 0.85) };
        self.icon.color.setBackgroundColor(Some(&color));
        self.text.color.setBackgroundColor(Some(&color));
    }
}

fn is_dark(button: &NSStatusBarButton) -> bool {
    // SAFETY: NSView conforms to NSAppearanceCustomization.
    let appearance: Retained<NSAppearance> = unsafe { msg_send![button, effectiveAppearance] };
    // SAFETY: reading AppKit's exported constants.
    let (aqua, dark) = unsafe { (NSAppearanceNameAqua, NSAppearanceNameDarkAqua) };
    let names = NSArray::from_slice(&[aqua, dark]);
    appearance.bestMatchFromAppearancesWithNames(&names).is_some_and(|n| &*n == dark)
}

/// Run the menu bar on the main thread and the tracker on a worker thread.
pub fn run(
    settings: Settings,
    mut client: AwClient,
    mut categorizer: Categorizer,
    verbose: bool,
    reload: impl FnMut() -> Option<Categorizer> + Send + 'static,
    config_path: Option<String>,
) -> ! {
    let mtm = MainThreadMarker::new().expect("menu bar must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    // Accessory: no Dock icon, no app menu.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let project_names = categorizer.project_names();
    let shared = Arc::new(Mutex::new(MenuState::offline()));
    let (poke_tx, poke_rx) = mpsc::channel::<()>();
    {
        let shared = Arc::clone(&shared);
        let settings = settings.clone();
        std::thread::spawn(move || {
            tracker::run_forever(&mut client, &mut categorizer, &settings, verbose, |res| {
                let state = match res {
                    Ok(snap) => MenuState::from_snapshot(snap),
                    Err(_) => MenuState::offline(),
                };
                *shared.lock().unwrap_or_else(|e| e.into_inner()) = state;
            }, reload, Some(poke_rx))
        });
    }

    let item: Retained<NSStatusItem> =
        NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    let target = MenuTarget::new(mtm, poke_tx, project_names.clone(), &settings.sound, config_path);
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    let info = |title: &str| {
        // SAFETY: no action selector, so no target requirements.
        let it = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm), &NSString::from_str(title), None, &NSString::new())
        };
        it.setEnabled(false);
        menu.addItem(&it);
        it
    };
    let goal_line = info("");
    let project_line = info("");
    let line1 = info("…");
    let line2 = info("");
    let action = |title: &str, action, key: &str| {
        // SAFETY: `target` implements `action` (see MenuTarget) and outlives
        // the menu because `run` never returns.
        let it = unsafe {
            let it = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm), &NSString::from_str(title), Some(action), &NSString::from_str(key));
            it.setTarget(Some(&target));
            it
        };
        menu.addItem(&it);
        it
    };
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    action("🎯 設定專注目標…", sel!(setGoal:), "g");
    // Project submenu: one item per configured project plus "none"; the
    // checkmark follows the manual selection.
    let project_menu = NSMenu::new(mtm);
    project_menu.setAutoenablesItems(false);
    let mut project_items = Vec::new();
    for (i, name) in project_names.iter().map(String::as_str).chain(["不指定專案"]).enumerate() {
        let tag = if i < project_names.len() { i as isize } else { -1 };
        // SAFETY: as for `action` above.
        let it = unsafe {
            let it = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm), &NSString::from_str(name), Some(sel!(setProject:)), &NSString::new());
            it.setTarget(Some(&target));
            it
        };
        it.setTag(tag);
        project_menu.addItem(&it);
        project_items.push((tag, it));
    }
    let project_parent = action("📁 目前專案", sel!(setProject:), "");
    project_parent.setSubmenu(Some(&project_menu));

    // Background sound submenu (+ nested "Frequencies"). There is no volume
    // control on purpose: playback is at unity gain and follows the system
    // volume and mute.
    let item_with = |menu: &NSMenu, title: &str, action, tag: isize| {
        // SAFETY: as for `action` above.
        let it = unsafe {
            let it = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm), &NSString::from_str(title), action, &NSString::new());
            it.setTarget(Some(&target));
            it
        };
        it.setTag(tag);
        menu.addItem(&it);
        it
    };
    let sound_menu = NSMenu::new(mtm);
    sound_menu.setAutoenablesItems(false);
    let mut sound_items = vec![(-1isize, item_with(&sound_menu, "關閉", Some(sel!(playSound:)), -1))];
    sound_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let freq_menu = NSMenu::new(mtm);
    freq_menu.setAutoenablesItems(false);
    let choices = target.ivars().choices.clone();
    let mut freq_added = false;
    for (i, c) in choices.iter().enumerate() {
        if matches!(c.source, sound::Source::Feishin) {
            sound_menu.addItem(&NSMenuItem::separatorItem(mtm));
        }
        let menu_for = if c.group.is_some() { &freq_menu } else { &sound_menu };
        if c.group.is_some() && !freq_added {
            freq_added = true;
            let parent = item_with(&sound_menu, "Frequencies", None, -2);
            parent.setSubmenu(Some(&freq_menu));
        }
        sound_items.push((i as isize, item_with(menu_for, c.title, Some(sel!(playSound:)), i as isize)));
    }
    let sound_parent = action("🎧 背景音", sel!(playSound:), "");
    sound_parent.setSubmenu(Some(&sound_menu));
    let last_sound = RefCell::new((isize::MIN, false));

    // One "rest" entry with a duration submenu; replaced by "end rest"
    // while a forced rest is running.
    let rest_menu = NSMenu::new(mtm);
    rest_menu.setAutoenablesItems(false);
    for (label, minutes) in [("5 分鐘", 5isize), ("10 分鐘", 10), ("15 分鐘", 15), ("30 分鐘", 30), ("不限時（手動結束）", 0)] {
        item_with(&rest_menu, label, Some(sel!(startRest:)), minutes);
    }
    let rest_parent = action("☕ 休息", sel!(startRest:), "");
    rest_parent.setSubmenu(Some(&rest_menu));
    let end_rest = action("🪨 結束休息，回去推石頭", sel!(endRest:), "e");
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    action("📊 報表與分類…", sel!(openReport:), "r");
    action("開啟 ActivityWatch", sel!(openDashboard:), "d");
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    // SAFETY: `terminate:` is handled by NSApplication via the responder chain.
    let quit = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm), &NSString::from_str("結束 aw-timer"), Some(sel!(terminate:)),
            &NSString::from_str("q"))
    };
    menu.addItem(&quit);
    // No permanent menu: clicks go to `statusClicked:` (see MenuTarget).
    *target.ivars().status_item.borrow_mut() = Some(item.clone());
    *target.ivars().menu.borrow_mut() = Some(menu.clone());

    let button = item.button(mtm).expect("status item has a button");
    // SAFETY: `target` implements statusClicked: and lives forever.
    unsafe {
        button.setTarget(Some(&target));
        button.setAction(Some(sel!(statusClicked:)));
    }
    button.sendActionOn(NSEventMask::LeftMouseUp | NSEventMask::RightMouseUp);
    let animate = settings.menubar_fps > 0.0;
    let ui = RefCell::new(LayerUi::new(&button, settings.menubar_fps.clamp(1.0, 30.0)));
    let last_lines = RefCell::new([String::new(), String::new(), "?".to_owned(), "?".to_owned()]);

    // 1 Hz bookkeeping only; Core Animation plays the frames on its own.
    let tick = RcBlock::new(move |_t: NonNull<NSTimer>| {
        let state = shared.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let pose = if state.phase == Phase::Working { Pose::Push } else { Pose::Rest };
        // Music is shown by the figure: singing while pushing, headphones
        // while resting.
        let music = target.ivars().player.borrow().is_playing();
        let (top, bottom) = state.time_text();
        let mut ui = ui.borrow_mut();

        CATransaction::begin();
        CATransaction::setDisableActions(true); // no implicit cross-fades
        ui.set_pose(pose, music, animate && state.phase != Phase::Offline);
        ui.set_text(top, bottom);
        if (item.length() - ui.width()).abs() > 0.5 {
            item.setLength(ui.width());
        }
        ui.layout(button.bounds().size.height);
        ui.set_dark(is_dark(&button));
        CATransaction::commit();

        // Sound checkmark and title: selected source, and whether paused.
        {
            let player = target.ivars().player.borrow();
            let selected = player
                .current()
                .and_then(|cur| choices.iter().position(|c| &c.source == cur))
                .map_or(-1, |i| i as isize);
            let paused = selected >= 0 && !player.is_playing();
            if *last_sound.borrow() != (selected, paused) {
                *last_sound.borrow_mut() = (selected, paused);
                for (tag, it) in &sound_items {
                    it.setState(if *tag == selected { NSControlStateValueOn } else { NSControlStateValueOff });
                }
                let name = usize::try_from(selected).ok().and_then(|i| choices.get(i)).map_or("關閉", |c| c.title);
                let suffix = if paused { "（暫停，右鍵繼續）" } else { "" };
                sound_parent.setTitle(&NSString::from_str(&format!("🎧 背景音：{name}{suffix}")));
            }
        }
        let resting = state.rest.is_some();
        rest_parent.setHidden(resting);
        end_rest.setHidden(!resting);
        let goal_text = state.goal.as_deref().map(|g| format!("🎯 目標：{g}"));
        let manual = state.manual_project.as_deref();
        let proj_text = match (manual, state.projects_line.is_empty()) {
            (None, true) => String::new(),
            (m, _) => format!(
                "📁 {}{}",
                m.map_or(String::new(), |m| format!("目前：{m}　")),
                if state.projects_line.is_empty() { String::new() } else { format!("今日 {}", state.projects_line) }
            ),
        };
        let mut last = last_lines.borrow_mut();
        let shown = [
            state.lines[0].clone(),
            state.lines[1].clone(),
            goal_text.clone().unwrap_or_default(),
            proj_text.clone(),
        ];
        if *last != shown {
            project_line.setTitle(&NSString::from_str(&proj_text));
            project_line.setHidden(proj_text.is_empty());
            let selected = manual.and_then(|m| project_names.iter().position(|n| n == m)).map_or(-1, |i| i as isize);
            for (tag, it) in &project_items {
                let on = if *tag == selected { NSControlStateValueOn } else { NSControlStateValueOff };
                it.setState(on);
            }
            project_parent.setTitle(&NSString::from_str(&format!("📁 目前專案：{}", manual.unwrap_or("未指定"))));
            line1.setTitle(&NSString::from_str(&state.lines[0]));
            line2.setTitle(&NSString::from_str(&state.lines[1]));
            goal_line.setTitle(&NSString::from_str(goal_text.as_deref().unwrap_or("")));
            goal_line.setHidden(goal_text.is_none());
            button.setToolTip(goal_text.as_deref().map(NSString::from_str).as_deref());
            *target.ivars().goal.borrow_mut() = state.goal.clone();
            *last = shown;
        }
    });
    // SAFETY: scheduled on the main run loop, which `app.run()` drives.
    let _timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.0, true, &tick) };

    app.run();
    std::process::exit(0)
}

/// Render a grid of frames to a PNG (white background, scale `k`) for review.
pub fn preview(path: &str, k: f64) -> Result<(), String> {
    let _mtm = MainThreadMarker::new().ok_or("must run on the main thread")?;
    let cols = 6;
    // Rows: push, push + music, rest intro, sky + music (meteor phases).
    let rows: [(Scene, bool, [f64; 6]); 4] = [
        (Scene::Push, false, [0.0, 0.17, 0.33, 0.5, 0.67, 0.83]),
        (Scene::Push, true, [0.0, 0.17, 0.33, 0.5, 0.67, 0.83]),
        (Scene::RestIntro, false, [0.0, 0.2, 0.4, 0.6, 0.8, 1.0]),
        (Scene::RestSky, true, [0.0, 0.19, 0.21, 0.23, 0.65, 0.66]),
    ];
    let samples: Vec<(Scene, bool, f64)> =
        rows.iter().flat_map(|&(sc, m, ps)| ps.into_iter().map(move |p| (sc, m, p))).collect();
    let n_rows = rows.len();
    let (cell_w, cell_h) = ((ICON_W + 30.0) * k, (ICON_H + 4.0) * k);
    let size = NSSize::new(cell_w * cols as f64, cell_h * n_rows as f64);
    let block = RcBlock::new(move |rect: NSRect| -> Bool {
        NSColor::whiteColor().setFill();
        NSBezierPath::fillRect(rect);
        for (i, &(scene, music, phase)) in samples.iter().enumerate() {
            let (col, row) = ((i % cols) as f64, (n_rows - 1 - i / cols) as f64);
            let origin = NSPoint::new(col * cell_w + k, row * cell_h + 2.0 * k);
            let (top, bottom) = if scene == Scene::Push { ("25m", "1.4h") } else { ("0m", "3.2h") };
            draw_prims(&sisyphus::frame(scene, phase, music), origin, k);
            let text_origin = NSPoint::new(origin.x + (ICON_W + TEXT_GAP) * k, origin.y);
            draw_text(top, bottom, text_origin, column_width(top, bottom), k);
        }
        Bool::YES
    });
    let image = NSImage::imageWithSize_flipped_drawingHandler(size, false, &block);
    let tiff = image.TIFFRepresentation().ok_or("no TIFF representation")?;
    let rep = NSBitmapImageRep::imageRepWithData(&tiff).ok_or("bad bitmap")?;
    // SAFETY: empty property dictionary is valid.
    let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }
        .ok_or("PNG encoding failed")?;
    std::fs::write(path, png.to_vec()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(streak: f64, total: f64) -> MenuState {
        MenuState { streak_secs: streak, total_secs: total, ..MenuState::offline() }
    }

    #[test]
    fn time_text_minutes_and_tenth_hours() {
        let mut s = state(25.0 * 60.0 + 59.0, 3.0 * 3600.0 + 17.0 * 60.0);
        s.phase = Phase::Working;
        assert_eq!(s.time_text(), ("25m".into(), "3.2h".into()));
        s.total_secs = 59.0 * 60.0;
        assert_eq!(s.time_text().1, "0.9h");
        s.phase = Phase::Offline;
        assert_eq!(s.time_text(), ("--".into(), "--".into()));
    }
}
