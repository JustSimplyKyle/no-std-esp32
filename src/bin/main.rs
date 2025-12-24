#![no_std]
#![feature(impl_trait_in_assoc_type)]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use core::{net::Ipv4Addr, str::FromStr};
use embedded_hal::pwm::SetDutyCycle;
use no_std_esp32::{
    mk_static,
    ps2::*,
    ps2_controller_task,
    servo::{self, servo_task, DELAY_SIGNAL, MAX_DUTY_CYCLE},
    web::{
        self, connection, link_monitor_task, net_task, run_dhcp, CommandType, Status,
        COMMAND_CHANNEL, WEB_POOL_SIZE,
    },
};

use defmt::{error, info};
use embassy_executor::Spawner;
use embassy_futures::select::{self, Either};
use embassy_net::{Ipv4Cidr, StackResources, StaticConfigV4};
use embassy_time::{Duration, Instant, Timer};
use esp_alloc::{self as _};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, OutputPin, Pull},
    ledc::{
        self,
        channel::{self, ChannelIFace},
        timer::{self, LSClockSource, TimerIFace},
        HighSpeed, LSGlobalClkSource, Ledc, LowSpeed,
    },
    rng::Rng,
    time::Rate,
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_wifi::{init, EspWifiController};

esp_bootloader_esp_idf::esp_app_desc!();

// --- Web App Definition ---
pub fn new_controller_output<'d>(
    pin: impl OutputPin + 'd,
    inintal_level: impl Into<Option<Level>>,
) -> Output<'d> {
    Output::new(
        pin,
        inintal_level.into().unwrap_or(Level::High),
        OutputConfig::default(),
    )
}

macro_rules! blink_vec {
    // Main entry point: matches comma-separated list of items
    ( $( $pin:ident $(: $state:expr)? ),* $(,)? ) => {
        Vec::from([
            $(
                (
                    &mut $pin,
                    blink_vec!(@value $($state)?) // Handle optional value
                ),
            )*
        ])
    };

    // Internal helper: if value is present, use it
    (@value $v:expr) => { $v };

    // Internal helper: if value is missing, default to false
    (@value) => { false };
}

struct StepMotor<'a> {
    channel: ledc::channel::Channel<'a, LowSpeed>,
    timer: timer::Timer<'a, LowSpeed>,
}

impl<'a> StepMotor<'a> {
    fn set_frequency(&mut self, frequency: Rate) {
        let duty = if frequency > Rate::from_khz(12) {
            timer::config::Duty::Duty5Bit
        } else {
            timer::config::Duty::Duty8Bit
        };
        self.timer
            .configure(timer::config::Config {
                duty,
                clock_source: LSClockSource::APBClk,
                frequency,
            })
            .unwrap(); // SAFETY: plz find a way to remove this.
        let timer: &'a timer::Timer<'a, LowSpeed> =
            unsafe { core::mem::transmute(&self.timer as &timer::Timer<'a, LowSpeed>) };
        self.channel
            .configure(channel::config::Config {
                timer,
                duty_pct: 100,
                pin_config: channel::config::PinConfig::PushPull,
            })
            .unwrap();
    }
    fn set_duty(&mut self, duty_percentage: u8) -> Result<(), channel::Error> {
        self.channel.set_duty(duty_percentage)
    }
    fn new(
        ledc: &Ledc<'a>,
        timer: timer::Number,
        channel: channel::Number,
        pin: impl OutputPin + 'a,
        initial_rate: Rate,
    ) -> Self {
        let timer = ledc.timer(timer);
        let channel = ledc.channel(channel, pin);
        let mut tmp = Self { timer, channel };
        tmp.set_frequency(initial_rate);
        tmp
    }
}
#[esp_hal_embassy::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);
    esp_alloc::heap_allocator!(#[unsafe(link_section = ".dram2_uninit")] size: 98767);
    esp_alloc::heap_allocator!(size: 48 * 1024);
    // Initialize timers and RNG
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let mut rng = Rng::new(peripherals.RNG);

    // Initialize Wifi Controller
    let esp_wifi_ctrl = &*mk_static!(
        EspWifiController<'static>,
        init(timg0.timer0, rng.clone()).unwrap()
    );

    let (controller, interfaces) = esp_wifi::wifi::new(&esp_wifi_ctrl, peripherals.WIFI).unwrap();

    // let timg0 = TimerGroup::new(peripherals.TIMG1);
    esp_hal_embassy::init(timg0.timer1);

    let device = interfaces.ap;

    let gw_ip_addr_str = "192.168.2.1";
    let gw_ip_addr = Ipv4Addr::from_str(gw_ip_addr_str).expect("failed to parse gateway ip");

    let config = embassy_net::Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(gw_ip_addr, 24),
        gateway: Some(gw_ip_addr),
        dns_servers: Default::default(),
    });

    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    // Init network stack
    // {WEB_POOL_SIZE + 3} sockets: 1 for HTTP, maybe 1 for DHCP (udp), 1 spare, `WEB_POOL_SIZE`
    let (stack, runner) = embassy_net::new(
        device,
        config,
        mk_static!(
            StackResources<{ WEB_POOL_SIZE + 3 }>,
            StackResources::<{ WEB_POOL_SIZE + 3 }>::new()
        ),
        seed,
    );

    println!("Spawning Web Task...");
    spawner
        .spawn(connection(controller))
        .expect("failed to spawn connection");
    spawner
        .spawn(net_task(runner))
        .expect("failed to spawn net_task");

    spawner
        .spawn(run_dhcp(stack, gw_ip_addr_str))
        .expect("failed to start dhcp server");

    spawner
        .spawn(link_monitor_task(stack, gw_ip_addr_str))
        .expect("failed to spawn monitor");

    web::start_web_server(spawner, stack).await;

    let mut rl = new_controller_output(peripherals.GPIO16, None);
    let mut ru = new_controller_output(peripherals.GPIO18, None);
    let mut rr = new_controller_output(peripherals.GPIO5, None);
    let mut rd = new_controller_output(peripherals.GPIO17, None);
    let mut ll = new_controller_output(peripherals.GPIO22, None);
    let mut lu = new_controller_output(peripherals.GPIO23, None);
    let mut lr = new_controller_output(peripherals.GPIO21, None);
    let mut ld = new_controller_output(peripherals.GPIO19, None);
    let mut in1 = new_controller_output(peripherals.GPIO26, Level::Low);
    let mut in2 = new_controller_output(peripherals.GPIO25, Level::Low);
    let servo_pwm = peripherals.GPIO2;

    let ledc = mk_static!(Ledc<'static>, Ledc::new(peripherals.LEDC));
    ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);

    let mut motor1 = StepMotor::new(
        ledc,
        timer::Number::Timer0,
        channel::Number::Channel0,
        peripherals.GPIO27,
        Rate::from_khz(8),
    );

    let mut motor2 = StepMotor::new(
        ledc,
        timer::Number::Timer1,
        channel::Number::Channel1,
        peripherals.GPIO4,
        Rate::from_khz(8),
    );

    // <turning wheels>
    let hstimer2 = mk_static!(
        timer::Timer<'static, HighSpeed>,
        ledc.timer::<HighSpeed>(timer::Number::Timer2)
    );

    hstimer2
        .configure(timer::config::Config {
            duty: timer::config::Duty::Duty12Bit,
            clock_source: timer::HSClockSource::APBClk,
            frequency: Rate::from_hz(50),
        })
        .unwrap();

    let mut channel2 = ledc.channel::<HighSpeed>(channel::Number::Channel2, servo_pwm);
    channel2
        .configure(channel::config::Config {
            timer: hstimer2,
            duty_pct: 10,
            pin_config: channel::config::PinConfig::PushPull,
        })
        .unwrap();

    MAX_DUTY_CYCLE
        .init(channel2.max_duty_cycle() as u32)
        .unwrap();

    spawner
        .spawn(servo_task(channel2))
        .expect("Failed to spawn servo task");

    let mut in3 = new_controller_output(peripherals.GPIO33, Level::Low);
    let mut in4 = new_controller_output(peripherals.GPIO32, Level::Low);
    let mut blink_rate = 75;
    let mut duty_percentage = 100;
    let mut blink_expiry: Option<(Instant, Vec<(&mut Output, bool)>)> = None;

    let delay = Delay::new();

    let clk = Output::new(peripherals.GPIO14, Level::High, OutputConfig::default());

    // CMD (MOSI) -> GPIO 13
    let cmd = Output::new(peripherals.GPIO13, Level::High, OutputConfig::default());

    // CS (Attention) -> GPIO 15
    let att = Output::new(peripherals.GPIO15, Level::High, OutputConfig::default());

    // DAT (MISO) -> GPIO 12
    // IMPORTANT: Enable internal Pull Up, as PS2 Dat line is open-collector
    let dat = Input::new(
        peripherals.GPIO12,
        InputConfig::default().with_pull(Pull::Up),
    );

    let mut ps2 = Ps2Controller::new(clk, cmd, att, dat, delay);

    // Initial Configuration
    info!("Configuring Gamepad...");
    match ps2.config_gamepad() {
        Ok(_) => info!("Success! Gamepad configured."),
        Err(_) => error!("Failed to configure gamepad. Check wiring."),
    }

    spawner
        .spawn(ps2_controller_task::ps2_controller_task(ps2))
        .unwrap();

    loop {
        // We decide how to wait for the command based on whether we are blinking or not
        let command = if let Some((expiry, pins)) = blink_expiry {
            match select::select(COMMAND_CHANNEL.receive(), Timer::at(expiry)).await {
                Either::First(cmd) => {
                    // We got a command! Return it to be processed below.
                    cmd
                }
                Either::Second(_) => {
                    for (ele, activeness) in pins {
                        if activeness {
                            ele.set_low();
                        } else {
                            ele.set_high();
                        }
                    }
                    blink_expiry = None;
                    // Restart the loop to wait for a command normally
                    continue;
                }
            }
        } else {
            COMMAND_CHANNEL.receive().await
        };

        // If we receive any manual movement command, we should probably cancel the blink timer
        // so the timer doesn't accidentally turn off the motors later.
        blink_expiry = None;
        let mut blink =
            |pins| blink_expiry = Some((Instant::now() + Duration::from_millis(blink_rate), pins));

        match command {
            CommandType::Heartbeat => {}
            // front wheel and back wheel go front
            CommandType::GoFront(Status::Pressed) => {
                rd.set_low();
                lr.set_low();
                in1.set_high();
                in2.set_low();
                in3.set_high();
                in4.set_low();
                info!("Message Received: GoFrontPressed");
            }
            CommandType::GoFront(Status::Released) => {
                rd.set_high();
                lr.set_high();
                in1.set_low();
                in2.set_low();
                in3.set_low();
                in4.set_low();
                info!("Message Received: GoFrontReleased");
            }
            CommandType::GoFront(Status::BlinkOnce) => {
                rd.set_low();
                lr.set_low();
                in1.set_high();
                in2.set_low();
                in3.set_high();
                in4.set_low();
                info!("Message Received: GoFrontBlink");
                blink(blink_vec!(rd, lr, in1: true, in3: true));
            }
            CommandType::GoBack(Status::Pressed) => {
                ru.set_low();
                ll.set_low();
                in1.set_low();
                in2.set_high();
                in3.set_low();
                in4.set_high();
                info!("Message Received: GoBackPressed");
            }
            CommandType::GoBack(Status::Released) => {
                ru.set_high();
                ll.set_high();
                in1.set_low();
                in2.set_low();
                in3.set_low();
                in4.set_low();
                info!("Message Received: GoBackReleased");
            }
            CommandType::GoBack(Status::BlinkOnce) => {
                ru.set_low();
                ll.set_low();
                in1.set_low();
                in2.set_high();
                in3.set_low();
                in4.set_high();
                info!("Message Received: GoBackBlinkOnce");
                blink(blink_vec!(ru, ll, in2: true, in4: true));
            }
            CommandType::TurnLeft(Status::Pressed) => {
                rd.set_low();
                servo::SERVO_SIGNAL.signal(85);
                info!("Message Received: TurnLeftPressed");
            }
            CommandType::TurnLeft(Status::Released) => {
                rd.set_high();
                info!("Message Received: TurnLeftReleased");
            }
            CommandType::TurnLeft(Status::BlinkOnce) => {
                rd.set_low();
                blink(blink_vec!(rd));
                info!("Message Received: TurnLeftBlinkOnce");
            }
            CommandType::TurnFront(Status::Pressed) => {
                servo::SERVO_SIGNAL.signal(45);
                info!("Message Received: TurnFrontPressed");
            }
            CommandType::TurnFront(_) => {
                info!("Message Received: TurnFrontNotPressed");
            }
            CommandType::TurnRight(Status::Pressed) => {
                lr.set_low();
                servo::SERVO_SIGNAL.signal(5);
                info!("Message Received: TurnRightPressed");
            }
            CommandType::TurnRight(Status::Released) => {
                lr.set_high();
                info!("Message Received: TurnRightReleased");
            }
            CommandType::TurnRight(Status::BlinkOnce) => {
                lr.set_low();
                info!("Message Received: TurnRightBlinkOnce");
                blink(blink_vec!(lr));
            }
            CommandType::PullUp(Status::Pressed) => {
                ld.set_low();
                info!("Message Received: PullUpPressed");
            }
            CommandType::PullUp(Status::Released) => {
                ld.set_high();
                info!("Message Received: PullUpReleased");
            }
            CommandType::PullUp(Status::BlinkOnce) => {
                ld.set_low();
                info!("Message Received: PullUpBlinkOnce");
                blink(blink_vec!(ld));
            }
            CommandType::PullDown(Status::Pressed) => {
                lu.set_low();
                info!("Message Received: PullDownPressed");
            }
            CommandType::PullDown(Status::Released) => {
                lu.set_high();
                info!("Message Received: PullDownReleased");
            }
            CommandType::PullDown(Status::BlinkOnce) => {
                lu.set_low();
                info!("Message Received: PullDownBlinkOnce");
                blink(blink_vec!(lu));
            }
            CommandType::ArmUp(Status::Pressed) => {
                rr.set_low();
                info!("Message Received: ArmUpPressed");
            }
            CommandType::ArmUp(Status::Released) => {
                rr.set_high();
                info!("Message Received: ArmUpReleased");
            }
            CommandType::ArmUp(Status::BlinkOnce) => {
                rr.set_low();
                info!("Message Received: ArmUpBlinkOnce");
                blink(blink_vec!(rr))
            }
            CommandType::ArmDown(Status::Pressed) => {
                rl.set_low();
                info!("Message Received: ArmDownPressed");
            }
            CommandType::ArmDown(Status::Released) => {
                rl.set_high();
                info!("Message Received: ArmDownReleased");
            }
            CommandType::ArmDown(Status::BlinkOnce) => {
                rl.set_low();
                info!("Message Received: ArmDownBlinkOnce");
                blink(blink_vec!(rl))
            }
            CommandType::BlinkRate(x) => {
                info!(
                    "Message Received: BlinkRate changed from {} to {}",
                    blink_rate, x
                );
                blink_rate = x;
            }
            CommandType::PwmPercentage(percentage) => {
                info!(
                    "Message Received: Pwm Percentage duty cycle changed from {} to {}",
                    duty_percentage, percentage
                );
                motor1
                    .set_duty(percentage)
                    .expect("failed to set motor1 duty cycle");
                motor2
                    .set_duty(percentage)
                    .expect("failed to set motor2 duty cycle");
                duty_percentage = percentage;
            }
            CommandType::FrequencyKilohertz(rate) => {
                motor1.set_frequency(Rate::from_hz(rate));
                motor2.set_frequency(Rate::from_hz(rate));
                motor1.set_duty(duty_percentage).unwrap();
                motor2.set_duty(duty_percentage).unwrap();
                info!("Message Received: Frequeency changed to {}", rate);
            }
            CommandType::ServoDelay(microsecond) => {
                let delay = Duration::from_micros(microsecond);
                DELAY_SIGNAL.signal(delay);
                info!("Message Received: Servo delay changed to {}µs", microsecond);
            }
        }
    }
}
