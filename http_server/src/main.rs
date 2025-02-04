/*  Wireless 2 DOF Camera Monitor
    - Controlling 2 Servo Motor via TCP Server
*/

#![no_std]
#![no_main]
#![allow(async_fn_in_trait)]

mod resources;
mod tasks;

use {
    crate::resources::gpio_list::{
        Irqs, 
        AssignedResources, 
        ServoPioResources, 
        NetworkResources,
        DisplayResources,
    },

    cyw43::JoinOptions,
    cyw43_pio::{PioSpi, DEFAULT_CLOCK_DIVIDER},
    
    embassy_executor::Spawner,
    embassy_time::{Duration, Timer},
    embassy_net::{
        tcp::TcpSocket,
        Config,
        DhcpConfig, 
        StackResources,
    },
    embassy_rp::{
        clocks::RoscRng,
        gpio::{Level, Output},
        peripherals::{DMA_CH0, PIO0, USB},
        pio::Pio,
        usb::Driver as UsbDriver,
    },

    embedded_io_async::Write,
    core::str::{from_utf8, FromStr},
    rand::RngCore,
    static_cell::StaticCell,
    defmt::*,
    {defmt_rtt as _, panic_probe as _},
};

const WIFI_NETWORK: &str = env!("WIFI_NETWORK");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");
const CLIENT_NAME: &str = "Pico-W";
const TCP_PORT: u16 = 80;

const CYW43_JOIN_ERROR: [&str; 16] = [
    "Success", 
    "Operation failed", 
    "Operation timed out",
    "Operation no matching network found",
    "Operation was aborted",
    "[Protocol Failure] Packet not acknowledged",
    "AUTH or ASSOC packet was unsolicited",
    "Attempt to ASSOC to an auto auth configuration",
    "Scan results are incomplete",
    "Scan aborted by another scan",
    "Scan aborted due to assoc in progress",
    "802.11h quiet period started",
    "User disabled scanning (WLC_SET_SCANSUPPRESS)",
    "No allowable channels to scat",
    "Scan aborted due to CCX fast roam",
    "Abort channel select"
];

#[embassy_executor::task]
async fn logger_task(driver: UsbDriver<'static, USB>) {
    embassy_usb_logger::run!(1024, log::LevelFilter::Info, driver);
}

#[embassy_executor::task]
async fn cyw43_task(runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let ph = embassy_rp::init(Default::default());
    let usb_driver = UsbDriver::new(ph.USB, Irqs);
    let r = split_resources!(ph);
    let p = r.network_resources;
    let mut led_toggle = true;
    
    unwrap!(spawner.spawn(logger_task(usb_driver)));

    log::info!("Preparing the Server!");

    let mut rng = RoscRng;
    let fw = include_bytes!("../cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("../cyw43-firmware/43439A0_clm.bin");

    let pwr = Output::new(p.CYW43_PWR_PIN, Level::Low);
    let cs = Output::new(p.CYW43_CS_PIN, Level::High);
    let mut pio = Pio::new(p.CYW43_PIO_CH, Irqs);
    let spi = PioSpi::new(
        &mut pio.common, 
        pio.sm0, 
        DEFAULT_CLOCK_DIVIDER,
        pio.irq0, 
        cs, 
        p.CYW43_SPI_DIO, 
        p.CYW43_SPI_CLK, 
        p.CYW43_DMA_CH
    );

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw).await;
    unwrap!(spawner.spawn(cyw43_task(runner)));

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    log::info!("CYW43 has been set!");    
    control.gpio_set(0, true).await;

    // Using DHCP config for the ipv4 address
    let mut dhcp_config = DhcpConfig::default();
    dhcp_config.hostname = Some(heapless::String::from_str(CLIENT_NAME).unwrap());
    let config = Config::dhcpv4(dhcp_config);

    // Generate random seed
    let seed = rng.next_u64();

    // Init network stack
    static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(net_device, config, RESOURCES.init(StackResources::new()), seed);

    unwrap!(spawner.spawn(net_task(runner)));

    // Connecting to the Network
    loop {
        match control.join(WIFI_NETWORK, JoinOptions::new(WIFI_PASSWORD.as_bytes())).await {
            Ok(_) => {
                Timer::after_millis(100).await;
                break
            },
            Err(err) => {
                if err.status<16 {
                    let error_code = err.status as usize;
                    control.gpio_set(0, led_toggle).await;
                    led_toggle = !led_toggle;
                    log::info!("Join failed with error = {}", CYW43_JOIN_ERROR[error_code]);
                }
            }
        }
    }

    // Wait for DHCP, not necessary when using static IP
    info!("Waiting for DHCP...");
    while !stack.is_config_up() {
        Timer::after_millis(100).await;
    }
    log::info!("DHCP is Now Up!");
    control.gpio_set(0, false).await;

    match stack.config_v4(){
        Some(value) => {
            log::info!("Server Address: {:?}", value.address.address());
            Timer::after_millis(100).await;
        },
        None => log::warn!("Unable to Get the Adrress")
    }

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];
    let mut buf = [0; 4096];

    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(180)));

        if let Err(e) = socket.accept(TCP_PORT).await {
            log::warn!("Accept Error: {:?}", e);
            continue;
        }

        log::info!("Received Connection from {:?}", socket.remote_endpoint());
        
        loop {
            let n = match socket.read(&mut buf).await {
                Ok(0) => {
                    log::info!("Connection closed by client");
                    break;
                }
                Ok(n) => {
                    let request = core::str::from_utf8(&buf[..n]).unwrap();

                    if request.starts_with("GET /led/on") {
                        control.gpio_set(0, true).await; // Turn on the LED
                    } else if request.starts_with("GET /led/off") {
                        control.gpio_set(0, false).await; // Turn off the LED
                    }

                    let response = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 158\r\n\r\n\
                    <html><body>\
                    <h1>LED Control</h1>\
                    <button onclick=\"window.location.href='/led/on'\">Turn On</button>\
                    <button onclick=\"window.location.href='/led/off'\">Turn Off</button>\
                    </body></html>";

                    match socket.write_all(response).await {
                        Ok(()) => {}
                        Err(e) => {
                            log::warn!("Write Error: {:?}", e);
                            break;
                        }
                    };
                }
                Err(e) => {
                    log::warn!("Read Error: {:?}", e);
                    break;
                }
                _ => {}
            };
        }
    }
}