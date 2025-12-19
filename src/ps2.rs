use defmt::{error, info, println};
use embassy_executor::Spawner;
use embassy_time::{Duration, Instant, Timer};
use embedded_hal::spi::SpiBus;
use esp_backtrace as _;
use esp_hal::{
    delay::Delay,
    gpio::{Level, Output, Pin},
    spi::{
        master::{Config, Spi},
        Mode,
    },
    timer::timg::TimerGroup,
    Async,
};

// ==============================================================================
// Constants & Commands
// ==============================================================================
pub const CMD_ENTER_CONFIG: [u8; 5] = [0x01, 0x43, 0x00, 0x01, 0x00];
pub const CMD_SET_MODE: [u8; 9] = [0x01, 0x44, 0x00, 0x01, 0x03, 0x00, 0x00, 0x00, 0x00];
pub const CMD_SET_BYTES_LARGE: [u8; 9] = [0x01, 0x4F, 0x00, 0xFF, 0xFF, 0x03, 0x00, 0x00, 0x00];
pub const CMD_EXIT_CONFIG: [u8; 9] = [0x01, 0x43, 0x00, 0x00, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A];
pub const CMD_ENABLE_RUMBLE: [u8; 5] = [0x01, 0x4D, 0x00, 0x00, 0x01];
pub const CMD_TYPE_READ: [u8; 9] = [0x01, 0x45, 0x00, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A, 0x5A];
pub const CMD_READ_DATA: [u8; 9] = [0x01, 0x42, 0, 0, 0, 0, 0, 0, 0];
pub const PSB_SELECT: u16 = 0x0001;
pub const PSB_L3: u16 = 0x0002;
pub const PSB_R3: u16 = 0x0004;
pub const PSB_START: u16 = 0x0008;
pub const PSB_PAD_UP: u16 = 0x0010;
pub const PSB_PAD_RIGHT: u16 = 0x0020;
pub const PSB_PAD_DOWN: u16 = 0x0040;
pub const PSB_PAD_LEFT: u16 = 0x0080;
pub const PSB_L2: u16 = 0x0100;
pub const PSB_R2: u16 = 0x0200;
pub const PSB_L1: u16 = 0x0400;
pub const PSB_R1: u16 = 0x0800;
pub const PSB_TRIANGLE: u16 = 0x1000;
pub const PSB_CIRCLE: u16 = 0x2000;
pub const PSB_CROSS: u16 = 0x4000;
pub const PSB_SQUARE: u16 = 0x8000;
pub const PSS_RX: usize = 5;
pub const PSS_RY: usize = 6;
pub const PSS_LX: usize = 7;
pub const PSS_LY: usize = 8;
pub const PSAB_PAD_RIGHT: usize = 9;
pub const PSAB_PAD_UP: usize = 11;
pub const PSAB_PAD_DOWN: usize = 12;
pub const PSAB_PAD_LEFT: usize = 10;
pub const PSAB_CROSS: usize = 18;
pub const CTRL_BYTE_DELAY: u32 = 10; // Microseconds between bytes

// ==============================================================================
// PS2 Controller Driver (SPI Version)
// ==============================================================================
pub struct PS2Controller<'d> {
    spi: Spi<'d, Async>,
    cs: Output<'d>,
    delay: Delay,

    // State
    pub data_buffer: [u8; 21],
    pub buttons: u16,
    pub last_buttons: u16,
    pub controller_type: u8,

    last_read_time: Instant,
    read_delay_ms: u64,
    en_rumble: bool,
    en_pressures: bool,
}

impl<'d> PS2Controller<'d> {
    pub fn new(spi: Spi<'d, Async>, cs: Output<'d>, delay: Delay) -> Self {
        Self {
            spi,
            cs,
            delay,
            data_buffer: [0u8; 21],
            buttons: 0xFFFF,
            last_buttons: 0xFFFF,
            controller_type: 0,
            last_read_time: Instant::now(),
            read_delay_ms: 1,
            en_rumble: false,
            en_pressures: false,
        }
    }

    /// Sends a full command string, byte-by-byte
    /// We do manual byte-by-byte transfer to insert delays required by PS2 hardware.
    fn send_command_string(&mut self, cmd: &[u8], store_result: bool) {
        self.cs.set_low();
        self.delay.delay_micros(CTRL_BYTE_DELAY);

        for (i, &byte) in cmd.iter().enumerate() {
            let mut buf = [byte];
            // SPI Transfer: Sends `byte`, receives into `buf[0]`
            match self.spi.transfer_in_place(&mut buf) {
                Ok(_) => {
                    if store_result && i < self.data_buffer.len() {
                        self.data_buffer[i] = buf[0];
                    }
                }
                Err(_) => {
                    // Handle SPI error if necessary
                }
            }
            // Essential delay between bytes for PS2
            self.delay.delay_micros(CTRL_BYTE_DELAY);
        }

        self.cs.set_high();
        self.delay.delay_millis(self.read_delay_ms as u32);
    }

    pub fn config_gamepad(&mut self, pressures: bool, rumble: bool) -> u8 {
        // Read a few times to stabilize
        self.read_gamepad(false, 0);
        self.read_gamepad(false, 0);

        // Check for basic connectivity
        if self.data_buffer[1] != 0x41
            && self.data_buffer[1] != 0x42
            && self.data_buffer[1] != 0x73
            && self.data_buffer[1] != 0x79
        {
            return 1; // No controller
        }

        self.read_delay_ms = 1;

        for y in 0..=10 {
            self.send_command_string(&CMD_ENTER_CONFIG, false);

            // Read Type special logic
            self.cs.set_low();
            self.delay.delay_micros(CTRL_BYTE_DELAY);

            let mut temp = [0u8; 9];
            // Manually transfer type read command
            for i in 0..9 {
                let mut buf = [CMD_TYPE_READ[i]];
                let _ = self.spi.transfer_in_place(&mut buf);
                temp[i] = buf[0];
                self.delay.delay_micros(CTRL_BYTE_DELAY);
            }
            self.cs.set_high();

            self.controller_type = temp[3];

            // Set Mode
            self.send_command_string(&CMD_SET_MODE, false);

            if rumble {
                self.send_command_string(&CMD_ENABLE_RUMBLE, false);
                self.en_rumble = true;
            }
            if pressures {
                self.send_command_string(&CMD_SET_BYTES_LARGE, false);
                self.en_pressures = true;
            }

            self.send_command_string(&CMD_EXIT_CONFIG, false);
            self.read_gamepad(false, 0);

            if pressures {
                if self.data_buffer[1] == 0x79 {
                    break;
                }
                if self.data_buffer[1] == 0x73 {
                    return 3;
                }
            }
            if self.data_buffer[1] == 0x73 {
                break;
            }

            if y == 10 {
                return 2;
            }
            self.read_delay_ms += 1;
        }
        0
    }

    fn reconfig_gamepad(&mut self) {
        self.send_command_string(&CMD_ENTER_CONFIG, false);
        self.send_command_string(&CMD_SET_MODE, false);
        if self.en_rumble {
            self.send_command_string(&CMD_ENABLE_RUMBLE, false);
        }
        if self.en_pressures {
            self.send_command_string(&CMD_SET_BYTES_LARGE, false);
        }
        self.send_command_string(&CMD_EXIT_CONFIG, false);
    }

    pub fn read_gamepad(&mut self, motor_small: bool, motor_large: u8) -> bool {
        let now = Instant::now();
        let elapsed_ms = now.duration_since(self.last_read_time).as_millis();

        if elapsed_ms > 1500 {
            self.reconfig_gamepad();
        }
        if elapsed_ms < self.read_delay_ms {
            self.delay
                .delay_millis((self.read_delay_ms - elapsed_ms) as u32);
        }

        // Prepare Motor command bytes
        let mut cmd = CMD_READ_DATA;
        cmd[3] = if motor_small { 0xFF } else { 0x00 };
        cmd[4] = if motor_large > 0 {
            0x40 + ((motor_large as u16 * (0xFF - 0x40)) / 255) as u8
        } else {
            0
        };

        let mut success = false;

        for _ in 0..5 {
            self.cs.set_low();
            self.delay.delay_micros(CTRL_BYTE_DELAY);

            // Read standard 9 bytes
            for i in 0..9 {
                let mut buf = [cmd[i]];
                let _ = self.spi.transfer_in_place(&mut buf);
                self.data_buffer[i] = buf[0];
                self.delay.delay_micros(CTRL_BYTE_DELAY);
            }

            // If extended mode (0x79), read extra 12 bytes
            if self.data_buffer[1] == 0x79 {
                for i in 0..12 {
                    let mut buf = [0u8];
                    let _ = self.spi.transfer_in_place(&mut buf);
                    self.data_buffer[9 + i] = buf[0];
                    self.delay.delay_micros(CTRL_BYTE_DELAY);
                }
            }

            self.cs.set_high();

            // Check header (0x7_ indicates analog mode)
            if (self.data_buffer[1] & 0xF0) == 0x70 {
                success = true;
                break;
            }

            self.reconfig_gamepad();
            self.delay.delay_millis(self.read_delay_ms as u32);
        }

        if !success && self.read_delay_ms < 10 {
            self.read_delay_ms += 1;
        }

        self.last_buttons = self.buttons;
        let b3 = self.data_buffer[3] as u16;
        let b4 = self.data_buffer[4] as u16;
        self.buttons = (b4 << 8) | b3;
        self.last_read_time = Instant::now();

        success
    }

    // --- State Accessors ---

    pub fn button(&self, mask: u16) -> bool {
        (!self.buttons & mask) != 0
    }
    pub fn analog(&self, id: usize) -> u8 {
        if id < 21 {
            self.data_buffer[id]
        } else {
            0
        }
    }
    pub fn new_button_state(&self) -> bool {
        (self.buttons ^ self.last_buttons) > 0
    }
    pub fn new_button_state_specific(&self, mask: u16) -> bool {
        ((self.buttons ^ self.last_buttons) & mask) > 0
    }
    pub fn button_pressed(&self, mask: u16) -> bool {
        self.new_button_state_specific(mask) && self.button(mask)
    }
    pub fn button_released(&self, mask: u16) -> bool {
        self.new_button_state_specific(mask) && !self.button(mask)
    }

    pub fn identify_type(&self) -> u8 {
        if self.controller_type == 0x03 {
            return 1;
        }
        // DualShock
        else if self.controller_type == 0x01 && self.data_buffer[1] == 0x42 {
            return 4;
        } else if self.controller_type == 0x01 && self.data_buffer[1] != 0x42 {
            return 2;
        } else if self.controller_type == 0x0C {
            return 3;
        }
        0
    }
}

// ==============================================================================
// Main
// ==============================================================================

// #[esp_hal_embassy::main]
// async fn main(_spawner: Spawner) {
//     let peripherals = esp_hal::init(esp_hal::Config::default());

//     let timg0 = TimerGroup::new(peripherals.TIMG0);
//     esp_hal_embassy::init(timg0.timer0);

//     let io = peripherals.GPIO;

//     // --- SPI Configuration ---
//     // PS2 requires LSB First.
//     // Recommended Speed: ~250kHz - 500kHz.
//     // Mode 3 (CPOL=1, CPHA=1) usually works best for PS2.

//     let sclk = io.gpio18;
//     let miso = io.gpio19;
//     let mosi = io.gpio23;
//     let cs_pin = io.gpio5;

//     let spi_config = Config::default()
//         .with_frequency(250.kHz())
//         .with_mode(Mode::_3) // Idle High, Capture Second Edge
//         .with_bit_order(SpiBitOrder::LsbFirst); // IMPORTANT: PS2 is LSB First

//     // Initialize hardware SPI
//     let spi = Spi::new(peripherals.SPI2, spi_config).unwrap().with_pins(
//         sclk,
//         mosi,
//         miso,
//         esp_hal::gpio::NoPin,
//     );

//     // CS (Attention) is controlled manually because PS2 packets are weird (multi-byte CS low)
//     let cs = Output::new(cs_pin, Level::High);
//     let delay = Delay::new();

//     // Create Controller Driver
//     let mut ps2 = PS2Controller::new(spi, cs, delay);

//     println!("Starting PS2 Controller (SPI Mode)...");

//     let mut error = 1;
//     let mut try_num = 1;

//     // Config Loop
//     while error != 0 {
//         Timer::after(Duration::from_millis(1000)).await;
//         error = ps2.config_gamepad(false, false);
//         println!("#try config {}", try_num);
//         try_num += 1;
//     }

//     let c_type = ps2.identify_type();
//     match c_type {
//         1 => println!("DualShock Controller found"),
//         3 => println!("Wireless DualShock Controller found"),
//         _ => println!("Controller type: {}", c_type),
//     }

//     let mut vibrate = 0;

//     // Main Loop
//     loop {
//         ps2.read_gamepad(false, vibrate);

//         if c_type == 1 || c_type == 3 {
//             // Analog Sticks
//             if ps2.button(PSB_L1) || ps2.button(PSB_R1) {
//                 println!(
//                     "LY:{} LX:{} RY:{} RX:{}",
//                     ps2.analog(PSS_LY),
//                     ps2.analog(PSS_LX),
//                     ps2.analog(PSS_RY),
//                     ps2.analog(PSS_RX)
//                 );
//             }

//             // Buttons
//             if ps2.button_pressed(PSB_CROSS) {
//                 println!("X Pressed");
//             }
//             if ps2.button_released(PSB_SQUARE) {
//                 println!("Square Released");
//             }

//             // Vibration test (mapped to Cross button pressure if enabled, or simple on/off)
//             if ps2.button(PSB_CROSS) {
//                 vibrate = 128;
//             } else {
//                 vibrate = 0;
//             }
//         }

//         Timer::after(Duration::from_millis(50)).await;
//     }
// }
