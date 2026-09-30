//! Stable gamepad codes for `EventKind::Button` / `EventKind::Axis`.
//!
//! Names follow gilrs' SDL-style layout (South = A on Xbox / Cross on
//! PlayStation). These numbers are part of the dataset format: never renumber,
//! only append.
//!
//! | code | button          | code | axis         |
//! |------|-----------------|------|--------------|
//! | 0    | South (A)       | 0    | LeftStickX   |
//! | 1    | East (B)        | 1    | LeftStickY   |
//! | 2    | North (Y)       | 2    | LeftZ        |
//! | 3    | West (X)        | 3    | RightStickX  |
//! | 4    | C               | 4    | RightStickY  |
//! | 5    | Z               | 5    | RightZ       |
//! | 6    | LeftTrigger (LB/L1)  | 6 | DPadX     |
//! | 7    | LeftTrigger2 (LT/L2, analog 0..1) | 7 | DPadY |
//! | 8    | RightTrigger (RB/R1) |      |              |
//! | 9    | RightTrigger2 (RT/R2, analog 0..1) | |      |
//! | 10   | Select (Back/Share)  |      |              |
//! | 11   | Start (Menu/Options) |      |              |
//! | 12   | Mode (Guide/PS)      |      |              |
//! | 13   | LeftThumb (L3)       |      |              |
//! | 14   | RightThumb (R3)      |      |              |
//! | 15   | DPadUp               |      |              |
//! | 16   | DPadDown             |      |              |
//! | 17   | DPadLeft             |      |              |
//! | 18   | DPadRight            |      |              |
//!
//! Stick axes are -1..1 with +Y = up (gilrs convention). Analog triggers are
//! logged as buttons 7/9 with values 0..1.

pub mod button {
    pub const SOUTH: u32 = 0;
    pub const EAST: u32 = 1;
    pub const NORTH: u32 = 2;
    pub const WEST: u32 = 3;
    pub const C: u32 = 4;
    pub const Z: u32 = 5;
    pub const LEFT_TRIGGER: u32 = 6;
    pub const LEFT_TRIGGER2: u32 = 7;
    pub const RIGHT_TRIGGER: u32 = 8;
    pub const RIGHT_TRIGGER2: u32 = 9;
    pub const SELECT: u32 = 10;
    pub const START: u32 = 11;
    pub const MODE: u32 = 12;
    pub const LEFT_THUMB: u32 = 13;
    pub const RIGHT_THUMB: u32 = 14;
    pub const DPAD_UP: u32 = 15;
    pub const DPAD_DOWN: u32 = 16;
    pub const DPAD_LEFT: u32 = 17;
    pub const DPAD_RIGHT: u32 = 18;
    /// Buttons whose value is analog (0..1) rather than 0/1.
    pub fn is_analog(code: u32) -> bool {
        code == LEFT_TRIGGER2 || code == RIGHT_TRIGGER2
    }
}

pub mod axis {
    pub const LEFT_STICK_X: u32 = 0;
    pub const LEFT_STICK_Y: u32 = 1;
    pub const LEFT_Z: u32 = 2;
    pub const RIGHT_STICK_X: u32 = 3;
    pub const RIGHT_STICK_Y: u32 = 4;
    pub const RIGHT_Z: u32 = 5;
    pub const DPAD_X: u32 = 6;
    pub const DPAD_Y: u32 = 7;
}
