#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

use core::convert::Infallible;
use defmt::info;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    gpio::{self, Input, InputConfig, Level, Output, OutputConfig, Pull},
    main,
    spi::{self, master::Spi},
    time::{Duration, Instant, Rate},
};
use {esp_alloc as _, esp_backtrace as _, esp_println as _};

use embedded_graphics::{
    mono_font::MonoTextStyleBuilder,
    prelude::*,
    primitives::{Circle, Line, PrimitiveStyle, Rectangle, StyledDrawable},
    text::{Baseline, Text, TextStyleBuilder},
};
use embedded_hal_bus::spi::ExclusiveDevice;
use epd_waveshare::{
    color::ColorType,
    epd2in13b_v4::{Display2in13b, Epd2in13b},
};
use epd_waveshare::{
    epd2in13b_v4::{BufferMonoDisplay2in13b, Chunk},
    prelude::*,
};

// This creates a default app-descriptor required by the esp-idf bootloader.
esp_bootloader_esp_idf::esp_app_desc!();

static BLACK: [u8; 4000] = *include_bytes!("../../assets/black.gray");
static RED: [u8; 4000] = *include_bytes!("../../assets/red.gray");

pub fn new_output<'d>(pin: impl gpio::OutputPin + 'd, initial_level: Level) -> Output<'d> {
    Output::new(pin, initial_level, OutputConfig::default())
}

#[main]
fn main() -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let dp = esp_hal::init(config);

    info!("Starting ESP32 E-Paper Display Demo");

    info!("Initializing peripherals...");

    let mut delay = Delay::new();

    let sclk = dp.GPIO18;
    let mosi = dp.GPIO23; // labled "dc"

    let cs = new_output(dp.GPIO5, Level::High);

    // E-paper control pins
    let busy = Input::new(dp.GPIO4, InputConfig::default().with_pull(Pull::Up));
    let rst = new_output(dp.GPIO16, Level::High);
    let dc = new_output(dp.GPIO17, Level::Low);

    info!("Setting up SPI...");

    // Configure SPI
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

    // Initialize the e-paper display
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

    let black_style = PrimitiveStyle::with_stroke(Color::Black, 1);
    let white_style = PrimitiveStyle::with_stroke(Color::White, 1);

    epd.update_achromatic_buffered(&mut spi_device, &mut delay, |buf, i| match i {
        Chunk::Buf1 => {
            buf.clear(Color::White)?;
            draw_text("E-Paper ESP32", 8, 2, Color::Black, buf)?;
            Ok(Some(()))
        }
        Chunk::Buf2 => {
            buf.clear(Color::White)?;
            Rectangle::with_corners(Point::new(2, 2), Point::new(50, 50))
                .draw_styled(&black_style, buf)?;
            Line::new(Point::new(2, 2), Point::new(50, 50)).draw_styled(&black_style, buf)?;
            Line::new(Point::new(2, 50), Point::new(50, 2)).draw_styled(&black_style, buf)?;
            Ok(Some(()))
        }
        Chunk::Buf3 => {
            buf.clear(Color::White)?;
            let rectangle = Rectangle::with_center(Point::new(25, 25), Size::new_equal(20));
            rectangle.draw_styled(&black_style, buf)?;
            Ok(Some(()))
        }
        Chunk::Buf4 => Ok(None),
    })
    .expect("fails to partial-update achromatic buffered");

    epd.update_chromatic_buffered(&mut spi_device, &mut delay, |buf, i| match i {
        Chunk::Buf1 => {
            buf.clear(Color::Black)?;
            draw_text("Hello ESP32!", 8, 20, Color::White, buf)?;
            Ok(Some(()))
        }
        Chunk::Buf2 => {
            buf.clear(Color::Black)?;
            Rectangle::with_corners(Point::new(52, 2), Point::new(100, 50))
                .draw_styled(&PrimitiveStyle::with_fill(Color::White), buf)?;
            Line::new(Point::new(52, 2), Point::new(100, 50)).draw_styled(&white_style, buf)?;
            Line::new(Point::new(100, 2), Point::new(52, 50)).draw_styled(&white_style, buf)?;
            Ok(Some(()))
        }
        Chunk::Buf3 => {
            buf.clear(Color::Black)?;
            Circle::with_center(Point::new(25, 25), 20).draw_styled(&white_style, buf)?;
            Ok(Some(()))
        }
        Chunk::Buf4 => Ok(None),
    })
    .expect("fails to partial-update chromatic buffered");

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
