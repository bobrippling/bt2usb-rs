#![no_std]
#![no_main]

use cyw43_pio::PioSpi;
use defmt::{unwrap, info, error, warn};
use embassy_futures::{
    join::join,
    select::{select, Either},
};
use embassy_time::{Timer, Duration};
use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
//use embassy_sync::{channel::Channel, blocking_mutex::raw::CriticalSectionRawMutex};
use static_cell::StaticCell;
use bt_hci::{
    cmd::le::LeSetScanParams, controller::ControllerCmdSync, param::LeAdvReportsIter
};
use trouble_host::prelude::{
    AdStructure,
    //Central,
    EventHandler,
    ScanConfig,
};
use trouble_host::scan::Scanner;
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

const CONNECTIONS_MAX: usize = 1;

const L2CAP_CHANNELS_MAX: usize = 3; // Signal + att + CoC

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("starting...");

    let p = embassy_rp::init(Default::default());

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

    info!("starting cyw43...");

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (_net_device, bt_device, mut control, runner) = cyw43::new_with_bluetooth(
        state,
        pwr,
        spi,
        fw,
        btfw
    ).await;

    unwrap!(spawner.spawn(cyw43_task(runner)));

    control.init(clm).await;

    let controller = ExternalController::<_, 10>::new(bt_device);

    info!("running...");

    run(controller, Input::new(p.PIN_15, Pull::Up)).await
}

#[embassy_executor::task]
async fn cyw43_task(runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>) -> ! {
    runner.run().await
}

async fn run<C>(
    controller: C,
    mut button: Input<'static>,
)
where C: Controller,
      C: ControllerCmdSync<LeSetScanParams>,
{
    let address: Address = Address::random([0xff, 0x8f, 0x1b, 0x05, 0xe4, 0xfe]); // TODO
    info!("Our address = {:?}", address);

    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> = HostResources::new();
    let stack = trouble_host::new(controller, &mut resources).set_random_address(address);
    let Host {
        mut central,
        runner: mut stack_runner,
        peripheral: _,
        ..
    } = stack.build();

    let handler = BleHandler;

    info!("Running ble, waiting for button press...");
    let (a, ()) = join(
        stack_runner.run_with_handler(&handler),
        async {
            loop {
                button_press(&mut button).await;

                info!("Got press, scanning for peripheral...");

                let mut config = ScanConfig::default();
                config.active = true;
                config.interval = Duration::from_secs(10);
                config.window = Duration::from_secs(3);

                let mut scanner = Scanner::new(central);

                'inner: {
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
                    let _session = match scan_result {
                        Ok(s) => s,
                        Err(_e) => {
                            error!("couldn't scan"); // TODO: emit `e`
                            break 'inner;
                        }
                    };

                    info!("infinite looping");
                    loop {
                        Timer::after(Duration::from_secs(1)).await;
                    }
                }

                central = scanner.into_inner();
            }
        }
    ).await;

    let () = a.unwrap();
}

async fn button_press(button: &mut Input<'static>) {
    button.wait_for_falling_edge().await;

    Timer::after_millis(50).await; // Debounce

    button.wait_for_rising_edge().await;
}

//async fn connect<'s, C>(
//    central: &mut Central<'s, C, DefaultPacketPool>,
//    stack: &'s Stack<'s, C, DefaultPacketPool>
//)
//where
//    C: Controller,
//{
//    info!("Connecting");
//
//    let config = ConnectConfig {
//        scan_config: Default::default(),
//        connect_params: Default::default(),
//    };
//    let conn = central.connect(&config).await.unwrap();
//
//    info!("Connected, creating gatt client");
//
//    let client = GattClient::<_, DefaultPacketPool, 10>
//        ::new(stack, &conn)
//        .await
//        .unwrap();
//
//    let _ = join(client.task(), async {
//        info!("Looking for battery service");
//        let services = client.services_by_uuid(&Uuid::new_short(0x180f)).await.unwrap();
//        let service = services.first().unwrap().clone();
//
//        info!("Looking for value handle");
//        let c: Characteristic<u8> = client
//            .characteristic_by_uuid(&service, &Uuid::new_short(0x2a19))
//            .await
//            .unwrap();
//
//        info!("Subscribing notifications");
//        let mut listener = client.subscribe(&c, false).await.unwrap();
//
//        let _ = join(
//            async {
//                loop {
//                    let mut data = [0; 1];
//                    client.read_characteristic(&c, &mut data[..]).await.unwrap();
//                    info!("Read value: {}", data[0]);
//                    Timer::after(Duration::from_secs(10)).await;
//                }
//            },
//            async {
//                loop {
//                    let data = listener.next().await;
//                    info!("Got notification: {:?} (val: {})", data.as_ref(), data.as_ref()[0]);
//                }
//            },
//        )
//            .await;
//        })
//    .await;
//}

struct BleHandler;

impl EventHandler for BleHandler {
    fn on_adv_reports(&self, it: LeAdvReportsIter<'_>) {
        info!("got adv reports");
        for report in it {
            let Ok(report) = report else {
                error!("couldn't get report from bytes");
                continue;
            };

            for ad in AdStructure::decode(report.data) {
                let Ok(ad) = ad else {
                    error!("couldn't parse advert");
                    continue;
                };

                // look for 1812-0-1000-8000-00805f9b34fb (HID)
                // LE onlu, LE limiteid discoverable, br/edr not supported
                // 961 keyboard, hid subtype
                //16-bit: 1812

                match ad {
                    AdStructure::ServiceUuids16(items) => {
                        info!("ServiceUuids16: {:?}", items);
                    },
                    AdStructure::ServiceData16 { uuid, data } => {
                        info!("ServiceData16: uuid {:?}, data {:?}", uuid, data);
                    }

                    AdStructure::CompleteLocalName(bytes) => {
                        let name = match str::from_utf8(bytes) {
                            Ok(n) => n,
                            Err(_e) => "<non-utf8>",
                        };

                        info!("found name: {}", name);
                    },

                    _ => {}
                }
            }
        }
    }
}
