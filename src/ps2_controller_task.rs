use defmt::warn;
use embassy_time::Duration;

use crate::ps2::Ps2Controller;
use crate::web::CommandType;
use crate::web::Status;
use crate::web::COMMAND_CHANNEL;

use embassy_time::Timer;

struct ButtonLogic {
    pressed: bool,
}

impl ButtonLogic {
    fn new() -> Self {
        Self { pressed: false }
    }

    /// Checks current input against previous state.
    /// Returns Some(Status) ONLY if the state changed.
    fn update(&mut self, is_pressed: bool) -> Option<Status> {
        if is_pressed != self.pressed {
            self.pressed = is_pressed;
            Some(if is_pressed {
                Status::Pressed
            } else {
                Status::Released
            })
        } else {
            None
        }
    }

    fn force_release(&mut self) -> Option<Status> {
        if self.pressed {
            self.pressed = false;
            Some(Status::Released)
        } else {
            None
        }
    }
}

#[embassy_executor::task]
pub async fn ps2_controller_task(mut ps2: Ps2Controller<'static>) {
    // We replace loose bools with these structured handlers
    let mut go_front = ButtonLogic::new();
    let mut go_back = ButtonLogic::new();
    let mut turn_left = ButtonLogic::new();
    let mut turn_right = ButtonLogic::new();
    let mut turn_front = ButtonLogic::new();

    if ps2.config_gamepad().is_err() {
        warn!("Initial PS2 config failed...");
    }

    loop {
        if ps2.read_gamepad() {
            let ly = ps2.analog_ly();
            let lx = ps2.analog_lx();
            let ry = ps2.analog_ry();
            let rx = ps2.analog_rx();

            // d[x,y] origin point top up left, with 0-255 in each direction
            if let Some(status) = turn_front.update(ly < 50) {
                COMMAND_CHANNEL.send(CommandType::TurnFront(status)).await;
            }

            if let Some(status) = turn_left.update(lx < 50) {
                COMMAND_CHANNEL.send(CommandType::TurnLeft(status)).await;
            }

            if let Some(status) = turn_right.update(lx > 200) {
                COMMAND_CHANNEL.send(CommandType::TurnRight(status)).await;
            }

            if let Some(status) = go_front.update(ry < 50) {
                COMMAND_CHANNEL.send(CommandType::GoFront(status)).await;
            }

            if let Some(status) = go_back.update(ry > 200) {
                COMMAND_CHANNEL.send(CommandType::GoBack(status)).await;
            }
        } else {
            // Force Release
            if let Some(status) = go_front.force_release() {
                COMMAND_CHANNEL.send(CommandType::GoFront(status)).await;
            }
            if let Some(status) = go_back.force_release() {
                COMMAND_CHANNEL.send(CommandType::GoBack(status)).await;
            }
            if let Some(status) = turn_right.force_release() {
                COMMAND_CHANNEL.send(CommandType::TurnLeft(status)).await;
            }
            if let Some(status) = turn_front.force_release() {
                COMMAND_CHANNEL.send(CommandType::TurnRight(status)).await;
            }

            warn!("Controller lost! Reconnecting...");
            let _ = ps2.config_gamepad();
        }

        Timer::after(Duration::from_millis(50)).await;
    }
}
