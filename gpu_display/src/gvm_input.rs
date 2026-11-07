// Copyright 2026 iceland gvm. Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! iceland gvm: input modes of the crosvm display window (iceland-vms 0.2-6).
//!
//! `CROSVM_GVM_INPUT_MODE` picks how the window's pointer and touch reach the guest:
//!
//! * `touch` (default, the upstream behaviour plus real multi-finger touch): the left button and every finger
//!   go to the guest's multi-touch screen; the guest's own gestures apply.
//! * `tablet`: an absolute pointer. The mouse moves the guest pointer (hover), buttons and the wheel pass
//!   through. A finger tap clicks where it lands, a drag drags with the left button, a long press (>= 600 ms
//!   without moving) is a right click, two fingers scroll, a two-finger tap is a right click.
//! * `touchpad`: the touch screen works like a laptop touchpad. A finger moves the guest pointer relatively,
//!   a tap is a left click, a long press or a two-finger tap a right click, two fingers scroll. The mouse stays
//!   absolute (tablet device).
//!
//! Coordinates are buffer pixels (display_wl.c scales surface coordinates by the buffer scale). This module
//! has no crosvm dependencies so its gesture logic is unit-tested on its own.

pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;

const TAP_MAX_MS: u64 = 300;
const LONG_PRESS_MS: u64 = 600;
const MOVE_THRESHOLD: i32 = 16; // buffer px (about 8 logical px at scale 2)
const SCROLL_STEP: i32 = 40; // buffer px of two-finger travel per wheel notch
const AXIS_STEP: f64 = 10.0; // wl_pointer axis units per wheel notch
const MAX_SLOTS: usize = 10;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Touch,
    Tablet,
    Touchpad,
}

impl Mode {
    pub fn parse(s: Option<&str>) -> Mode {
        match s {
            Some("tablet") => Mode::Tablet,
            Some("touchpad") => Mode::Touchpad,
            _ => Mode::Touch,
        }
    }

    pub fn from_env() -> Mode {
        Mode::parse(std::env::var("CROSVM_GVM_INPUT_MODE").ok().as_deref())
    }
}

/// One output event; `*Syn` ends an evdev frame inside a report (the report itself ends with one too).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Out {
    MtDown { slot: i32, id: i32, x: i32, y: i32 },
    MtMove { slot: i32, x: i32, y: i32 },
    MtUp { slot: i32, last: bool },
    AbsMove { x: i32, y: i32 },
    AbsButton { code: u16, down: bool },
    AbsWheel { v: i32, h: i32 },
    AbsSyn,
    RelMove { dx: i32, dy: i32 },
    RelButton { code: u16, down: bool },
    RelWheel { v: i32 },
    RelSyn,
}

#[derive(Clone, Copy, Debug)]
struct Finger {
    id: i32,
    slot: i32,
    x: i32,
    y: i32,
    x0: i32,
    y0: i32,
}

pub struct Mapper {
    pub mode: Mode,
    lbutton: bool,
    fingers: Vec<Finger>,
    t0: u64,
    moved: bool,
    dragging: bool,
    two: bool,
    scrolled: bool,
    centroid: (i32, i32),
    scroll_acc: (i32, i32),
    axis_acc: (f64, f64),
    mt_next_id: i32,
}

fn click(code: u16, abs: bool) -> Vec<Out> {
    if abs {
        vec![Out::AbsButton { code, down: true }, Out::AbsSyn, Out::AbsButton { code, down: false }]
    } else {
        vec![Out::RelButton { code, down: true }, Out::RelSyn, Out::RelButton { code, down: false }]
    }
}

impl Mapper {
    pub fn new(mode: Mode) -> Mapper {
        Mapper {
            mode,
            lbutton: false,
            fingers: Vec::new(),
            t0: 0,
            moved: false,
            dragging: false,
            two: false,
            scrolled: false,
            centroid: (0, 0),
            scroll_acc: (0, 0),
            axis_acc: (0.0, 0.0),
            mt_next_id: 0,
        }
    }

    fn centroid_now(&self) -> (i32, i32) {
        let n = self.fingers.len().max(1) as i32;
        let sx: i32 = self.fingers.iter().map(|f| f.x).sum();
        let sy: i32 = self.fingers.iter().map(|f| f.y).sum();
        (sx / n, sy / n)
    }

    // ---- mouse (wl_pointer) ----------------------------------------------------------------
    pub fn pointer_move(&mut self, x: i32, y: i32) -> Vec<Out> {
        match self.mode {
            Mode::Touch => {
                if self.lbutton {
                    vec![Out::MtMove { slot: 0, x, y }]
                } else {
                    vec![]
                }
            }
            _ => vec![Out::AbsMove { x, y }],
        }
    }

    pub fn pointer_button(&mut self, code: u16, down: bool, x: i32, y: i32) -> Vec<Out> {
        match self.mode {
            Mode::Touch => {
                if code != BTN_LEFT {
                    return vec![];
                }
                self.lbutton = down;
                if down {
                    let id = self.mt_next_id;
                    self.mt_next_id = self.mt_next_id.wrapping_add(1) & 0xffff;
                    vec![Out::MtDown { slot: 0, id, x, y }]
                } else {
                    vec![Out::MtUp { slot: 0, last: true }]
                }
            }
            _ => vec![Out::AbsMove { x, y }, Out::AbsButton { code, down }],
        }
    }

    /// `axis`: 0 vertical, 1 horizontal (wl_pointer); `value` in wl units (positive = down / right).
    pub fn pointer_axis(&mut self, axis: u32, value: f64) -> Vec<Out> {
        if self.mode == Mode::Touch {
            return vec![];
        }
        let acc = if axis == 0 { &mut self.axis_acc.0 } else { &mut self.axis_acc.1 };
        *acc += value;
        let steps = (*acc / AXIS_STEP).trunc();
        if steps == 0.0 {
            return vec![];
        }
        *acc -= steps * AXIS_STEP;
        let n = steps as i32;
        if axis == 0 {
            vec![Out::AbsWheel { v: -n, h: 0 }] // REL_WHEEL positive = up
        } else {
            vec![Out::AbsWheel { v: 0, h: n }]
        }
    }

    // ---- touch (wl_touch) --------------------------------------------------------------------
    pub fn finger_down(&mut self, id: i32, x: i32, y: i32, now_ms: u64) -> Vec<Out> {
        if self.fingers.len() >= MAX_SLOTS || self.fingers.iter().any(|f| f.id == id) {
            return vec![];
        }
        let slot = (0..MAX_SLOTS as i32).find(|s| !self.fingers.iter().any(|f| f.slot == *s)).unwrap_or(0);
        self.fingers.push(Finger { id, slot, x, y, x0: x, y0: y });
        if self.mode == Mode::Touch {
            let tid = self.mt_next_id;
            self.mt_next_id = self.mt_next_id.wrapping_add(1) & 0xffff;
            return vec![Out::MtDown { slot, id: tid, x, y }];
        }
        let mut out = vec![];
        if self.fingers.len() == 1 {
            self.t0 = now_ms;
            self.moved = false;
            self.dragging = false;
            self.two = false;
            self.scrolled = false;
            if self.mode == Mode::Tablet {
                out.push(Out::AbsMove { x, y }); // the pointer follows the finger; no click yet
            }
        } else if self.fingers.len() == 2 {
            if self.dragging {
                out.push(Out::AbsButton { code: BTN_LEFT, down: false });
                self.dragging = false;
            }
            self.two = true;
            self.centroid = self.centroid_now();
            self.scroll_acc = (0, 0);
        }
        out
    }

    pub fn finger_motion(&mut self, id: i32, x: i32, y: i32, _now_ms: u64) -> Vec<Out> {
        let Some(i) = self.fingers.iter().position(|f| f.id == id) else {
            return vec![];
        };
        let (px, py) = (self.fingers[i].x, self.fingers[i].y);
        self.fingers[i].x = x;
        self.fingers[i].y = y;
        let f = self.fingers[i];
        if self.mode == Mode::Touch {
            return vec![Out::MtMove { slot: f.slot, x, y }];
        }
        if (x - f.x0).abs() > MOVE_THRESHOLD || (y - f.y0).abs() > MOVE_THRESHOLD {
            self.moved = true;
        }
        if self.two {
            if self.fingers.len() < 2 {
                return vec![];
            }
            let c = self.centroid_now();
            self.scroll_acc.0 += c.0 - self.centroid.0;
            self.scroll_acc.1 += c.1 - self.centroid.1;
            self.centroid = c;
            let (sv, sh) = (self.scroll_acc.1 / SCROLL_STEP, self.scroll_acc.0 / SCROLL_STEP);
            if sv == 0 && sh == 0 {
                return vec![];
            }
            self.scroll_acc.1 -= sv * SCROLL_STEP;
            self.scroll_acc.0 -= sh * SCROLL_STEP;
            self.scrolled = true;
            // natural scrolling: the content follows the fingers (fingers up = scroll down = negative wheel)
            return match self.mode {
                // p3: horizontal two-finger scroll goes to the tablet device's REL_HWHEEL (the virtio mouse has
                // no horizontal wheel); one axis per report, vertical first
                Mode::Touchpad => {
                    if sv != 0 {
                        vec![Out::RelWheel { v: sv }]
                    } else if sh != 0 {
                        vec![Out::AbsWheel { v: 0, h: -sh }]
                    } else {
                        vec![]
                    }
                }
                _ => vec![Out::AbsWheel { v: sv, h: -sh }],
            };
        }
        match self.mode {
            Mode::Tablet => {
                if !self.moved {
                    return vec![];
                }
                if !self.dragging {
                    self.dragging = true;
                    return vec![
                        Out::AbsMove { x: f.x0, y: f.y0 },
                        Out::AbsButton { code: BTN_LEFT, down: true },
                        Out::AbsSyn,
                        Out::AbsMove { x, y },
                    ];
                }
                vec![Out::AbsMove { x, y }]
            }
            Mode::Touchpad => {
                let (dx, dy) = (x - px, y - py);
                if dx == 0 && dy == 0 {
                    vec![]
                } else {
                    vec![Out::RelMove { dx, dy }]
                }
            }
            Mode::Touch => vec![],
        }
    }

    pub fn finger_up(&mut self, id: i32, now_ms: u64) -> Vec<Out> {
        let Some(i) = self.fingers.iter().position(|f| f.id == id) else {
            return vec![];
        };
        let f = self.fingers.remove(i);
        if self.mode == Mode::Touch {
            return vec![Out::MtUp { slot: f.slot, last: self.fingers.is_empty() }];
        }
        if !self.fingers.is_empty() {
            return vec![]; // the gesture ends with the last finger
        }
        let held = now_ms.saturating_sub(self.t0);
        let abs = self.mode == Mode::Tablet;
        if self.two {
            self.two = false;
            // a two-finger tap (no scroll, short) is a right click
            if !self.scrolled && !self.moved && held < LONG_PRESS_MS {
                return click(BTN_RIGHT, abs);
            }
            return vec![];
        }
        if self.dragging {
            self.dragging = false;
            return vec![Out::AbsButton { code: BTN_LEFT, down: false }];
        }
        if self.moved {
            return vec![];
        }
        let code = if held >= LONG_PRESS_MS {
            BTN_RIGHT
        } else if held <= TAP_MAX_MS || abs {
            BTN_LEFT
        } else {
            return vec![]; // touchpad: a medium press without movement is nothing
        };
        let mut out = vec![];
        if abs {
            out.push(Out::AbsMove { x: f.x0, y: f.y0 });
        }
        out.extend(click(code, abs));
        out
    }

    pub fn finger_cancel(&mut self) -> Vec<Out> {
        let mut out = vec![];
        if self.mode == Mode::Touch {
            let n = self.fingers.len();
            for (k, f) in self.fingers.iter().enumerate() {
                out.push(Out::MtUp { slot: f.slot, last: k + 1 == n });
            }
        } else if self.dragging {
            out.push(Out::AbsButton { code: BTN_LEFT, down: false });
        }
        self.fingers.clear();
        self.dragging = false;
        self.two = false;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tablet_tap_is_left_click_where_it_lands() {
        let mut m = Mapper::new(Mode::Tablet);
        assert_eq!(m.finger_down(1, 100, 200, 0), vec![Out::AbsMove { x: 100, y: 200 }]);
        let o = m.finger_up(1, 100);
        assert_eq!(o[0], Out::AbsMove { x: 100, y: 200 });
        assert_eq!(&o[1..], &click(BTN_LEFT, true)[..]);
    }

    #[test]
    fn tablet_long_press_is_right_click() {
        let mut m = Mapper::new(Mode::Tablet);
        m.finger_down(1, 10, 10, 0);
        m.finger_motion(1, 14, 12, 300); // below the threshold
        let o = m.finger_up(1, 700);
        assert!(o.contains(&Out::AbsButton { code: BTN_RIGHT, down: true }));
    }

    #[test]
    fn tablet_drag_holds_left() {
        let mut m = Mapper::new(Mode::Tablet);
        m.finger_down(1, 10, 10, 0);
        let o = m.finger_motion(1, 60, 10, 50);
        assert!(o.contains(&Out::AbsButton { code: BTN_LEFT, down: true }));
        assert_eq!(m.finger_up(1, 90), vec![Out::AbsButton { code: BTN_LEFT, down: false }]);
    }

    #[test]
    fn two_finger_scroll_and_tap() {
        let mut m = Mapper::new(Mode::Tablet);
        m.finger_down(1, 100, 300, 0);
        m.finger_down(2, 200, 300, 10);
        m.finger_motion(1, 100, 220, 20);
        let o = m.finger_motion(2, 200, 220, 30);
        assert_eq!(o, vec![Out::AbsWheel { v: -1, h: 0 }]); // centroid up 40 px = one notch (natural: down)
        m.finger_up(1, 40);
        assert_eq!(m.finger_up(2, 50), vec![]); // scrolled: no click
        m.finger_down(3, 100, 100, 100);
        m.finger_down(4, 200, 100, 110);
        m.finger_up(3, 150);
        assert_eq!(m.finger_up(4, 160), click(BTN_RIGHT, true));
    }

    #[test]
    fn touchpad_moves_relatively_and_taps() {
        let mut m = Mapper::new(Mode::Touchpad);
        assert_eq!(m.finger_down(1, 500, 500, 0), vec![]);
        assert_eq!(m.finger_motion(1, 530, 490, 20), vec![Out::RelMove { dx: 30, dy: -10 }]);
        assert_eq!(m.finger_up(1, 40), vec![]); // moved: no click
        m.finger_down(2, 500, 500, 100);
        assert_eq!(m.finger_up(2, 180), click(BTN_LEFT, false));
        m.finger_down(3, 500, 500, 200);
        assert_eq!(m.finger_up(3, 900), click(BTN_RIGHT, false));
    }

    #[test]
    fn touch_mode_forwards_fingers_and_left_button() {
        let mut m = Mapper::new(Mode::Touch);
        assert!(matches!(m.finger_down(7, 1, 2, 0)[0], Out::MtDown { slot: 0, x: 1, y: 2, .. }));
        assert!(matches!(m.finger_down(8, 3, 4, 0)[0], Out::MtDown { slot: 1, .. }));
        assert_eq!(m.finger_up(7, 5), vec![Out::MtUp { slot: 0, last: false }]);
        assert_eq!(m.finger_up(8, 6), vec![Out::MtUp { slot: 1, last: true }]);
        assert_eq!(m.pointer_move(5, 5), vec![]);
        assert!(matches!(m.pointer_button(BTN_LEFT, true, 5, 5)[0], Out::MtDown { .. }));
        assert_eq!(m.pointer_button(BTN_RIGHT, true, 5, 5), vec![]);
    }

    #[test]
    fn touchpad_horizontal_scroll_uses_the_tablet_hwheel() {
        let mut m = Mapper::new(Mode::Touchpad);
        m.finger_down(1, 100, 300, 0);
        m.finger_down(2, 100, 400, 10);
        // each finger's 80 px moves the centroid 40 px = one notch (natural: fingers right = content right)
        assert_eq!(m.finger_motion(1, 180, 300, 20), vec![Out::AbsWheel { v: 0, h: -1 }]);
        assert_eq!(m.finger_motion(2, 180, 400, 30), vec![Out::AbsWheel { v: 0, h: -1 }]);
    }

    #[test]
    fn wheel_notches() {
        let mut m = Mapper::new(Mode::Tablet);
        assert_eq!(m.pointer_axis(0, 5.0), vec![]);
        assert_eq!(m.pointer_axis(0, 5.0), vec![Out::AbsWheel { v: -1, h: 0 }]);
        assert_eq!(m.pointer_axis(1, -20.0), vec![Out::AbsWheel { v: 0, h: -2 }]);
    }
}
