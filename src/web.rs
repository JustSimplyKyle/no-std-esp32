use core::str::FromStr;
use edge_nal_embassy::UdpBuffers;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use picoserve::{
    extract::Form,
    response::{File, IntoResponse},
    routing::{self, post},
    AppBuilder,
};

use defmt::{info, warn};
use esp_wifi::wifi::WifiDevice;

use embassy_net::Runner;

use esp_wifi::wifi::AccessPointConfiguration;

use esp_wifi::wifi::Configuration;

use esp_wifi::wifi::WifiEvent;

use esp_wifi::wifi::WifiState;

use esp_wifi::wifi::WifiController;

use embassy_executor::Spawner;

use embassy_time::Duration;

use embassy_time::Timer;

use embassy_net::Stack;
use picoserve::AppRouter;
use serde::Deserialize;

use crate::mk_static;

pub static COMMAND_CHANNEL: Channel<CriticalSectionRawMutex, CommandType, { WEB_POOL_SIZE * 2 }> =
    Channel::new();

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
    ServoDelay(u64),
    Heartbeat,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pressed,
    Released,
    BlinkOnce,
}

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

include!("../include.rs");

#[embassy_executor::task]
pub async fn link_monitor_task(stack: Stack<'static>, gw_ip: &'static str) {
    info!("Waiting for Link...");
    loop {
        if stack.is_link_up() {
            info!("AP Link Up! Web Server at: http://{}/", gw_ip);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }
}

#[embassy_executor::task]
pub async fn run_dhcp(stack: Stack<'static>, gw_ip_addr: &'static str) {
    use core::net::{Ipv4Addr, SocketAddrV4};

    use edge_dhcp::{
        io::{self, DEFAULT_SERVER_PORT},
        server::{Server, ServerOptions},
    };
    use edge_nal::UdpBind;
    use edge_nal_embassy::Udp;

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

pub const WEB_POOL_SIZE: usize = 4;

#[embassy_executor::task(pool_size = WEB_POOL_SIZE)]
pub async fn web_task(
    id: usize,
    stack: Stack<'static>,
    app: &'static AppRouter<Application>,
    config: &'static picoserve::Config<Duration>,
) {
    let mut tcp_rx = [0u8; 1024];
    let mut tcp_tx = [0u8; 1024];
    let mut http_buf = [0u8; 2048];
    let port = 80;

    info!("Web server listening on port {}", port);

    picoserve::Server::new(&app, &config, &mut http_buf)
        .listen_and_serve(id, stack, port, &mut tcp_rx, &mut tcp_tx)
        .await;
}

pub async fn start_web_server(spawner: Spawner, stack: embassy_net::Stack<'static>) {
    info!("Starting web server with {} tasks...", WEB_POOL_SIZE);

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
pub async fn connection(mut controller: WifiController<'static>) {
    info!("start connection task");
    loop {
        if let WifiState::ApStarted = esp_wifi::wifi::wifi_state() {
            // We are up, wait until stopped
            controller.wait_for_event(WifiEvent::ApStop).await;
            warn!("Wi-Fi lost connection, reconnecting...");
            // Timer::after(Duration::from_millis(5000)).await
        }

        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = Configuration::AccessPoint(AccessPointConfiguration {
                ssid: "esp-wifi".into(),
                ..Default::default()
            });
            controller.set_configuration(&client_config).unwrap();
            info!("Starting wifi...");
            controller.start_async().await.unwrap();
            info!("Wifi started!");
        }
    }
}

#[embassy_executor::task]
pub async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}
