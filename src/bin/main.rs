#![no_std]
#![feature(impl_trait_in_assoc_type)]
#![no_main]

extern crate alloc;

use core::{net::Ipv4Addr, str::FromStr};
// use defmt::warn; // defmt not always available on raw esp32 without probe-rs setup, using esp_println

use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_futures::select::{self, Either};
use embassy_net::{Ipv4Cidr, Runner, Stack, StackResources, StaticConfigV4};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use embassy_time::{Duration, Instant, Timer};
use esp_alloc::{self as _};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{Level, Output, OutputConfig, OutputPin},
    rng::Rng,
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
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
enum Status {
    Pressed,
    Released,
    BlinkOnce,
}

static COMMAND_CHANNEL: Channel<CriticalSectionRawMutex, CommandType, 3> = Channel::new();

async fn handle_command(Form(form): Form<CommandType>) -> impl IntoResponse {
    match COMMAND_CHANNEL.try_send(form) {
        Ok(_) => "Command Sent",
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

pub fn new_controller_output<'d>(pin: impl OutputPin + 'd) -> Output<'d> {
    Output::new(pin, Level::High, OutputConfig::default())
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
        mk_static!(StackResources<5>, StackResources::<5>::new()),
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

    let mut rl = new_controller_output(peripherals.GPIO16);
    let mut ru = new_controller_output(peripherals.GPIO18);
    let mut rr = new_controller_output(peripherals.GPIO5);
    let mut rd = new_controller_output(peripherals.GPIO17);
    let mut ll = new_controller_output(peripherals.GPIO22);
    let mut lu = new_controller_output(peripherals.GPIO23);
    let mut lr = new_controller_output(peripherals.GPIO21);
    let mut ld = new_controller_output(peripherals.GPIO19);
    let mut blink_rate = 100;
    let mut blink_expiry: Option<(Instant, (&mut Output, Option<&mut Output>))> = None;

    loop {
        // We decide how to wait for the command based on whether we are blinking or not
        let command = if let Some((expiry, pins)) = blink_expiry {
            // We are currently blinking. Wait for EITHER a new command OR the timer.
            match select::select(COMMAND_CHANNEL.receive(), Timer::at(expiry)).await {
                Either::First(cmd) => {
                    // We got a command! Return it to be processed below.
                    cmd
                }
                Either::Second(_) => {
                    pins.0.set_high();
                    if let Some(pin) = pins.1 {
                        pin.set_high();
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
            CommandType::GoFront(Status::Pressed) => {
                lu.set_low();
                ru.set_low();
                info!("Message Received: GoFrontPressed");
            }
            CommandType::GoFront(Status::Released) => {
                lu.set_high();
                ru.set_high();
                info!("Message Received: GoFrontReleased");
            }
            CommandType::GoFront(Status::BlinkOnce) => {
                lu.set_low();
                ru.set_low();
                info!("Message Received: GoFrontBlink");
                blink((&mut lu, Some(&mut ru)));
            }
            CommandType::GoBack(Status::Pressed) => {
                ld.set_low();
                rd.set_low();
                info!("Message Received: GoBackPressed");
            }
            CommandType::GoBack(Status::Released) => {
                ld.set_high();
                rd.set_high();
                info!("Message Received: GoBackReleased");
            }
            CommandType::GoBack(Status::BlinkOnce) => {
                ld.set_low();
                rd.set_low();
                info!("Message Received: GoBackBlinkOnce");
                blink((&mut ld, Some(&mut rd)));
            }
            CommandType::TurnLeft(Status::Pressed) => {
                lu.set_low();
                info!("Message Received: TurnLeftPressed");
            }
            CommandType::TurnLeft(Status::Released) => {
                lu.set_high();
                info!("Message Received: TurnLeftReleased");
            }
            CommandType::TurnLeft(Status::BlinkOnce) => {
                lu.set_low();
                blink((&mut lu, None));
                info!("Message Received: TurnLeftBlinkOnce");
            }
            CommandType::TurnRight(Status::Pressed) => {
                ru.set_low();
                info!("Message Received: TurnRightPressed");
            }
            CommandType::TurnRight(Status::Released) => {
                ru.set_high();
                info!("Message Received: TurnRightReleased");
            }
            CommandType::TurnRight(Status::BlinkOnce) => {
                ru.set_low();
                info!("Message Received: TurnRightBlinkOnce");
                blink((&mut ru, None));
            }
            CommandType::PullUp(Status::Pressed) => {
                rr.set_low();
                info!("Message Received: PullUpPressed");
            }
            CommandType::PullUp(Status::Released) => {
                rr.set_high();
                info!("Message Received: PullUpReleased");
            }
            CommandType::PullUp(Status::BlinkOnce) => {
                rr.set_low();
                info!("Message Received: PullUpBlinkOnce");
                blink((&mut rr, None));
            }
            CommandType::PullDown(Status::Pressed) => {
                rl.set_low();
                info!("Message Received: PullDownPressed");
            }
            CommandType::PullDown(Status::Released) => {
                rl.set_high();
                info!("Message Received: PullDownReleased");
            }
            CommandType::PullDown(Status::BlinkOnce) => {
                rl.set_low();
                info!("Message Received: PullDownBlinkOnce");
                blink((&mut rl, None));
            }
            CommandType::ArmUp(Status::Pressed) => {
                ll.set_low();
                info!("Message Received: ArmUpPressed");
            }
            CommandType::ArmUp(Status::Released) => {
                ll.set_high();
                info!("Message Received: ArmUpReleased");
            }
            CommandType::ArmUp(Status::BlinkOnce) => {
                ll.set_low();
                info!("Message Received: ArmUpBlinkOnce");
                blink((&mut ll, None));
            }
            CommandType::ArmDown(Status::Pressed) => {
                lr.set_low();
                info!("Message Received: ArmDownPressed");
            }
            CommandType::ArmDown(Status::Released) => {
                lr.set_high();
                info!("Message Received: ArmDownReleased");
            }
            CommandType::ArmDown(Status::BlinkOnce) => {
                lr.set_low();
                info!("Message Received: ArmDownBlinkOnce");
                blink((&mut lr, None));
            }
            CommandType::BlinkRate(x) => {
                blink_rate = x;
                info!(
                    "Message Received: BlinkRate changed from {} to {}",
                    blink_rate, x
                );
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

const WEB_POOL_SIZE: usize = 3;

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
            Timer::after(Duration::from_millis(5000)).await
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
