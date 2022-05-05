use anyhow::{anyhow, Context, Result};
use bytes::BytesMut;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc};

mod pcapng;

mod position;
use position::Position;

mod uci_packets;
use uci_packets::*;

mod device;
use device::{Device, MAX_DEVICE};

mod session;
use session::MAX_SESSION;

pub mod web;

const MAX_PAYLOAD_SIZE: usize = 4096;

pub type MacAddress = u64;

struct Connection {
    socket: TcpStream,
    buffer: BytesMut,
    pcapng_file: Option<pcapng::File>,
}

impl Connection {
    fn new(socket: TcpStream, pcapng_file: Option<pcapng::File>) -> Self {
        Connection {
            socket,
            buffer: BytesMut::with_capacity(MAX_PAYLOAD_SIZE),
            pcapng_file,
        }
    }

    async fn read(&mut self) -> Result<Option<UciCommandPacket>> {
        let len = self.socket.read_buf(&mut self.buffer).await?;
        if len == 0 {
            return Ok(None);
        }

        if let Some(ref mut pcapng_file) = self.pcapng_file {
            pcapng_file.write(&self.buffer, pcapng::Direction::Tx)?
        }

        let packet = UciPacketPacket::parse(&self.buffer)?;
        self.buffer.clear();
        Ok(Some(packet.try_into()?))
    }

    async fn write(&mut self, packet: UciPacketPacket) -> Result<()> {
        let buffer = packet.to_bytes();

        if let Some(ref mut pcapng_file) = self.pcapng_file {
            pcapng_file.write(&buffer, pcapng::Direction::Rx)?
        }

        let _ = self.socket.try_write(&buffer)?;
        Ok(())
    }
}

#[derive(Debug)]
pub enum PicaCommand {
    // Connect a new device.
    Connect(TcpStream),
    // Disconnect the selected device.
    Disconnect(usize),
    // Execute ranging command for selected device and session.
    Ranging(usize, u32),
    // Execute UCI command received for selected device.
    Command(usize, UciCommandPacket),
    // Set Position
    SetPosition(MacAddress, Position),
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum PicaEvent {
    // A Device was added
    AddDevice {
        mac_address: MacAddress,
        #[serde(flatten)]
        position: Position,
    },
    // A Device was removed
    RemoveDevice {
        mac_address: MacAddress,
    },
    // A Device position has changed
    UpdatePosition {
        mac_address: MacAddress,
        #[serde(flatten)]
        position: Position,
    },
    UpdateNeighbor {
        mac_address: MacAddress,
        neighbor: MacAddress,
        distance: u16,
        azimuth: i16,
        elevation: i8,
    },
}

#[derive(Debug)]
struct Beacon {
    position: Position,
    mac_address: MacAddress,
}

pub struct Pica {
    devices: HashMap<usize, Device>,
    beacons: HashMap<MacAddress, Beacon>,
    counter: usize,
    rx: mpsc::Receiver<PicaCommand>,
    tx: mpsc::Sender<PicaCommand>,
    event_tx: broadcast::Sender<PicaEvent>,
    pcapng_dir: Option<PathBuf>,
}

impl Pica {
    pub fn new(event_tx: broadcast::Sender<PicaEvent>, pcapng_dir: Option<PathBuf>) -> Self {
        let (tx, rx) = mpsc::channel(MAX_SESSION * MAX_DEVICE);
        Pica {
            devices: HashMap::new(),
            beacons: HashMap::new(),
            counter: 0,
            rx,
            tx,
            event_tx,
            pcapng_dir,
        }
    }

    pub fn tx(&self) -> mpsc::Sender<PicaCommand> {
        self.tx.clone()
    }

    fn get_device_mut(&mut self, device_handle: usize) -> &mut Device {
        self.devices.get_mut(&device_handle).unwrap()
    }

    fn get_device(&self, device_handle: usize) -> &Device {
        self.devices.get(&device_handle).unwrap()
    }

    fn get_device_mut_by_mac(&mut self, mac_address: MacAddress) -> Option<&mut Device> {
        self.devices
            .values_mut()
            .find(|d| d.mac_address as u64 == mac_address)
    }

    fn send_event(&self, event: PicaEvent) {
        // An error here means that we have
        // no receivers, so ignore it
        let _ = self.event_tx.send(event);
    }

    async fn connect(&mut self, stream: TcpStream) {
        let (packet_tx, mut packet_rx) = mpsc::channel(MAX_SESSION);
        let device_handle = self.counter;
        let pica_tx = self.tx.clone();
        let pcapng_dir = self.pcapng_dir.clone();

        println!("[{}] Connecting device", device_handle);

        self.counter += 1;
        let device = Device::new(device_handle, packet_tx);

        self.send_event(PicaEvent::AddDevice {
            mac_address: device.mac_address as u64,
            position: device.position.clone(),
        });

        self.devices.insert(device_handle, device);

        // Spawn and detach the connection handling task.
        // The task notifies pica when exiting to let it clean
        // the state.
        tokio::spawn(async move {
            let pcapng_file: Option<pcapng::File> = pcapng_dir
                .map(|dir| {
                    let full_path = dir.join(format!("device-{}.pcapng", device_handle));
                    println!("Recording pcapng to file {}", full_path.as_path().display());
                    pcapng::File::create(full_path)
                })
                .transpose()
                .unwrap();

            let mut connection = Connection::new(stream, pcapng_file);
            'outer: loop {
                tokio::select! {
                    // Read command packet sent from connected UWB host.
                    // Run associated command.
                    result = connection.read() => {
                        match result {
                            Ok(Some(packet)) =>
                                pica_tx.send(PicaCommand::Command(device_handle, packet)).await.unwrap(),
                            Ok(None) |
                            Err(_) => break 'outer
                        }
                    },

                    // Send response packets to the connected UWB host.
                    Some(packet) = packet_rx.recv() =>
                        if connection.write(packet).await.is_err() {
                            break 'outer
                        }
                }
            }
            pica_tx
                .send(PicaCommand::Disconnect(device_handle))
                .await
                .unwrap()
        });

        // Send device status notification with state Ready as required
        // by the UCI specification (section 6.1 Initialization of UWBS).
        self.devices
            .get(&device_handle)
            .unwrap()
            .tx
            .send(
                DeviceStatusNtfBuilder {
                    device_state: DeviceState::DeviceStateReady,
                }
                .build()
                .into(),
            )
            .await
            .unwrap()
    }

    fn disconnect(&mut self, device_handle: usize) -> Result<()> {
        println!("[{}] Disconnecting device", device_handle);

        let device = self.devices.get(&device_handle).context("Unknown device")?;

        self.send_event(PicaEvent::RemoveDevice {
            mac_address: device.mac_address as u64,
        });

        self.devices.remove(&device_handle);

        Ok(())
    }

    async fn ranging(&mut self, device_handle: usize, session_id: u32) {
        println!("[{}] Ranging event", device_handle);
        println!("  session_id={}", session_id);

        let device = self.get_device(device_handle);
        let session = device.get_session(session_id).unwrap();

        let mut measurements = Vec::new();
        session
            .get_dst_mac_addresses()
            .iter()
            .for_each(|mac_address| {
                if let Some(beacon) = self.beacons.get(mac_address) {
                    let local = device
                        .position
                        .compute_range_azimuth_elevation(&beacon.position);
                    let remote = beacon
                        .position
                        .compute_range_azimuth_elevation(&device.position);

                    assert!(local.0 == remote.0);

                    // TODO: support extended address
                    measurements.push(ShortAddressTwoWayRangingMeasurement {
                        mac_address: *mac_address as u16,
                        status: StatusCode::UciStatusOk,
                        nlos: 0, // in Line Of Sight
                        distance: local.0,
                        aoa_azimuth: local.1 as u16,
                        aoa_azimuth_fom: 100, // Yup, pretty sure about this
                        aoa_elevation: local.2 as u16,
                        aoa_elevation_fom: 100, // Yup, pretty sure about this
                        aoa_destination_azimuth: remote.1 as u16,
                        aoa_destination_azimuth_fom: 100,
                        aoa_destination_elevation: remote.2 as u16,
                        aoa_destination_elevation_fom: 100,
                        slot_index: 0,
                    });
                }
            });

        device
            .tx
            .send(
                // TODO: support extended address
                ShortMacTwoWayRangeDataNtfBuilder {
                    sequence_number: session.sequence_number,
                    session_id: session_id as u32,
                    rcr_indicator: 0,            //TODO
                    current_ranging_interval: 0, //TODO
                    two_way_ranging_measurements: measurements,
                }
                .build()
                .into(),
            )
            .await
            .unwrap();

        let device = self.get_device_mut(device_handle);
        let session = device.get_session_mut(session_id).unwrap();

        session.sequence_number += 1;
    }

    async fn command(&mut self, device_handle: usize, cmd: UciCommandPacket) -> Result<()> {
        if !self.devices.contains_key(&device_handle) {
            anyhow::bail!("Received command for disconnected device {}", device_handle);
        }

        match cmd.specialize() {
            UciCommandChild::CoreCommand(core_command) => match core_command.specialize() {
                CoreCommandChild::DeviceResetCmd(cmd) => {
                    self.get_device_mut(device_handle).device_reset(cmd).await
                }
                CoreCommandChild::GetDeviceInfoCmd(cmd) => {
                    self.get_device(device_handle).get_device_info(cmd).await
                }
                CoreCommandChild::GetCapsInfoCmd(cmd) => {
                    self.get_device(device_handle).get_caps_info(cmd).await
                }
                CoreCommandChild::SetConfigCmd(cmd) => {
                    self.get_device_mut(device_handle).set_config(cmd).await
                }
                CoreCommandChild::GetConfigCmd(cmd) => {
                    self.get_device(device_handle).get_config(cmd).await
                }
                CoreCommandChild::None => anyhow::bail!("Unsupported core command"),
            },
            UciCommandChild::SessionCommand(session_command) => {
                match session_command.specialize() {
                    SessionCommandChild::SessionInitCmd(cmd) => {
                        self.get_device_mut(device_handle).session_init(cmd).await
                    }
                    SessionCommandChild::SessionDeinitCmd(cmd) => {
                        self.get_device_mut(device_handle).session_deinit(cmd).await
                    }
                    SessionCommandChild::SessionSetAppConfigCmd(cmd) => {
                        self.get_device_mut(device_handle)
                            .session_set_app_config(cmd)
                            .await
                    }
                    SessionCommandChild::SessionGetAppConfigCmd(cmd) => {
                        self.get_device(device_handle)
                            .session_get_app_config(cmd)
                            .await
                    }
                    SessionCommandChild::SessionGetCountCmd(cmd) => {
                        self.get_device(device_handle).session_get_count(cmd).await
                    }
                    SessionCommandChild::SessionGetStateCmd(cmd) => {
                        self.get_device(device_handle).session_get_state(cmd).await
                    }
                    SessionCommandChild::SessionUpdateControllerMulticastListCmd(cmd) => {
                        self.get_device_mut(device_handle)
                            .session_update_controller_multicast_list(cmd)
                            .await
                    }
                    SessionCommandChild::None => anyhow::bail!("Unsupported session command"),
                }
            }
            UciCommandChild::RangingCommand(ranging_command) => {
                match ranging_command.specialize() {
                    RangingCommandChild::RangeStartCmd(cmd) => {
                        let pica_tx = self.tx.clone();
                        self.get_device_mut(device_handle)
                            .range_start(cmd, pica_tx)
                            .await
                    }
                    RangingCommandChild::RangeStopCmd(cmd) => {
                        self.get_device_mut(device_handle).range_stop(cmd).await
                    }
                    RangingCommandChild::RangeGetRangingCountCmd(cmd) => {
                        self.get_device_mut(device_handle)
                            .range_get_ranging_count(cmd)
                            .await
                    }
                    RangingCommandChild::None => anyhow::bail!("Unsupported ranging command"),
                }
            }
            UciCommandChild::PicaCommand(pica_command) => match pica_command.specialize() {
                PicaCommandChild::PicaInitDeviceCmd(cmd) => {
                    self.init_device(device_handle, cmd).await
                }
                PicaCommandChild::PicaSetDevicePositionCmd(cmd) => {
                    self.set_device_position(device_handle, cmd).await
                }
                PicaCommandChild::PicaCreateBeaconCmd(cmd) => {
                    self.create_beacon(device_handle, cmd).await
                }
                PicaCommandChild::PicaSetBeaconPositionCmd(cmd) => {
                    self.set_beacon_position(device_handle, cmd).await
                }
                PicaCommandChild::PicaDestroyBeaconCmd(cmd) => {
                    self.destroy_beacon(device_handle, cmd).await
                }
                PicaCommandChild::None => anyhow::bail!("Unsupported Pica command"),
            },
            UciCommandChild::AndroidCommand(android_command) => {
                match android_command.specialize() {
                    AndroidCommandChild::AndroidSetCountryCodeCmd(cmd) => {
                        self.set_country_code(device_handle, cmd).await
                    }
                    AndroidCommandChild::AndroidGetPowerStatsCmd(cmd) => {
                        self.get_power_stats(device_handle, cmd).await
                    }
                    AndroidCommandChild::None => anyhow::bail!("Unsupported ranging command"),
                }
            }
            _ => anyhow::bail!("Unsupported command type"),
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        loop {
            use PicaCommand::*;
            match self.rx.recv().await {
                Some(Connect(stream)) => self.connect(stream).await,
                Some(Disconnect(device_handle)) => self.disconnect(device_handle)?,
                Some(Ranging(device_handle, session_id)) => {
                    self.ranging(device_handle, session_id).await
                }
                Some(Command(device_handle, cmd)) => self.command(device_handle, cmd).await?,
                Some(SetPosition(mac, position)) => self.set_position(mac, position)?,
                None => (),
            }
        }
    }

    async fn init_device(
        &mut self,
        device_handle: usize,
        cmd: PicaInitDeviceCmdPacket,
    ) -> Result<()> {
        let mac_address = cmd.get_mac_address();
        let position = cmd.get_position();
        println!("[_] Init device");
        println!("  mac_address=0x{:x}", mac_address);
        println!("  position={:?}", position);

        let device = self.get_device_mut(device_handle);
        device.mac_address = mac_address as usize;
        device.position = Position::from(cmd.get_position());
        // FIXME: send event for the mac_address change
        Ok(self
            .get_device(device_handle)
            .tx
            .send(
                PicaInitDeviceRspBuilder {
                    status: StatusCode::UciStatusOk,
                }
                .build()
                .into(),
            )
            .await?)
    }

    fn update_position(&self, mac_address: MacAddress, position: Position) {
        self.send_event(PicaEvent::UpdatePosition {
            mac_address,
            position: position.clone(),
        });

        let devices = self
            .devices
            .values()
            .map(|d| (d.mac_address as MacAddress, d.position.clone()));
        let beacons = self
            .beacons
            .values()
            .map(|b| (b.mac_address, b.position.clone()));

        for (device_mac_address, device_position) in devices.chain(beacons) {
            if mac_address != device_mac_address {
                let local = position.compute_range_azimuth_elevation(&device_position);
                let remote = device_position.compute_range_azimuth_elevation(&position);

                assert!(local.0 == remote.0);

                self.send_event(PicaEvent::UpdateNeighbor {
                    mac_address,
                    neighbor: device_mac_address,
                    distance: local.0,
                    azimuth: local.1,
                    elevation: local.2,
                });

                self.send_event(PicaEvent::UpdateNeighbor {
                    mac_address: device_mac_address,
                    neighbor: mac_address,
                    distance: remote.0,
                    azimuth: remote.1,
                    elevation: remote.2,
                });
            }
        }
    }

    fn set_position(&mut self, mac_address: MacAddress, position: Position) -> Result<()> {
        if let Some(d) = self.get_device_mut_by_mac(mac_address) {
            d.position = position.clone();
        } else if let Some(b) = self.beacons.get_mut(&mac_address) {
            b.position = position.clone();
        } else {
            return Err(anyhow!("Device or Beacon not found"));
        }

        self.update_position(mac_address, position);

        Ok(())
    }

    async fn set_device_position(
        &mut self,
        device_handle: usize,
        cmd: PicaSetDevicePositionCmdPacket,
    ) -> Result<()> {
        let mut device = self.get_device_mut(device_handle);
        device.position = cmd.get_position().into();

        let position = device.position.clone();
        let mac_address = device.mac_address as u64;

        self.update_position(mac_address, position);

        Ok(self
            .get_device(device_handle)
            .tx
            .send(
                PicaSetDevicePositionRspBuilder {
                    status: StatusCode::UciStatusOk,
                }
                .build()
                .into(),
            )
            .await?)
    }

    async fn create_beacon(
        &mut self,
        device_handle: usize,
        cmd: PicaCreateBeaconCmdPacket,
    ) -> Result<()> {
        let mac_address = cmd.get_mac_address();
        let position = cmd.get_position();
        println!("[_] Create beacon");
        println!("  mac_address=0x{:x}", mac_address);
        println!("  position={:?}", position);

        let status = if self.beacons.contains_key(&mac_address) {
            StatusCode::UciStatusFailed
        } else {
            self.send_event(PicaEvent::AddDevice {
                mac_address,
                position: Position::from(position),
            });
            assert!(self
                .beacons
                .insert(
                    mac_address,
                    Beacon {
                        position: Position::from(position),
                        mac_address,
                    },
                )
                .is_none());
            StatusCode::UciStatusOk
        };

        Ok(self
            .get_device(device_handle)
            .tx
            .send(PicaCreateBeaconRspBuilder { status }.build().into())
            .await?)
    }

    async fn set_beacon_position(
        &mut self,
        device_handle: usize,
        cmd: PicaSetBeaconPositionCmdPacket,
    ) -> Result<()> {
        let mac_address = cmd.get_mac_address();
        let position = cmd.get_position();
        println!("[_] Set beacon position");
        println!("  mac_address=0x{:x}", mac_address);
        println!("  position={:?}", position);

        let status = if let Some(b) = self.beacons.get_mut(&mac_address) {
            b.position = Position::from(position);
            StatusCode::UciStatusOk
        } else {
            StatusCode::UciStatusFailed
        };

        if status == StatusCode::UciStatusOk {
            self.update_position(mac_address, Position::from(position));
        }
        Ok(self
            .get_device(device_handle)
            .tx
            .send(PicaSetBeaconPositionRspBuilder { status }.build().into())
            .await?)
    }

    async fn destroy_beacon(
        &mut self,
        device_handle: usize,
        cmd: PicaDestroyBeaconCmdPacket,
    ) -> Result<()> {
        let mac_address = cmd.get_mac_address();
        println!("[_] Destroy beacon");
        println!("  mac_address=0x{:x}", mac_address);

        let status = if self.beacons.remove(&mac_address).is_some() {
            self.send_event(PicaEvent::RemoveDevice { mac_address });
            StatusCode::UciStatusOk
        } else {
            StatusCode::UciStatusFailed
        };

        Ok(self
            .get_device(device_handle)
            .tx
            .send(PicaDestroyBeaconRspBuilder { status }.build().into())
            .await?)
    }

    async fn set_country_code(
        &mut self,
        device_handle: usize,
        cmd: AndroidSetCountryCodeCmdPacket,
    ) -> Result<()> {
        let country_code = *cmd.get_country_code();
        println!("[{}] Set country code", device_handle);
        println!("  country_code={},{}", country_code[0], country_code[1]);

        let device = self.get_device_mut(device_handle);
        device.country_code = country_code;
        Ok(device
            .tx
            .send(
                AndroidSetCountryCodeRspBuilder {
                    status: StatusCode::UciStatusOk,
                }
                .build()
                .into(),
            )
            .await?)
    }

    async fn get_power_stats(
        &mut self,
        device_handle: usize,
        _cmd: AndroidGetPowerStatsCmdPacket,
    ) -> Result<()> {
        println!("[{}] Get power stats", device_handle);

        // TODO
        let device = self.get_device(device_handle);
        Ok(device
            .tx
            .send(
                AndroidGetPowerStatsRspBuilder {
                    stats: PowerStats {
                        status: StatusCode::UciStatusOk,
                        idle_time_ms: 0,
                        tx_time_ms: 0,
                        rx_time_ms: 0,
                        total_wake_count: 0,
                    },
                }
                .build()
                .into(),
            )
            .await?)
    }
}