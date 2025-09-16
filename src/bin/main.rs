#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

extern crate alloc;

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::LazyCell;
use core::convert::Infallible;
use core::mem::MaybeUninit;
use defmt::{dbg, info, println};
use embassy_executor::Spawner;
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::peripherals::Peripherals;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    gpio::{self, Input, InputConfig, Level, Output, OutputConfig, Pull},
    main,
    rng::Rng,
    spi::{self, master::Spi},
    time::{Duration, Instant, Rate},
    timer::timg::TimerGroup,
};
use esp_wifi::wifi::{self, AccessPointInfo};
use picoserve::routing::get;
use {esp_backtrace as _, esp_println as _};

use embedded_graphics::{
    mono_font::MonoTextStyleBuilder,
    prelude::*,
    primitives::{Circle, Line, PrimitiveStyle, Rectangle, StyledDrawable},
    text::{Baseline, Text, TextStyleBuilder},
};
// use embedded_hal_bus::spi::ExclusiveDevice;
use epd_waveshare::prelude::*;
use epd_waveshare::{
    color::ColorType,
    epd2in13b_v4::{Display2in13b, Epd2in13b},
};

// This creates a default app-descriptor required by the esp-idf bootloader.
esp_bootloader_esp_idf::esp_app_desc!();

static BLACK: [u8; 4000] = *include_bytes!("../../assets/black.gray");
static RED: [u8; 4000] = *include_bytes!("../../assets/red.gray");

pub fn new_output<'d>(pin: impl gpio::OutputPin + 'd, initial_level: Level) -> Output<'d> {
    Output::new(pin, initial_level, OutputConfig::default())
}

#[esp_hal_embassy::main]
async fn main(_spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let dp = esp_hal::init(config);

    esp_alloc::heap_allocator!(size: 72 * 1024);

    info!("Starting ESP32 E-Paper Display Demo");

    info!("Setting up wifi...");

    let timg0 = TimerGroup::new(dp.TIMG0);
    let init = esp_wifi::init(timg0.timer0, Rng::new(dp.RNG)).unwrap();

    let (mut wifi, _) = esp_wifi::wifi::new(&init, dp.WIFI).expect("fails to create wifi");

    wifi.set_configuration(&wifi::Configuration::Client(wifi::ClientConfiguration {
        ssid: String::from("詠謙's Galaxy Note10+"),
        password: String::from("mcvc8854"),
        ..Default::default()
    }))
    .expect("fails to set wifi configuration");

    wifi.start().expect("fails to start wifi");

    let res: Result<Vec<AccessPointInfo>, _> = wifi.scan_n(20);
    if let Ok(res) = res {
        for (i, ap) in res.iter().enumerate() {
            println!("{}: {}", i + 1, *ap.ssid);
        }
    }

    wifi.connect()
        .expect("fails to connect to 詠謙's Galaxy Note10+");

    // Wait to get connected
    println!("Waiting to get connected...");
    loop {
        let res = wifi.is_connected();
        match res {
            Ok(connected) => {
                if connected {
                    break;
                }
            }
            Err(err) => {
                println!("Fails to connect: {:?}", err);
                loop {}
            }
        }
    }
    println!("{:?}", wifi.is_connected());

    let app = Rc::new(picoserve::Router::new().route("/", get(|| async { "Hello World" })));

    let config = picoserve::Config::new(picoserve::Timeouts {
        start_read_request: Some(Duration::from_secs(5)),
        persistent_start_read_request: Some(Duration::from_secs(1)),
        read_request: Some(Duration::from_secs(1)),
        write: Some(Duration::from_secs(1)),
    })
    .keep_connection_alive();

    // let socket = TcpLis

    info!("Initializing peripherals for epd...");

    let mut delay = Delay::new();

    let sclk = dp.GPIO18;
    let mosi = dp.GPIO23; // labled "dc"

    let cs = new_output(dp.GPIO5, Level::High);

    let busy = Input::new(dp.GPIO4, InputConfig::default().with_pull(Pull::Up));
    let rst = new_output(dp.GPIO16, Level::High);
    let dc = new_output(dp.GPIO17, Level::Low);

    info!("Setting up SPI...");

    let spi = Spi::new(
        dp.SPI2,
        spi::master::Config::default()
            .with_frequency(Rate::from_mhz(4))
            .with_mode(spi::Mode::_0),
    )
    .expect("can't set up spi port")
    .with_sck(sclk)
    .with_mosi(mosi);

    let mut spi_device = ExclusiveDevice::new(spi, cs, delay).expect("can't setup spi device");

    info!("Creating e-paper display instance...");

    let mut epd = Epd2in13b::new(&mut spi_device, busy, dc, rst, &mut delay, None)
        .expect("can't setup epd2in13b instance");

    epd.set_background_color(TriColor::White);

    info!("Clearing display...");
    epd.clear_frame(&mut spi_device, &mut delay)
        .expect("can't clear frame of the epd");

    info!("Loading image data...");

    // Display the preloaded images
    epd.update_color_frame_with(
        &mut spi_device,
        &mut delay,
        |i| BLACK.get(i).copied().unwrap_or(0xFF),
        |i| RED.get(i).copied().unwrap_or(0x00),
        BLACK.len(),
        RED.len(),
    )
    .expect("can't update colorframe");

    info!("Displaying frame...");
    epd.display_frame(&mut spi_device, &mut delay)
        .expect("can't display colorframe");

    info!("Waiting five seconds so you can see the image...");
    let delay_start = Instant::now();
    while delay_start.elapsed() < Duration::from_millis(5000) {}

    info!("Drawing graphics...");
    epd.clear_frame(&mut spi_device, &mut delay)
        .expect("can't update colorframe");

    let black_style = PrimitiveStyle::with_stroke(TriColor::Black, 1);
    let red_style = PrimitiveStyle::with_stroke(TriColor::Chromatic, 1);

    let mut buffer = Display2in13b::default();

    || -> Result<(), Infallible> {
        buffer.clear(TriColor::White)?;
        let buf = &mut buffer;
        draw_text("E-Paper ESP32", 8, 2, TriColor::Black, buf)?;
        draw_text("Hello ESP32!", 8, 20, TriColor::Chromatic, buf)?;

        Rectangle::with_corners(Point::new(2, 52), Point::new(50, 100))
            .draw_styled(&black_style, buf)?;
        Rectangle::with_corners(Point::new(52, 52), Point::new(100, 100))
            .draw_styled(&PrimitiveStyle::with_fill(TriColor::Chromatic), buf)?;
        Line::new(Point::new(52, 52), Point::new(100, 100)).draw_styled(&red_style, buf)?;
        Line::new(Point::new(100, 52), Point::new(52, 100)).draw_styled(&red_style, buf)?;

        Line::new(Point::new(2, 52), Point::new(50, 100)).draw_styled(&black_style, buf)?;
        Line::new(Point::new(2, 100), Point::new(50, 52)).draw_styled(&black_style, buf)?;
        Circle::with_center(Point::new(25, 125), 20).draw_styled(&red_style, buf)?;

        let rectangle = Rectangle::with_center(Point::new(25, 125), Size::new_equal(20));
        rectangle.draw_styled(&black_style, buf)?;

        Ok(())
    }()
    .expect("drawing fails to initialize");

    epd.update_color_frame(
        &mut spi_device,
        &mut delay,
        buffer.bw_buffer(),
        buffer.chromatic_buffer(),
    )
    .expect("updates color frame data");
    epd.display_frame(&mut spi_device, &mut delay)
        .expect("fails to display frame");

    info!("Putting display to sleep...");
    epd.sleep(&mut spi_device, &mut delay)
        .expect("fails to put epd to sleep");

    // Main loop - could add periodic updates here
    loop {
        let delay_start = Instant::now();
        while delay_start.elapsed() < Duration::from_millis(1000) {}
        info!("E-paper display is sleeping...");
    }
}

fn draw_text<
    const WIDTH: u32,
    const HEIGHT: u32,
    const BWRBIT: bool,
    const BYTECOUNT: usize,
    COLOR: ColorType + PixelColor,
>(
    text: &str,
    x: i32,
    y: i32,
    color: COLOR,
    display: &mut Display<WIDTH, HEIGHT, BWRBIT, BYTECOUNT, COLOR>,
) -> Result<(), Infallible> {
    let style = MonoTextStyleBuilder::new()
        .font(&embedded_graphics::mono_font::ascii::FONT_9X15)
        .text_color(color)
        .build();

    let text_style = TextStyleBuilder::new().baseline(Baseline::Top).build();

    Text::with_text_style(text, Point::new(x, y), style, text_style).draw(display)?;
    Ok(())
}
