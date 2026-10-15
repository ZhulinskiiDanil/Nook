//! Preview hotkey state machine (spec §7.1). Pure: fed by the low-level keyboard hook,
//! returns commands and whether to swallow the key. No Win32 here.
//!
//! - Show: LCtrl+LAlt held alone longer than `hold_ms` while a plan is pending.
//! - Releasing either key hides instantly.
//! - AltGr (= fake LCtrl + RAlt with the same timestamp) never counts.
//! - Any other key while LCtrl/LAlt are down = someone else's shortcut: poison until released.
//! - Apply: two clean short taps within 400 ms, or Enter while the preview is visible;
//!   only if this plan has been previewed at least once. Reject: Esc while visible.
//! - Enter/Esc are swallowed only while the preview is on screen.

pub const VK_RETURN: u32 = 0x0D;
pub const VK_ESCAPE: u32 = 0x1B;
pub const VK_LCONTROL: u32 = 0xA2;
pub const VK_LMENU: u32 = 0xA4;
pub const VK_RMENU: u32 = 0xA5;

pub const DOUBLE_TAP_MS: u32 = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    Show,
    Hide,
    Apply,
    Reject,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Ctx {
    pub has_plan: bool,
    pub previewed: bool,
    /// Fullscreen app / game / presentation (SHQueryUserNotificationState).
    pub suppressed: bool,
}

#[derive(Debug, Default)]
pub struct Machine {
    pub hold_ms: u32,
    lctrl: bool,
    lctrl_time: u32,
    lalt: bool,
    poisoned: bool,
    combo_since: Option<u32>,
    showing: bool,
    last_tap_end: Option<u32>,
    swallow_up: Option<u32>,
}

impl Machine {
    pub fn new(hold_ms: u32) -> Self {
        Self {
            hold_ms,
            ..Default::default()
        }
    }

    pub fn showing(&self) -> bool {
        self.showing
    }

    /// True while a hold timer is needed.
    pub fn combo_active(&self) -> bool {
        self.combo_since.is_some() && !self.showing
    }

    fn maybe_start(&mut self, time: u32) {
        if self.lctrl && self.lalt && !self.poisoned && self.combo_since.is_none() {
            self.combo_since = Some(time);
        }
    }

    fn hide(&mut self, out: &mut Vec<Cmd>) {
        if self.showing {
            self.showing = false;
            out.push(Cmd::Hide);
        }
    }

    fn poison(&mut self, out: &mut Vec<Cmd>) {
        self.poisoned = true;
        self.combo_since = None;
        self.last_tap_end = None;
        self.hide(out);
    }

    /// One key event. Returns (commands, swallow).
    pub fn on_key(&mut self, vk: u32, down: bool, time: u32, ctx: Ctx) -> (Vec<Cmd>, bool) {
        let mut out = Vec::new();
        match (vk, down) {
            (VK_LCONTROL, true) => {
                if !self.lctrl {
                    self.lctrl = true;
                    self.lctrl_time = time;
                    self.maybe_start(time);
                }
            }
            (VK_LMENU, true) => {
                if !self.lalt {
                    self.lalt = true;
                    self.maybe_start(time);
                }
            }
            (VK_LCONTROL | VK_LMENU, false) => {
                let was_combo = self.combo_since;
                let was_showing = self.showing;
                self.hide(&mut out);
                if let Some(start) = was_combo
                    && !self.poisoned
                {
                    let short = time.wrapping_sub(start) < self.hold_ms && !was_showing;
                    match self.last_tap_end {
                        Some(prev) if short && time.wrapping_sub(prev) <= DOUBLE_TAP_MS => {
                            self.last_tap_end = None;
                            if ctx.has_plan && ctx.previewed && !ctx.suppressed {
                                out.push(Cmd::Apply);
                            }
                        }
                        _ => self.last_tap_end = short.then_some(time),
                    }
                }
                self.combo_since = None;
                if vk == VK_LCONTROL {
                    self.lctrl = false;
                } else {
                    self.lalt = false;
                }
                if !self.lctrl && !self.lalt {
                    self.poisoned = false;
                }
            }
            (VK_RMENU, true) => {
                // AltGr: Windows sends a fake LCtrl with the same timestamp right before RAlt.
                if self.lctrl && time.wrapping_sub(self.lctrl_time) <= 1 && !self.lalt {
                    self.lctrl = false;
                    self.combo_since = None;
                    self.last_tap_end = None;
                } else if self.lctrl || self.lalt {
                    self.poison(&mut out);
                }
            }
            (VK_RETURN, true) if self.showing => {
                self.hide(&mut out);
                if ctx.has_plan && ctx.previewed {
                    out.push(Cmd::Apply);
                }
                self.swallow_up = Some(VK_RETURN);
                // Ignore the rest of this hold.
                self.poisoned = true;
                return (out, true);
            }
            (VK_ESCAPE, true) if self.showing => {
                self.hide(&mut out);
                out.push(Cmd::Reject);
                self.swallow_up = Some(VK_ESCAPE);
                self.poisoned = true;
                return (out, true);
            }
            (k, false) if self.swallow_up == Some(k) => {
                self.swallow_up = None;
                return (out, true);
            }
            (_, true) => {
                if self.lctrl || self.lalt {
                    self.poison(&mut out);
                } else {
                    self.last_tap_end = None;
                }
            }
            (_, false) => {}
        }
        (out, false)
    }

    /// Timer tick while the combo is held.
    pub fn on_tick(&mut self, now: u32, ctx: Ctx) -> Vec<Cmd> {
        match self.combo_since {
            Some(t) if !self.showing && !self.poisoned && now.wrapping_sub(t) >= self.hold_ms => {
                if ctx.has_plan && !ctx.suppressed {
                    self.showing = true;
                    vec![Cmd::Show]
                } else {
                    vec![]
                }
            }
            _ => vec![],
        }
    }

    /// The plan went away (applied elsewhere, cancelled): hide if visible.
    pub fn force_hide(&mut self) -> Vec<Cmd> {
        let mut out = Vec::new();
        self.hide(&mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: Ctx = Ctx {
        has_plan: true,
        previewed: true,
        suppressed: false,
    };
    const UNSEEN: Ctx = Ctx {
        has_plan: true,
        previewed: false,
        suppressed: false,
    };
    const NONE: Ctx = Ctx {
        has_plan: false,
        previewed: false,
        suppressed: false,
    };
    const GAME: Ctx = Ctx {
        has_plan: true,
        previewed: true,
        suppressed: true,
    };

    struct T {
        m: Machine,
        log: Vec<Cmd>,
    }

    impl T {
        fn new() -> Self {
            Self {
                m: Machine::new(150),
                log: vec![],
            }
        }
        fn k(&mut self, vk: u32, down: bool, t: u32, c: Ctx) -> bool {
            let (cmds, sw) = self.m.on_key(vk, down, t, c);
            self.log.extend(cmds);
            sw
        }
        fn tick(&mut self, t: u32, c: Ctx) {
            let cmds = self.m.on_tick(t, c);
            self.log.extend(cmds);
        }
        fn hold(&mut self, t0: u32, ms: u32, c: Ctx) {
            self.k(VK_LCONTROL, true, t0, c);
            self.k(VK_LMENU, true, t0 + 5, c);
            let mut t = t0 + 5;
            while t < t0 + 5 + ms {
                t += 25;
                self.tick(t, c);
            }
        }
        fn release(&mut self, t: u32, c: Ctx) {
            self.k(VK_LMENU, false, t, c);
            self.k(VK_LCONTROL, false, t + 2, c);
        }
    }

    #[test]
    fn hold_shows_release_hides() {
        let mut t = T::new();
        t.hold(1000, 100, PLAN);
        assert!(t.log.is_empty(), "not before 150 ms");
        t.tick(1200, PLAN);
        assert_eq!(t.log, vec![Cmd::Show]);
        t.release(1300, PLAN);
        assert_eq!(t.log, vec![Cmd::Show, Cmd::Hide]);
    }

    #[test]
    fn no_plan_or_fullscreen_does_nothing() {
        for c in [NONE, GAME] {
            let mut t = T::new();
            t.hold(0, 500, c);
            t.release(600, c);
            assert!(t.log.is_empty(), "{c:?}: {:?}", t.log);
        }
    }

    #[test]
    fn altgr_never_triggers() {
        // Polish ą: AltGr down = fake LCtrl + RAlt (same time), 'a', then ups.
        let mut t = T::new();
        for i in 0..3 {
            let b = i * 1000;
            t.k(VK_LCONTROL, true, b, PLAN);
            t.k(VK_RMENU, true, b, PLAN);
            t.k(0x41, true, b + 50, PLAN);
            t.tick(b + 300, PLAN);
            t.k(0x41, false, b + 120, PLAN);
            t.k(VK_LCONTROL, false, b + 400, PLAN);
            t.k(VK_RMENU, false, b + 400, PLAN);
        }
        // AltGr held alone for a long time, with LAlt never pressed.
        t.k(VK_LCONTROL, true, 9000, PLAN);
        t.k(VK_RMENU, true, 9000, PLAN);
        t.tick(9500, PLAN);
        assert!(t.log.is_empty(), "{:?}", t.log);
    }

    #[test]
    fn other_key_means_foreign_shortcut() {
        let mut t = T::new();
        t.k(VK_LCONTROL, true, 0, PLAN);
        t.k(VK_LMENU, true, 5, PLAN);
        t.k(0x2E, true, 50, PLAN); // Ctrl+Alt+Del-like
        t.tick(400, PLAN);
        assert!(t.log.is_empty());
        // Showing, then another key → hide.
        let mut t = T::new();
        t.hold(0, 300, PLAN);
        t.k(0x54, true, 400, PLAN); // Ctrl+Alt+T
        assert_eq!(t.log, vec![Cmd::Show, Cmd::Hide]);
        t.tick(600, PLAN);
        assert_eq!(t.log.len(), 2, "stays hidden until keys are released");
    }

    #[test]
    fn enter_applies_esc_rejects_and_are_swallowed_only_in_preview() {
        let mut t = T::new();
        assert!(
            !t.k(VK_RETURN, true, 0, PLAN),
            "Enter is not swallowed without preview"
        );
        assert!(!t.k(VK_RETURN, false, 10, PLAN));
        t.hold(100, 300, PLAN);
        assert!(t.k(VK_RETURN, true, 500, PLAN));
        assert!(
            t.k(VK_RETURN, false, 550, PLAN),
            "matching key-up swallowed too"
        );
        assert_eq!(t.log, vec![Cmd::Show, Cmd::Hide, Cmd::Apply]);

        let mut t = T::new();
        t.hold(0, 300, PLAN);
        assert!(t.k(VK_ESCAPE, true, 400, PLAN));
        assert_eq!(t.log, vec![Cmd::Show, Cmd::Hide, Cmd::Reject]);
        t.release(500, PLAN);
        assert_eq!(t.log.len(), 3, "release after Esc changes nothing");
    }

    #[test]
    fn double_tap_applies_only_after_preview() {
        let tap = |t: &mut T, at: u32, c: Ctx| {
            t.k(VK_LCONTROL, true, at, c);
            t.k(VK_LMENU, true, at + 10, c);
            t.k(VK_LMENU, false, at + 70, c);
            t.k(VK_LCONTROL, false, at + 80, c);
        };
        let mut t = T::new();
        tap(&mut t, 0, UNSEEN);
        tap(&mut t, 200, UNSEEN);
        assert!(t.log.is_empty(), "never previewed → no apply");

        let mut t = T::new();
        tap(&mut t, 0, PLAN);
        tap(&mut t, 200, PLAN);
        assert_eq!(t.log, vec![Cmd::Apply]);

        let mut t = T::new();
        tap(&mut t, 0, PLAN);
        tap(&mut t, 700, PLAN);
        assert!(t.log.is_empty(), "too slow");

        // Long preview hold, then one quick tap: not a double tap.
        let mut t = T::new();
        t.hold(0, 400, PLAN);
        t.release(450, PLAN);
        tap(&mut t, 500, PLAN);
        assert_eq!(t.log, vec![Cmd::Show, Cmd::Hide]);
    }

    #[test]
    fn key_repeat_does_not_restart_hold() {
        let mut t = T::new();
        t.k(VK_LCONTROL, true, 0, PLAN);
        t.k(VK_LMENU, true, 0, PLAN);
        for ms in (30..140).step_by(30) {
            t.k(VK_LCONTROL, true, ms, PLAN); // auto-repeat
            t.k(VK_LMENU, true, ms, PLAN);
        }
        t.tick(160, PLAN);
        assert_eq!(t.log, vec![Cmd::Show]);
    }
}
