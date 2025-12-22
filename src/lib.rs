#![no_std]
#![feature(impl_trait_in_assoc_type)]

use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use serde::Deserialize;

pub mod ps2;

pub mod ps2_controller_task;
pub mod servo;
pub mod web_tasks;

#[macro_export]
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

pub static COMMAND_CHANNEL: Channel<
    CriticalSectionRawMutex,
    CommandType,
    { web_tasks::WEB_POOL_SIZE * 2 },
> = Channel::new();

#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "cmd", content = "status")]
pub enum CommandType {
    GoFront(Status),
    GoBack(Status),
    TurnLeft(Status),
    TurnRight(Status),
    TurnFront(Status),
    PullUp(Status),
    PullDown(Status),
    ArmUp(Status),
    ArmDown(Status),
    BlinkRate(u64),
    FrequencyKilohertz(u32),
    PwmPercentage(u8),
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pressed,
    Released,
    BlinkOnce,
}
