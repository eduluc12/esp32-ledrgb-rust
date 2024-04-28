#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

use core::mem::MaybeUninit;
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, Stack, StackResources};
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::gpio::IO;
use esp_hal::ledc::{channel, timer, LSGlobalClkSource, LowSpeed, LEDC};
use core::str::FromStr;
use esp_hal::{
    clock::ClockControl,
    embassy,
    ledc::{self},
    peripherals::Peripherals,
    prelude::*,
    rng::Rng,
    timer::TimerGroup,
};
use esp_println::println;
use esp_wifi::wifi::{ClientConfiguration, Configuration};
use esp_wifi::wifi::{WifiController, WifiDevice, WifiEvent, WifiStaDevice, WifiState};
use esp_wifi::{initialize, EspWifiInitFor};
use rust_mqtt::{
    client::{client::MqttClient, client_config::ClientConfig},
    packet::v5::reason_codes::ReasonCode,
    utils::rng_generator::CountingRng,
};
use serde::{Deserialize, Serialize};
use static_cell::make_static;

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");
const MQTT_SERVER_HOST: &str = env!("MQTT_SERVER_HOST");

static mut MQTT_CLIENT: MaybeUninit<MqttClient<'_, TcpSocket<'_>, 5, CountingRng>> =
    MaybeUninit::uninit();

#[derive(Serialize, Deserialize, Debug, Default)]
struct Color {
    r: u32,
    g: u32,
    b: u32,
}

#[main]
async fn main(spawner: Spawner) {
    let peripherals = Peripherals::take();
    let system = peripherals.SYSTEM.split();
    let clocks = ClockControl::max(system.clock_control).freeze();
    let timer = esp_hal::timer::TimerGroup::new(peripherals.TIMG1, &clocks, None).timer0;

    let mut ledc = LEDC::new(peripherals.LEDC, &clocks);
    ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);

    let mut lstimer0 = ledc.get_timer::<LowSpeed>(timer::Number::Timer0);
    lstimer0
        .configure(timer::config::Config {
            duty: timer::config::Duty::Duty8Bit,
            clock_source: timer::LSClockSource::APBClk,
            frequency: 24.kHz(),
        })
        .unwrap();

    let io = IO::new(peripherals.GPIO, peripherals.IO_MUX);
    let led_r = io.pins.gpio0.into_push_pull_output();
    let led_g = io.pins.gpio2.into_push_pull_output();
    let led_b = io.pins.gpio4.into_push_pull_output();

    let mut channel_r = ledc.get_channel(channel::Number::Channel0, led_r);
    channel_r
        .configure(channel::config::Config {
            timer: &lstimer0,
            duty_pct: 0,
            pin_config: channel::config::PinConfig::PushPull,
        })
        .unwrap();

    let mut channel_g: channel::Channel<'_, LowSpeed, _> =
        ledc.get_channel(channel::Number::Channel1, led_g);
    channel_g
        .configure(channel::config::Config {
            timer: &lstimer0,
            duty_pct: 0,
            pin_config: channel::config::PinConfig::PushPull,
        })
        .unwrap();

    let mut channel_b: channel::Channel<'_, LowSpeed, _> =
        ledc.get_channel(channel::Number::Channel2, led_b);
    channel_b
        .configure(channel::config::Config {
            timer: &lstimer0,
            duty_pct: 0,
            pin_config: channel::config::PinConfig::PushPull,
        })
        .unwrap();

    let init = initialize(
        EspWifiInitFor::Wifi,
        timer,
        Rng::new(peripherals.RNG),
        system.radio_clock_control,
        &clocks,
    )
    .unwrap();

    let wifi = peripherals.WIFI;
    let (wifi_interface, controller) =
        esp_wifi::wifi::new_with_mode(&init, wifi, WifiStaDevice).unwrap();

    let timer_group0 = TimerGroup::new_async(peripherals.TIMG0, &clocks);
    embassy::init(&clocks, timer_group0);

    let config = Config::dhcpv4(Default::default());

    let seed = 1234; // very random, very secure seed

    let stack = &*make_static!(Stack::new(
        wifi_interface,
        config,
        make_static!(StackResources::<3>::new()),
        seed
    ));

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(&stack)).ok();

    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    println!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            println!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    let mut socket = TcpSocket::new(&stack, make_static!([0; 4096]), make_static!([0; 4096]));
    socket.set_timeout(Some(embassy_time::Duration::from_secs(10)));

    println!("Connecting to ...");
    let remote_endpoint = (embassy_net::Ipv4Address::from_str(MQTT_SERVER_HOST).unwrap(), 1883);
    let connection = socket.connect(remote_endpoint).await;
    if let Err(e) = connection {
        println!("Connect error: {:?}", e);
        return;
    }
    println!("Connected to network!");

    let mut config = ClientConfig::new(
        rust_mqtt::client::client_config::MqttVersion::MQTTv5,
        CountingRng(20000),
    );
    config.add_max_subscribe_qos(rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS0);
    config.add_client_id("123456");
    config.keep_alive = 60;
    config.max_packet_size = 100;

    unsafe {
        MQTT_CLIENT.write(MqttClient::<_, 5, _>::new(
            socket,
            make_static!([0; 80]),
            80,
            make_static!([0; 80]),
            80,
            config,
        ));
    }

    println!("Connecting to broker ...");

    unsafe {
        let rt = MQTT_CLIENT.as_mut_ptr();
        match (*rt).connect_to_broker().await {
            Ok(()) => println!("Connected to broker!"),
            Err(mqtt_error) => match mqtt_error {
                ReasonCode::NetworkError => {
                    println!("MQTT Network Error");
                }
                _ => {
                    println!("Other MQTT Error: {:?}", mqtt_error);
                }
            },
        }

        match (*rt).subscribe_to_topic("rgb").await {
            Ok(_) => println!("Subscribed to topic"),
            Err(_) => println!("Error to subscribe to topic"),
        }
    }

    spawner.spawn(broker_ping()).ok();

    loop {
        unsafe {
            let rt = MQTT_CLIENT.as_mut_ptr();
            match (*rt).receive_message().await {
                Ok((_, buf)) => {
                    let (value, _) = serde_json_core::from_slice::<Color>(&buf).unwrap();
                    channel_r.set_duty_hw(value.r);
                    channel_g.set_duty_hw(value.g);
                    channel_b.set_duty_hw(value.b);
                }
                Err(err) => {
                    println!("Error from subscriber {}", err);
                }
            };
            Timer::after(Duration::from_millis(100)).await;
        }
    }
}

#[embassy_executor::task]
async fn broker_ping() {
    loop {
        unsafe {
            Timer::after(Duration::from_millis(5000)).await;
            let rt: *mut MqttClient<'_, TcpSocket<'_>, 5, CountingRng> = MQTT_CLIENT.as_mut_ptr();
            match (*rt)
                .send_message(
                    "null",
                    "".as_bytes(),
                    rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS0,
                    false,
                )
                .await
            {
                Ok(()) => {}
                Err(mqtt_error) => match mqtt_error {
                    ReasonCode::NetworkError => {
                        println!("MQTT Network Error");
                    }
                    _ => {
                        println!("Other MQTT Error: {:?}", mqtt_error);
                    }
                },
            }
        }
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    println!("Device capabilities: {:?}", controller.get_capabilities());
    loop {
        match esp_wifi::wifi::get_wifi_state() {
            WifiState::StaConnected => {
                // wait until we're no longer connected
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
                Timer::after(Duration::from_millis(5000)).await
            }
            _ => {}
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let ssid: heapless::String<32> = SSID.try_into().unwrap();
            let password: heapless::String<64> = PASSWORD.try_into().unwrap();
            let client_config = Configuration::Client(ClientConfiguration {
                ssid,
                password,
                ..Default::default()
            });
            controller.set_configuration(&client_config).unwrap();
            println!("Starting wifi");
            controller.start().await.unwrap();
            println!("Wifi started!");
        }
        println!("About to connect...");

        match controller.connect().await {
            Ok(_) => println!("Wifi connected!"),
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<WifiDevice<'static, WifiStaDevice>>) {
    stack.run().await
}
