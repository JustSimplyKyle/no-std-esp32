use defmt::info;
use embassy_sync::blocking_mutex::CriticalSectionMutex;
use embassy_sync::once_lock::OnceLock;
use embassy_time::Duration;
use embedded_hal::pwm::SetDutyCycle;

use embassy_time::Timer;
use esp_hal::ledc::channel::Channel;

use core::sync::atomic::Ordering;

use esp_hal::ledc::{timer, HighSpeed, LowSpeed};

use core::sync::atomic::AtomicU32;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

use embassy_sync::signal::Signal;

pub static SERVO_SIGNAL: Signal<CriticalSectionRawMutex, u32> = Signal::new();

pub static MAX_DUTY_CYCLE: OnceLock<u32> = OnceLock::new();

pub static INITIAL_ANGLE: AtomicU32 = AtomicU32::new(45);

pub static DELAY_SIGNAL: Signal<CriticalSectionRawMutex, Duration> = Signal::new();

#[embassy_executor::task]
pub async fn servo_task(mut channel: Channel<'static, HighSpeed>) {
    // Set initial position
    let _ = channel.set_duty_cycle(duty_from_angle(45).await);

    let mut delay = Duration::from_millis(1);

    loop {
        // Wait here until the Main Task signals a new angle.
        // The executor puts this task to sleep (no CPU usage) until signaled.
        let target_angle = SERVO_SIGNAL.wait().await;
        let current_angle = INITIAL_ANGLE.load(Ordering::Relaxed);

        if DELAY_SIGNAL.signaled() {
            delay = DELAY_SIGNAL.wait().await;
            info!("Delay for servo has changed to {}µs", delay.as_micros());
        }

        if target_angle == current_angle {
            continue;
        }

        info!(
            "Servo Task: Moving from {} to {}",
            current_angle, target_angle
        );

        let mut turn_to = async |i| {
            let duty = duty_from_angle(i).await;
            let _ = channel.set_duty_cycle(duty);
            Timer::after(delay).await;
            INITIAL_ANGLE.store(i, Ordering::Relaxed);
        };

        if current_angle < target_angle {
            for i in (current_angle + 1)..=target_angle {
                // Check if a NEW command came in while we were moving, up
                if SERVO_SIGNAL.signaled() {
                    break;
                }

                turn_to(i).await;
            }
        } else {
            for i in (target_angle..current_angle).rev() {
                if SERVO_SIGNAL.signaled() {
                    break;
                }
                turn_to(i).await;
            }
        }
    }
}

async fn duty_from_angle(deg: u32) -> u16 {
    let max_duty_cycle = *MAX_DUTY_CYCLE.get().await;
    let min_duty = (25 * max_duty_cycle) / 1000;
    // Maximum duty (12.5%)
    // For 12bit -> 125 * 4096 /1000 => 512
    let max_duty = (125 * max_duty_cycle) / 1000;
    // 512 - 102 => 410
    let duty_gap = max_duty - min_duty;
    let duty = min_duty + ((deg * duty_gap) / 180);
    duty as u16
}
