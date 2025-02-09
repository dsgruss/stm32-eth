// A copy of the `ip.rs` example, but for the STM32F107.

#![no_std]
#![no_main]

extern crate panic_itm;

use cortex_m::asm;
use cortex_m_rt::{entry, exception};
use stm32_eth::{
    hal::flash::FlashExt,
    hal::gpio::GpioExt,
    hal::rcc::RccExt,
    stm32::{interrupt, CorePeripherals, Peripherals, SYST},
};

use core::cell::RefCell;
use cortex_m::interrupt::Mutex;

use core::fmt::Write;
use cortex_m_semihosting::hio;

use fugit::RateExtU32;
use log::{Level, LevelFilter, Metadata, Record};
use smoltcp::iface::{InterfaceBuilder, NeighborCache};
use smoltcp::socket::{TcpSocket, TcpSocketBuffer};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr, Ipv4Address};

use stm32_eth::{EthPins, RingEntry};

static mut LOGGER: HioLogger = HioLogger {};

struct HioLogger {}

impl log::Log for HioLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Trace
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            let mut stdout = hio::hstdout().unwrap();
            writeln!(stdout, "{} - {}", record.level(), record.args()).unwrap();
        }
    }
    fn flush(&self) {}
}

const SRC_MAC: [u8; 6] = [0x00, 0x00, 0xDE, 0xAD, 0xBE, 0xEF];

static TIME: Mutex<RefCell<u64>> = Mutex::new(RefCell::new(0));
static ETH_PENDING: Mutex<RefCell<bool>> = Mutex::new(RefCell::new(false));

#[entry]
fn main() -> ! {
    unsafe {
        log::set_logger(&LOGGER).unwrap();
    }
    log::set_max_level(LevelFilter::Info);

    let mut stdout = hio::hstdout().unwrap();

    let p = Peripherals::take().unwrap();
    let mut cp = CorePeripherals::take().unwrap();

    let mut flash = p.FLASH.constrain();

    let rcc = p.RCC.constrain();
    // HCLK must be at least 25MHz to use the ethernet peripheral
    let clocks = rcc
        .cfgr
        .sysclk(32.MHz())
        .hclk(32.MHz())
        .freeze(&mut flash.acr);

    setup_systick(&mut cp.SYST);

    writeln!(stdout, "Enabling ethernet...").unwrap();

    let mut gpioa = p.GPIOA.split();
    let mut gpiob = p.GPIOB.split();
    let mut gpioc = p.GPIOC.split();

    let ref_clk = gpioa.pa1.into_floating_input(&mut gpioa.crl);
    let crs = gpioa.pa7.into_floating_input(&mut gpioa.crl);
    let tx_en = gpiob.pb11.into_alternate_push_pull(&mut gpiob.crh);
    let tx_d0 = gpiob.pb12.into_alternate_push_pull(&mut gpiob.crh);
    let tx_d1 = gpiob.pb13.into_alternate_push_pull(&mut gpiob.crh);
    let rx_d0 = gpioc.pc4.into_floating_input(&mut gpioc.crl);
    let rx_d1 = gpioc.pc5.into_floating_input(&mut gpioc.crl);

    let eth_pins = EthPins {
        ref_clk,
        crs,
        tx_en,
        tx_d0,
        tx_d1,
        rx_d0,
        rx_d1,
    };

    let mut rx_ring: [RingEntry<_>; 8] = Default::default();
    let mut tx_ring: [RingEntry<_>; 2] = Default::default();
    let (mut eth_dma, _eth_mac) = stm32_eth::new(
        p.ETHERNET_MAC,
        p.ETHERNET_MMC,
        p.ETHERNET_DMA,
        &mut rx_ring[..],
        &mut tx_ring[..],
        clocks,
        eth_pins,
    )
    .unwrap();
    eth_dma.enable_interrupt();

    let local_addr = Ipv4Address::new(10, 0, 0, 1);
    let ip_addr = IpCidr::new(IpAddress::from(local_addr), 24);
    let mut ip_addrs = [ip_addr];
    let mut neighbor_storage = [None; 16];
    let neighbor_cache = NeighborCache::new(&mut neighbor_storage[..]);
    let ethernet_addr = EthernetAddress(SRC_MAC);

    let mut sockets: [_; 1] = Default::default();
    let mut iface = InterfaceBuilder::new(&mut eth_dma, &mut sockets[..])
        .hardware_addr(ethernet_addr.into())
        .ip_addrs(&mut ip_addrs[..])
        .neighbor_cache(neighbor_cache)
        .finalize();

    let mut server_rx_buffer = [0; 2048];
    let mut server_tx_buffer = [0; 2048];
    let server_socket = TcpSocket::new(
        TcpSocketBuffer::new(&mut server_rx_buffer[..]),
        TcpSocketBuffer::new(&mut server_tx_buffer[..]),
    );
    let server_handle = iface.add_socket(server_socket);

    writeln!(stdout, "Ready, listening at {}", ip_addr).unwrap();
    loop {
        let time: u64 = cortex_m::interrupt::free(|cs| *TIME.borrow(cs).borrow());
        cortex_m::interrupt::free(|cs| {
            let mut eth_pending = ETH_PENDING.borrow(cs).borrow_mut();
            *eth_pending = false;
        });
        match iface.poll(Instant::from_millis(time as i64)) {
            Ok(true) => {
                let socket = iface.get_socket::<TcpSocket>(server_handle);
                if !socket.is_open() {
                    socket
                        .listen(80)
                        .or_else(|e| writeln!(stdout, "TCP listen error: {:?}", e))
                        .unwrap();
                }

                if socket.can_send() {
                    write!(socket, "hello\n")
                        .map(|_| {
                            socket.close();
                        })
                        .or_else(|e| writeln!(stdout, "TCP send error: {:?}", e))
                        .unwrap();
                }
            }
            Ok(false) => {
                // Sleep if no ethernet work is pending
                cortex_m::interrupt::free(|cs| {
                    let eth_pending = ETH_PENDING.borrow(cs).borrow_mut();
                    if !*eth_pending {
                        asm::wfi();
                        // Awaken by interrupt
                    }
                });
            }
            Err(e) =>
            // Ignore malformed packets
            {
                writeln!(stdout, "Error: {:?}", e).unwrap()
            }
        }
    }
}

fn setup_systick(syst: &mut SYST) {
    syst.set_reload(SYST::get_ticks_per_10ms() / 10);
    syst.enable_counter();
    syst.enable_interrupt();
}

#[exception]
fn SysTick() {
    cortex_m::interrupt::free(|cs| {
        let mut time = TIME.borrow(cs).borrow_mut();
        *time += 1;
    })
}

#[interrupt]
fn ETH() {
    cortex_m::interrupt::free(|cs| {
        let mut eth_pending = ETH_PENDING.borrow(cs).borrow_mut();
        *eth_pending = true;
    });

    // Clear interrupt flags
    let p = unsafe { Peripherals::steal() };
    stm32_eth::eth_interrupt_handler(&p.ETHERNET_DMA);
}
