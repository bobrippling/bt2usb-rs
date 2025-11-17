#![no_std]
#![no_main]

use bt_hci::cmd::le::LeExtCreateConn;
use bt_hci::controller::ControllerCmdAsync;
use bt_hci::param::AddrKind;
use cyw43_pio::PioSpi;
use defmt::{debug, error, info, unwrap, Format};
use embassy_futures::{
    join::join,
    //select::{select, Either},
};
use embassy_rp::watchdog::Watchdog;
use embassy_time::{Duration, Ticker, Timer};
use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output, /*Input, Pull*/};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_sync::{channel::Channel, blocking_mutex::raw::CriticalSectionRawMutex};
use static_cell::StaticCell;
use bt_hci::{
    cmd::le::LeSetScanParams, controller::ControllerCmdSync, param::LeAdvReportsIter
};
use trouble_host::gatt::GattClient;
use trouble_host::prelude::{
    AdStructure, Central, Characteristic, ConnectConfig,/* ConnectParams,*/ EventHandler, ScanConfig, Uuid
};
use trouble_host::scan::Scanner;
use trouble_host::Stack;
use trouble_host::{
    prelude::{
        ExternalController,
        Controller,
        Address,
        HostResources,
        DefaultPacketPool,
        Host,
        //ConnectConfig,
        //ScanConfig,
    },
    //gatt::{
        //GattClient,
    //},
    //attribute::{
        //Uuid,
        //Characteristic,
    //},
    //Stack,
};
use defmt_rtt as _;
use panic_probe as _;
//use embassy_time as _;

const CONNECTIONS_MAX: usize = 2; // connection and space for scanning??

const L2CAP_CHANNELS_MAX: usize = 3; // Signal + att + CoC

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let watchdog = embassy_rp::watchdog::Watchdog::new(p.WATCHDOG);
    //unwrap!();
    spawner.spawn(unwrap!(dinner_time(watchdog)));

    #[cfg(feature = "skip-cyw43-firmware")]
    let (fw, clm, btfw) = (&[], &[], &[]);

    #[cfg(not(feature = "skip-cyw43-firmware"))]
    let (fw, clm, btfw) = {
        let fw = include_bytes!("43439A0.bin");
        let clm = include_bytes!("43439A0_clm.bin");
        let btfw = include_bytes!("43439A0_btfw.bin");
        (fw, clm, btfw)
    };

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        cyw43_pio::DEFAULT_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        p.DMA_CH0,
    );

    debug!("starting cyw43...");
    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (_net_device, bt_device, mut control, runner) = cyw43::new_with_bluetooth(
        state,
        pwr,
        spi,
        fw,
        btfw
    ).await;
    spawner.spawn(unwrap!(cyw43_task(runner)));
    control.init(clm).await;
    let controller = ExternalController::<_, 10>::new(bt_device);

    debug!("running...");
    run(controller).await
}

#[embassy_executor::task]
async fn dinner_time(mut watchdog: Watchdog) {
    let mut ticker = Ticker::every(Duration::from_secs(5));
    loop {
        watchdog.feed();
        ticker.next().await;
    }
}

#[embassy_executor::task]
async fn cyw43_task(runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>) -> ! {
    runner.run().await
}

async fn run<C>(
    controller: C,
)
where C: Controller,
      C: ControllerCmdSync<LeSetScanParams>,
      C: ControllerCmdAsync<LeExtCreateConn>,
      <C as embedded_io::ErrorType>::Error: Format,
{
    let address: Address = Address::random([0xff, 0x8f, 0x1b, 0x05, 0xe4, 0xfe]); // TODO
    info!("using address = {:?}", address);

    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> = HostResources::new();
    let stack = trouble_host::new(controller, &mut resources)
        .set_random_address(address)
        .set_random_generator_seed(&mut embassy_rp::clocks::RoscRng);

    let Host {
        mut central,
        runner: mut stack_runner,
        peripheral: _,
        ..
    } = stack.build();

    let handler = BleHandler {
        channel: Channel::new(),
    };

    let (a, ()) = join(
        stack_runner.run_with_handler(&handler),
        async {
            loop {
                info!("scanning for peripheral...");

                let mut config = ScanConfig::default();
                config.active = true;

                // scan every `interval` secs, for a duration of `window` secs
                config.interval = Duration::from_secs(3);
                config.window = Duration::from_secs(2);

                let mut scanner = Scanner::new(central);

                let addr = 'inner: {
                    /*
                    let res = select(
                        scanner.scan(&config),
                        Timer::after(Duration::from_secs(15))
                    ).await;

                    let scan_result = match res {
                        Either::First(sr) => sr,
                        Either::Second(_timeout) => {
                            warn!("timeout scanning");
                            break 'inner;
                        },
                    };
                    */
                    let scan_result = scanner.scan(&config).await;

                    let session = match scan_result {
                        Ok(s) => s,
                        Err(_e) => {
                            error!("couldn't scan"); // TODO: emit `e`
                            break 'inner None;
                        }
                    };

                    info!("waiting for BLE keyboard...");
                    let addr = handler.channel.receive().await;
                    drop(session); // stop scan
                    Some(addr)
                };

                central = scanner.into_inner();

                if let Some(addr) = addr {
                    info!("found BLE keyboard {}", addr);
                    connect(&addr, &mut central, &stack).await;
                }
            }
        }
    ).await;

    let () = a.unwrap();
}

struct BleHandler {
    channel: Channel<CriticalSectionRawMutex, Address, 1>,
}

impl EventHandler for BleHandler {
    fn on_adv_reports(&self, it: LeAdvReportsIter<'_>) {
        debug!("got adv reports");

        for report in it {
            let report = match report {
                Ok(r) => r,
                Err(e) => {
                    error!("[advert] couldn't get report from bytes: {}", e);
                    continue;
                }
            };

            debug!(
                "[advert] from {} ({}), kind: {}",
                report.addr,
                addr_kind_str(report.addr_kind),
                report.event_kind
            );

            debug!("  bytes = {:#x}", report.data);

            let mut is_keyboard = false;

            for ad in AdStructure::decode(report.data) {
                let ad = match ad {
                    Ok(a) => a,
                    Err(e) => {
                        debug!("  error parsing: {}", e);
                        continue;
                    }
                };

                // look for 1812-0-1000-8000-00805f9b34fb (HID)
                // LE onlu, LE limiteid discoverable, br/edr not supported
                // 961 keyboard, hid subtype
                // 16-bit: 1812

                match ad {
                    AdStructure::ServiceUuids16(items) => {
                        for uuid_pair in items {
                            let uuid = Uuid::from(*uuid_pair);
                            debug!("  ServiceUuids16: {}", uuid);

                            if uuid.as_short() == 0x1812 {
                                is_keyboard = true;
                            }
                        }
                    },
                    AdStructure::ServiceData16 { uuid, data } => {
                        let uuid = Uuid::from(uuid);
                        debug!("  ServiceData16: uuid {}, data {:x}", uuid, data);

                        if uuid.as_short() == 0x1812 {
                            is_keyboard = true;
                        }
                    }

                    AdStructure::ServiceUuids128(_items) => {
                        // TODO: check for 0x1812?
                    },

                    AdStructure::CompleteLocalName(bytes) | AdStructure::ShortenedLocalName(bytes) => {
                        debug!("  CompleteLocalName: {}", {
                            let name = match str::from_utf8(bytes) {
                                Ok(n) => n,
                                Err(_e) => "<non-utf8>",
                            };
                            name
                        });
                    }

                    AdStructure::Flags(_flags) => {},
                    AdStructure::ManufacturerSpecificData { company_identifier: _, payload: _ } => {},
                    AdStructure::Unknown { ty: _, data: _ } => {},
                }
            }

            if is_keyboard {
                let addr = Address {
                    kind: report.addr_kind,
                    addr: report.addr,
                };

                info!(
                    "[keyboard] found keyboard: {} (kind {})",
                    addr,
                    addr_kind_str(addr.kind),
                );

                if self.channel.try_send(addr).is_err() {
                    error!("couldn't notify about keyboard: channel full");
                }
            }
        }
    }
}

fn addr_kind_str(k: AddrKind) -> &'static str {
    match k.into_inner() {
        byte if byte == AddrKind::RANDOM.into_inner() => "random",
        byte if byte == AddrKind::PUBLIC.into_inner() => "public",
        byte if byte == AddrKind::RANDOM.into_inner() => "random",
        byte if byte == AddrKind::RESOLVABLE_PRIVATE_OR_PUBLIC.into_inner() => "resolvable-public",
        byte if byte == AddrKind::RESOLVABLE_PRIVATE_OR_RANDOM.into_inner() => "resolvable-random",
        byte if byte == AddrKind::ANONYMOUS_ADV.into_inner() => "anonymous-adv",
        _ => "unknown",
    }
}

async fn connect<'s, C>(
    addr: &Address,
    central: &mut Central<'s, C, DefaultPacketPool>,
    stack: &'s Stack<'s, C, DefaultPacketPool>
)
where
    C: Controller,
    //C: ControllerCmdAsync<LeExtCreateConn>,
    <C as embedded_io::ErrorType>::Error: Format,
{
    #![allow(dead_code)]

    info!("connecting to {}...", addr);

    let config = ConnectConfig {
        scan_config: ScanConfig {
            filter_accept_list: &[(addr.kind, &addr.addr)],
            ..Default::default()
        },
        connect_params: Default::default(), /*ConnectParams {
            max_connection_interval: Duration::from_secs(5),
            ..Default::default()
        },*/
    };

    let conn = match central.connect(&config).await {
        Ok(c) => c,
        Err(e) => {
            // probably a timeout
            error!("couldn't connect to {}: {}", addr, e);
            return;
        }
    };

    info!("connected, securing...");

    // FIXME: needs newer trouble-host
    {
        use trouble_host::prelude::ConnectionEvent;

        unwrap!(conn.request_security());

        info!("requested security, waiting response...");

        loop {
            let ok = match conn.next().await {
                ConnectionEvent::PairingComplete { security_level, ..} => {
                    info!("Pairing complete: {:?}", security_level);
                    true
                },
                ConnectionEvent::PairingFailed(err) => {
                    error!("Pairing failed: {:?}", err);
                    false
                },
                ConnectionEvent::Disconnected { reason } => {
                    error!("Disconnected: {:?}", reason);
                    false
                }
                evt => {
                    info!("got unexpected event {}", evt);
                    continue;
                }
            };

            if ok {
                break;
            }
            return;
        }
    }

    info!("connected (+secured), creating gatt client...");

    let client = unwrap!(GattClient::<_, _, 10>::new(stack, &conn).await);

    info!("created gatt client, looking for services");

    const SERV_HID: u16 = 0x1812;
    const SERV_DEV_INFO: u16 = 0x180a;

    let _ = join(client.task(), async {
        let services = client.services_by_uuid(&Uuid::new_short(SERV_HID)).await.unwrap();
        let service = services.first().unwrap();
        info!("found HID service");

        const CHAR_HID_INFORMATION: u16 = 0x2a4a;
        const CHAR_HID_CONTROL_POINT: u16 = 0x2a4c;
        const CHAR_REPORT_MAP: u16 = 0x2a4b;
        const CHAR_PROTOCOL_MODE: u16 = 0x2a4e;
        const CHAR_REPORT: u16 = 0x2a4d;
        const CHAR_BOOT_KEYBOARD_INPUT_REPORT: u16 = 0x2a22;
        const CHAR_BOOT_KEYBOARD_OUTPUT_REPORT: u16 = 0x2a32;

        let c: Characteristic<u8> = client
            .characteristic_by_uuid(&service, &Uuid::new_short(CHAR_REPORT))
            .await
            .unwrap();

        info!("found HID (report) characteristic");

        let mut listener = client.subscribe(&c, false).await.unwrap();

        let _ = join(
            async {
                loop {
                    let mut data = [0; 1];
                    client.read_characteristic(&c, &mut data[..]).await.unwrap();
                    info!("Read value: {}", data[0]);
                    Timer::after(Duration::from_secs(10)).await;
                }
            },
            async {
                loop {
                    let notif = listener.next().await;

                    info!(
                        "Got notification: {:?}",
                        notif.as_ref(),
                    );
                }
            },
        )
            .await;
        })
    .await;
}
