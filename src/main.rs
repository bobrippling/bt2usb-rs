#![no_std]
#![no_main]

use cyw43_pio::PioSpi;
use defmt::{unwrap, info};
use embassy_time::{Timer, Duration};
use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
use static_cell::StaticCell;
//use trouble_example_apps::ble_bas_central;
use trouble_host::{
    prelude::{
        ExternalController,
        Controller,
        Address,
        HostResources,
        DefaultPacketPool,
        Host,
        ConnectConfig,
        ScanConfig,
    },
    gatt::{
        GattClient,
    },
    attribute::{
        Uuid,
        Characteristic,
    },
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
    let p = embassy_rp::init(Default::default());

    #[cfg(feature = "skip-cyw43-firmware")]
    let (fw, clm, btfw) = (&[], &[], &[]);

    #[cfg(not(feature = "skip-cyw43-firmware"))]
    let (fw, clm, btfw) = {
        // IMPORTANT
        //
        // Download and make sure these files from https://github.com/embassy-rs/embassy/tree/main/cyw43-firmware
        // are available in `./examples/rp-pico-w`. (should be automatic)
        //
        // IMPORTANT
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

    run(controller).await
}

#[embassy_executor::task]
async fn cyw43_task(runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>) -> ! {
    runner.run().await
}

async fn run(controller: impl Controller) {
    // Using a fixed "random" address can be useful for testing. In real scenarios, one would
    // use e.g. the MAC 6 byte array as the address (how to get that varies by the platform).
    let address: Address = Address::random([0xff, 0x8f, 0x1b, 0x05, 0xe4, 0xff]);
    info!("Our address = {:?}", address);

    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> = HostResources::new();
    let stack = trouble_host::new(controller, &mut resources).set_random_address(address);
    let Host {
        mut central,
        mut runner,
        ..
    } = stack.build();

    // NOTE: Modify this to match the address of the peripheral you want to connect to.
    // Currently it matches the address used by the peripheral examples
    let target: Address = Address::random([0xff, 0x8f, 0x1a, 0x05, 0xe4, 0xff]);

    let config = ConnectConfig {
        connect_params: Default::default(),
        scan_config: ScanConfig {
            filter_accept_list: &[(target.kind, &target.addr)],
            ..Default::default()
        },
    };

    info!("Scanning for peripheral...");
    let _ = join(runner.run(), async {
        info!("Connecting");

        let conn = central.connect(&config).await.unwrap();
        info!("Connected, creating gatt client");

        let client = GattClient::<_, DefaultPacketPool, 10>
            ::new(&stack, &conn)
            .await
            .unwrap();

        let _ = join(client.task(), async {
            info!("Looking for battery service");
            let services = client.services_by_uuid(&Uuid::new_short(0x180f)).await.unwrap();
            let service = services.first().unwrap().clone();

            info!("Looking for value handle");
            let c: Characteristic<u8> = client
                .characteristic_by_uuid(&service, &Uuid::new_short(0x2a19))
                .await
                .unwrap();

            info!("Subscribing notifications");
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
                        let data = listener.next().await;
                        info!("Got notification: {:?} (val: {})", data.as_ref(), data.as_ref()[0]);
                    }
                },
            )
            .await;
        })
        .await;
    })
    .await;
}
