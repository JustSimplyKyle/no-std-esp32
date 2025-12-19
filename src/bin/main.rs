#![no_std]
#![feature(impl_trait_in_assoc_type)]
#![no_main]

extern crate alloc;

use alloc::{
    boxed::Box,
    vec::{self, Vec},
};
use core::{net::Ipv4Addr, str::FromStr};
use embedded_hal::digital;
use fugit::RateExtU32;
use no_std_esp32::ps2::*;
// use defmt::warn; // defmt not always available on raw esp32 without probe-rs setup, using esp_println

use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_futures::select::{self, Either};
use embassy_net::{Ipv4Cidr, Runner, Stack, StackResources, StaticConfigV4};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    channel::Channel,
    semaphore::{FairSemaphore, Semaphore},
    signal::Signal,
};
use embassy_time::{Duration, Instant, Timer};
use esp_alloc::{self as _, HEAP};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    gpio::{Level, Output, OutputConfig, OutputPin, Pin},
    ledc::{
        channel::{self, ChannelIFace},
        timer::{self, LSClockSource, TimerIFace},
        HighSpeed, LSGlobalClkSource, Ledc, LowSpeed,
    },
    rng::Rng,
    spi::{master::Spi, BitOrder, Mode},
    time::Rate,
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_wifi::{
    init,
    wifi::{
        AccessPointConfiguration, Configuration, WifiController, WifiDevice, WifiEvent, WifiState,
    },
    EspWifiController,
};
use picoserve::{
    extract::Form,
    response::{File, IntoResponse},
    routing::{self, post},
    AppBuilder, AppRouter,
};
use serde::Deserialize;

esp_bootloader_esp_idf::esp_app_desc!();

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "cmd", content = "status")]
enum CommandType {
    GoFront(Status),
    GoBack(Status),
    TurnLeft(Status),
    TurnRight(Status),
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
enum Status {
    Pressed,
    Released,
    BlinkOnce,
}

static COMMAND_CHANNEL: Channel<CriticalSectionRawMutex, CommandType, { WEB_POOL_SIZE * 2 }> =
    Channel::new();

async fn handle_command(Form(form): Form<CommandType>) -> impl IntoResponse {
    match COMMAND_CHANNEL.try_send(form) {
        Ok(_) => {
            // info!("Free heap: {} bytes", HEAP.free());
            "Command Sent"
        }
        Err(_) => {
            warn!("Command Queue Full!");
            "Busy"
        }
    }
}

const GW_IP_ADDR_ENV: Option<&'static str> = option_env!("GATEWAY_IP");

// --- Web App Definition ---
pub struct Application;

macro_rules! static_routes {
    ($base:literal, $(
        $route:literal
    ),* $(,)?) => {{
        picoserve::Router::new()
        $(
            .route(
                {
                    if $route == "index.html" {
                        "/"
                    } else {
                        concat!("/", $route)
                    }
                },
                routing::get_service({
                    let content_type = if $route.ends_with(".js") {
                        "application/javascript"
                    } else if $route.ends_with(".css") {
                        "text/css"
                    } else if $route.ends_with(".wasm") {
                        "application/wasm"
                    } else if $route.ends_with(".html") {
                        "text/html"
                    } else {
                        "application/octet-stream"
                    };
                    let bd = include_bytes!(concat!($base, "/", $route));
                    File::with_content_type(content_type, bd)
                }),
            )
        )*
    }};
}

include!("../../include.rs");

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

pub fn set_frequency<'d>(
    timer: &mut timer::Timer<'d, LowSpeed>,
    mut channel: channel::Channel<'d, LowSpeed>, // Take ownership
    frequency: Rate,
) -> channel::Channel<'d, LowSpeed> {
    let duty = if frequency > Rate::from_khz(12) {
        timer::config::Duty::Duty5Bit
    } else {
        timer::config::Duty::Duty8Bit
    };

    timer
        .configure(timer::config::Config {
            duty,
            clock_source: LSClockSource::APBClk,
            frequency,
        })
        .unwrap();

    // 2. Re-link the channel.
    // We use unsafe to extend the lifetime of the timer reference
    // to match the peripheral lifetime 'd. This is safe because
    // we are returning both objects to the same scope.
    let timer_ref: &'d timer::Timer<'d, LowSpeed> =
        unsafe { core::mem::transmute(timer as &timer::Timer<'d, LowSpeed>) };

    channel
        .configure(channel::config::Config {
            timer: timer_ref,
            duty_pct: 100,
            pin_config: channel::config::PinConfig::PushPull,
        })
        .unwrap();

    channel // Return ownership back
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

    let gw_ip_addr_str = GW_IP_ADDR_ENV.unwrap_or("192.168.2.1");
    let gw_ip_addr = Ipv4Addr::from_str(gw_ip_addr_str).expect("failed to parse gateway ip");

    let config = embassy_net::Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(gw_ip_addr, 24),
        gateway: Some(gw_ip_addr),
        dns_servers: Default::default(),
    });

    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    // Init network stack
    // 3 sockets: 1 for HTTP, maybe 1 for DHCP (udp), 1 spare
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

    start_web_server(spawner, stack).await;

    println!("Waiting for Link...");
    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    println!("AP Link Up! Web Server at: http://{gw_ip_addr_str}/");

    while !stack.is_config_up() {
        Timer::after(Duration::from_millis(100)).await
    }

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
    let motor1_pwm = peripherals.GPIO27;
    let motor2_pwm = peripherals.GPIO4;
    let mut ledc = Ledc::new(peripherals.LEDC);
    ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);

    let mut lstimer0 = ledc.timer::<LowSpeed>(timer::Number::Timer0);
    let mut channel0 = ledc.channel(channel::Number::Channel0, motor1_pwm);
    let mut lstimer1 = ledc.timer::<LowSpeed>(timer::Number::Timer1);
    let mut channel1 = ledc.channel::<LowSpeed>(channel::Number::Channel1, motor2_pwm);

    channel0 = set_frequency(&mut lstimer0, channel0, Rate::from_khz(8));
    channel1 = set_frequency(&mut lstimer1, channel1, Rate::from_khz(8));
    let mut in3 = new_controller_output(peripherals.GPIO33, Level::Low);
    let mut in4 = new_controller_output(peripherals.GPIO32, Level::Low);
    // let mut in3 = new_controller_output(peripherals.GPIO35, None);
    // let mut in4 = new_controller_output(peripherals.GPIO34);
    let mut blink_rate = 75;
    let mut duty_percentage = 75;
    let mut blink_expiry: Option<(Instant, Vec<(&mut Output, bool)>)> = None;
    // --- SPI Configuration ---
    // PS2 requires LSB First.
    // Recommended Speed: ~250kHz - 500kHz.
    // Mode 3 (CPOL=1, CPHA=1) usually works best for PS2.

    // let sclk = peripherals.GPIO14;
    // let miso = peripherals.GPIO12;
    // let mosi = peripherals.GPIO13;
    // let cs_pin = peripherals.GPIO15;

    // let spi_config = esp_hal::spi::master::Config::default()
    //     .with_frequency(Rate::from_khz(250))
    //     .with_mode(Mode::_3) // Idle High, Capture Second Edge
    //     .with_read_bit_order(BitOrder::LsbFirst); // IMPORTANT: PS2 is LSB First

    // // Initialize hardware SPI
    // let spi = Spi::new(peripherals.SPI2, spi_config)
    //     .unwrap()
    //     .into_async()
    //     .with_sck(sclk)
    //     .with_miso(miso)
    //     .with_mosi(mosi);

    // // CS (Attention) is controlled manually because PS2 packets are weird (multi-byte CS low)
    // let cs = Output::new(cs_pin, Level::High, OutputConfig::default());
    // let delay = Delay::new();

    // Create Controller Driver
    // let mut ps2 = PS2Controller::new(spi, cs, delay);

    // println!("Starting PS2 Controller (SPI Mode)...");

    // let mut error = 1;
    // let mut try_num = 1;

    // // Config Loop
    // while error != 0 {
    //     Timer::after(Duration::from_millis(1000)).await;
    //     error = ps2.config_gamepad(false, false);
    //     println!("#try config {}", try_num);
    //     try_num += 1;
    // }

    // let c_type = ps2.identify_type();
    // match c_type {
    //     1 => println!("DualShock Controller found"),
    //     3 => println!("Wireless DualShock Controller found"),
    //     _ => println!("Controller type: {}", c_type),
    // }

    // let mut vibrate = 0;

    // // Main Loop
    // loop {
    //     ps2.read_gamepad(false, vibrate);

    //     if c_type == 1 || c_type == 3 {
    //         // Analog Sticks
    //         if ps2.button(PSB_L1) || ps2.button(PSB_R1) {
    //             println!(
    //                 "LY:{} LX:{} RY:{} RX:{}",
    //                 ps2.analog(PSS_LY),
    //                 ps2.analog(PSS_LX),
    //                 ps2.analog(PSS_RY),
    //                 ps2.analog(PSS_RX)
    //             );
    //         }

    //         // Buttons
    //         if ps2.button_pressed(PSB_CROSS) {
    //             println!("X Pressed");
    //         }
    //         if ps2.button_released(PSB_SQUARE) {
    //             println!("Square Released");
    //         }

    //         // Vibration test (mapped to Cross button pressure if enabled, or simple on/off)
    //         if ps2.button(PSB_CROSS) {
    //             vibrate = 128;
    //         } else {
    //             vibrate = 0;
    //         }
    //     }

    //     Timer::after(Duration::from_millis(50)).await;
    // }

    loop {
        // We decide how to wait for the command based on whether we are blinking or not
        let command = if let Some((expiry, pins)) = blink_expiry {
            // let p = select::select_array([COMMAND_CHANNEL.receive(), Timer::at(expiry)]).await;
            // We are currently blinking. Wait for EITHER a new command OR the timer.
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
            CommandType::TurnRight(Status::Pressed) => {
                lr.set_low();
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
                channel0
                    .set_duty(percentage)
                    .expect("failed to set channel0 duty cycle");
                channel1
                    .set_duty(percentage)
                    .expect("failed to set channel1 duty cycle");
                duty_percentage = percentage;
            }
            CommandType::FrequencyKilohertz(rate) => {
                channel0 = set_frequency(&mut lstimer0, channel0, Rate::from_hz(rate));
                channel1 = set_frequency(&mut lstimer1, channel1, Rate::from_hz(rate));
                channel0.set_duty(duty_percentage).unwrap();
                channel1.set_duty(duty_percentage).unwrap();
                info!("Message Received: Frequeency changed to {}", rate);
            }
        }
    }
}

#[embassy_executor::task]
async fn run_dhcp(stack: Stack<'static>, gw_ip_addr: &'static str) {
    use core::net::{Ipv4Addr, SocketAddrV4};

    use edge_dhcp::{
        io::{self, DEFAULT_SERVER_PORT},
        server::{Server, ServerOptions},
    };
    use edge_nal::UdpBind;
    use edge_nal_embassy::{Udp, UdpBuffers};

    let ip = Ipv4Addr::from_str(gw_ip_addr).expect("dhcp task failed to parse gw ip");

    let mut buf = [0u8; 1500];

    let mut gw_buf = [Ipv4Addr::UNSPECIFIED];

    let buffers = mk_static!(
        UdpBuffers::<3, 1024, 1024, 10>,
        UdpBuffers::<3, 1024, 1024, 10>::new()
    );

    let unbound_socket = Udp::new(stack, buffers);
    let mut bound_socket = unbound_socket
        .bind(core::net::SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::UNSPECIFIED,
            DEFAULT_SERVER_PORT,
        )))
        .await
        .unwrap();

    loop {
        _ = io::server::run(
            &mut Server::<_, 64>::new_with_et(ip),
            &ServerOptions::new(ip, Some(&mut gw_buf)),
            &mut bound_socket,
            &mut buf,
        )
        .await
        .inspect_err(|_e| warn!("DHCP server error")); // fixed unused variable warning
        Timer::after(Duration::from_millis(500)).await;
    }
}

const WEB_POOL_SIZE: usize = 4;

#[embassy_executor::task(pool_size = WEB_POOL_SIZE)]
async fn web_task(
    id: usize,
    stack: Stack<'static>,
    app: &'static AppRouter<Application>,
    config: &'static picoserve::Config<Duration>,
) {
    let mut tcp_rx = [0u8; 1024];
    let mut tcp_tx = [0u8; 1024];
    let mut http_buf = [0u8; 2048];
    let port = 80;

    println!("Web server listening on port {}", port);

    picoserve::Server::new(&app, &config, &mut http_buf)
        .listen_and_serve(id, stack, port, &mut tcp_rx, &mut tcp_tx)
        .await;
}

pub async fn start_web_server(spawner: Spawner, stack: embassy_net::Stack<'static>) {
    println!("Starting web server with {WEB_POOL_SIZE} tasks...");

    let app = mk_static!(AppRouter<Application>, Application.build_app());

    let config = mk_static!(
        picoserve::Config::<Duration>,
        picoserve::Config::new(picoserve::Timeouts {
            start_read_request: Some(Duration::from_secs(5)),
            persistent_start_read_request: Some(Duration::from_secs(1)),
            read_request: Some(Duration::from_secs(1)),
            write: Some(Duration::from_secs(1)),
        })
        .keep_connection_alive()
    );

    for id in 0..WEB_POOL_SIZE {
        spawner.must_spawn(web_task(id, stack, app, config));
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    loop {
        if let WifiState::ApStarted = esp_wifi::wifi::wifi_state() {
            // We are up, wait until stopped
            controller.wait_for_event(WifiEvent::ApStop).await;
            warn!("Wi-Fi lost connection, reconnecting...");
            // Timer::after(Duration::from_millis(5000)).await
        }

        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = Configuration::AccessPoint(AccessPointConfiguration {
                ssid: "esp-wifi".try_into().unwrap(),
                ..Default::default()
            });
            controller.set_configuration(&client_config).unwrap();
            println!("Starting wifi...");
            controller.start_async().await.unwrap();
            println!("Wifi started!");
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}
