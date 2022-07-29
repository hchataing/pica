// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use anyhow::Result;
use bytes::{Bytes, BytesMut};
use serde::Serialize;
use std::collections::HashMap;
use std::fmt::Display;
use std::path::PathBuf;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot};

use num_traits::{FromPrimitive, ToPrimitive};

mod pcapng;

mod position;
use position::Position;

mod uci_packets;
use uci_packets::StatusCode as UciStatusCode;
use uci_packets::*;

mod device;
use device::{Device, MAX_DEVICE};

mod session;
use session::MAX_SESSION;

pub mod web;
use web::Category;

pub mod mac_address;
use mac_address::MacAddress;

const MAX_PAYLOAD_SIZE: usize = 4096;

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

    async fn read(&mut self) -> Result<Option<BytesMut>> {
        let len = self.socket.read_buf(&mut self.buffer).await?;
        if len == 0 {
            return Ok(None);
        }

        if let Some(ref mut pcapng_file) = self.pcapng_file {
            pcapng_file
                .write(&self.buffer, pcapng::Direction::Tx)
                .await?
        }

        let bytes = self.buffer.split_to(self.buffer.len());
        Ok(Some(bytes))
    }

    async fn write(&mut self, packet: Bytes) -> Result<()> {
        if let Some(ref mut pcapng_file) = self.pcapng_file {
            pcapng_file.write(&packet, pcapng::Direction::Rx).await?
        }

        let _ = self.socket.try_write(&packet)?;
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub enum PicaCommandStatus {
    Ok,
    Error(PicaCommandError),
}

impl Display for PicaCommandStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status = match self {
            PicaCommandStatus::Ok => "Ok",
            PicaCommandStatus::Error(err) => match err {
                PicaCommandError::AddAnchorFailed(_) => "AddAnchorFailed",
                PicaCommandError::DeviceNotFound(_) => "DeviceNotFound",
                PicaCommandError::SendStatusFailed(_) => "SendStatusFailed",
                PicaCommandError::SendCmdRspFailed(_) => "SendCmdRspFailed",
            },
        };
        write!(f, "PicaCommandStatus: {}", status)
    }
}

#[derive(Error, Debug, Clone, PartialEq)]
pub enum PicaCommandError {
    #[error("Device already exists: {0}")]
    AddAnchorFailed(MacAddress),
    #[error("Device not found: {0}")]
    DeviceNotFound(MacAddress),
    #[error("Failed to send error: {0}")]
    SendStatusFailed(String),
    #[error("Failed to send uci command response: {0}")]
    SendCmdRspFailed(String),
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
    // Init Uci Device
    InitUciDevice(MacAddress, Position, oneshot::Sender<PicaCommandStatus>),
    // Set Position
    SetPosition(MacAddress, Position, oneshot::Sender<PicaCommandStatus>),
    // Create Anchor
    CreateAnchor(MacAddress, Position, oneshot::Sender<PicaCommandStatus>),
    // Destroy Anchor
    DestroyAnchor(MacAddress, oneshot::Sender<PicaCommandStatus>),
    // Get State
    GetState(oneshot::Sender<Vec<web::Device>>),
}

impl Display for PicaCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cmd = match self {
            PicaCommand::Connect(_) => "Connect",
            PicaCommand::Disconnect(_) => "Disconnect",
            PicaCommand::Ranging(_, _) => "Ranging",
            PicaCommand::Command(_, _) => "Command",
            PicaCommand::InitUciDevice(_, _, _) => "InitUciDevice",
            PicaCommand::SetPosition(_, _, _) => "SetPosition",
            PicaCommand::CreateAnchor(_, _, _) => "CreateAnchor",
            PicaCommand::DestroyAnchor(_, _) => "DestroyAnchor",
            PicaCommand::GetState(_) => "GetState",
        };
        write!(f, "{}", cmd)
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum PicaEvent {
    // A Device was added
    DeviceAdded {
        device: web::Device,
    },
    // A Device was removed
    DeviceRemoved {
        device: web::Device,
    },
    // A Device position has changed
    DeviceUpdated {
        device: web::Device,
    },
    NeighborUpdated {
        source_device: web::Device,
        destination_device: web::Device,
        distance: u16,
        azimuth: i16,
        elevation: i8,
    },
}

#[derive(Debug, Clone, Copy)]
struct Anchor {
    mac_address: MacAddress,
    position: Position,
}

pub struct Pica {
    devices: HashMap<usize, Device>,
    anchors: HashMap<MacAddress, Anchor>,
    counter: usize,
    rx: mpsc::Receiver<PicaCommand>,
    tx: mpsc::Sender<PicaCommand>,
    event_tx: broadcast::Sender<PicaEvent>,
    pcapng_dir: Option<PathBuf>,
}

/// Result of UCI packet parsing.
enum UciParseResult {
    Ok(UciCommandPacket),
    Err(Bytes),
    Skip,
}

/// Parse incoming UCI packets.
/// Handle parsing errors by crafting a suitable error response packet.
fn parse_uci_packet(bytes: &[u8]) -> UciParseResult {
    match UciPacketPacket::parse(bytes) {
        // Parsing error. Determine what error response should be
        // returned to the host:
        // - response and notifications are ignored, no response
        // - if the group id is not known, STATUS_UNKNOWN_GID,
        // - otherwise, and to simplify the code, STATUS_UNKNOWN_OID is
        //      always returned. That means that malformed commands
        //      get the same status code, instead of
        //      STATUS_SYNTAX_ERROR.
        Err(_) => {
            let message_type = (bytes[0] >> 5) & 0x7;
            let group_id = bytes[0] & 0xf;
            let opcode_id = bytes[1] & 0x3f;

            let status = match (
                MessageType::from_u8(message_type),
                GroupId::from_u8(group_id),
            ) {
                (Some(MessageType::Command), Some(_)) => UciStatusCode::UciStatusUnknownOid,
                (Some(MessageType::Command), None) => UciStatusCode::UciStatusUnknownGid,
                _ => return UciParseResult::Skip,
            };
            // The PDL generated code cannot be used to generate
            // responses with invalid group identifiers.
            let response = vec![
                (MessageType::Response.to_u8().unwrap() << 5) | group_id,
                opcode_id,
                0,
                1,
                status.to_u8().unwrap(),
            ];
            UciParseResult::Err(response.into())
        }

        // Parsing success, ignore non command packets.
        Ok(packet) => {
            if let Ok(cmd) = packet.try_into() {
                UciParseResult::Ok(cmd)
            } else {
                UciParseResult::Skip
            }
        }
    }
}

impl Pica {
    pub fn new(event_tx: broadcast::Sender<PicaEvent>, pcapng_dir: Option<PathBuf>) -> Self {
        let (tx, rx) = mpsc::channel(MAX_SESSION * MAX_DEVICE);
        Pica {
            devices: HashMap::new(),
            anchors: HashMap::new(),
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

    fn get_device_mut(&mut self, device_handle: usize) -> Option<&mut Device> {
        self.devices.get_mut(&device_handle)
    }

    fn get_device(&self, device_handle: usize) -> Option<&Device> {
        self.devices.get(&device_handle)
    }

    fn get_category(&self, mac_address: &MacAddress) -> Option<Category> {
        if self.anchors.contains_key(mac_address) {
            Some(Category::Anchor)
        } else if self
            .devices
            .iter()
            .any(|(_, device)| device.mac_address == *mac_address)
        {
            Some(Category::Uci)
        } else {
            None
        }
    }

    fn get_device_mut_by_mac(&mut self, mac_address: MacAddress) -> Option<&mut Device> {
        self.devices
            .values_mut()
            .find(|d| d.mac_address == mac_address)
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
        let mut device = Device::new(device_handle, packet_tx, self.tx.clone());
        device.init();

        self.send_event(PicaEvent::DeviceAdded {
            device: web::Device::new(Category::Uci, device.mac_address, device.position),
        });

        self.devices.insert(device_handle, device);

        // Spawn and detach the connection handling task.
        // The task notifies pica when exiting to let it clean
        // the state.
        tokio::spawn(async move {
            let pcapng_file: Option<pcapng::File> = if let Some(dir) = pcapng_dir {
                let full_path = dir.join(format!("device-{}.pcapng", device_handle));
                println!("Recording pcapng to file {}", full_path.as_path().display());
                Some(pcapng::File::create(full_path).await.unwrap())
            } else {
                None
            };

            let mut connection = Connection::new(stream, pcapng_file);
            'outer: loop {
                tokio::select! {
                    // Read command packet sent from connected UWB host.
                    // Run associated command.
                    result = connection.read() =>
                        match result {
                            Ok(Some(packet)) =>
                                match parse_uci_packet(&packet) {
                                    UciParseResult::Ok(cmd) =>
                                        pica_tx.send(PicaCommand::Command(device_handle, cmd)).await.unwrap(),
                                    UciParseResult::Err(response) =>
                                        connection.write(response).await.unwrap(),
                                    UciParseResult::Skip => (),
                                },
                            Ok(None) | Err(_) => break 'outer
                        },

                    // Send response packets to the connected UWB host.
                    Some(packet) = packet_rx.recv() =>
                        if connection.write(packet.to_bytes()).await.is_err() {
                            break 'outer
                        }
                }
            }
            pica_tx
                .send(PicaCommand::Disconnect(device_handle))
                .await
                .unwrap()
        });
    }

    fn disconnect(&mut self, device_handle: usize) {
        println!("[{}] Disconnecting device", device_handle);

        match self
            .devices
            .get(&device_handle)
            .ok_or_else(|| PicaCommandError::DeviceNotFound(device_handle.into()))
        {
            Ok(device) => {
                self.send_event(PicaEvent::DeviceRemoved {
                    device: web::Device::new(Category::Uci, device.mac_address, device.position),
                });
                self.devices.remove(&device_handle);
            }
            Err(err) => println!("{}", err),
        }
    }

    async fn ranging(&mut self, device_handle: usize, session_id: u32) {
        println!("[{}] Ranging event", device_handle);
        println!("  session_id={}", session_id);

        let device = self.get_device(device_handle).unwrap();
        let session = device.get_session(session_id).unwrap();

        let mut measurements = Vec::new();
        session
            .get_dst_mac_addresses()
            .iter()
            .for_each(|mac_address| {
                if let Some(anchor) = self.anchors.get(mac_address) {
                    let local = device
                        .position
                        .compute_range_azimuth_elevation(&anchor.position);
                    let remote = anchor
                        .position
                        .compute_range_azimuth_elevation(&device.position);

                    assert!(local.0 == remote.0);

                    // TODO: support extended address
                    match mac_address {
                        MacAddress::Short(address) => {
                            measurements.push(ShortAddressTwoWayRangingMeasurement {
                                mac_address: u16::from_be_bytes(*address),
                                status: UciStatusCode::UciStatusOk,
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
                            })
                        }
                        MacAddress::Extend(_) => unimplemented!(),
                    }
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

        let device = self.get_device_mut(device_handle).unwrap();
        let session = device.get_session_mut(session_id).unwrap();

        session.sequence_number += 1;
    }

    async fn command(&mut self, device_handle: usize, cmd: UciCommandPacket) {
        // TODO: implement fragmentation support
        assert_eq!(
            cmd.get_packet_boundary_flag(),
            PacketBoundaryFlag::Complete,
            "Boundary flag is true, implement fragmentation"
        );

        match self
            .get_device_mut(device_handle)
            .ok_or_else(|| PicaCommandError::DeviceNotFound(device_handle.into()))
        {
            Ok(device) => {
                let response = device.command(cmd).into();
                device.tx.send(response).await.unwrap_or_else(|err| {
                    println!("{}", PicaCommandError::SendStatusFailed(err.to_string()))
                });
            }
            Err(err) => println!("{}", err),
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        loop {
            use PicaCommand::*;
            match self.rx.recv().await {
                Some(Connect(stream)) => {
                    self.connect(stream).await;
                }
                Some(Disconnect(device_handle)) => self.disconnect(device_handle),
                Some(Ranging(device_handle, session_id)) => {
                    self.ranging(device_handle, session_id).await;
                }
                Some(Command(device_handle, cmd)) => self.command(device_handle, cmd).await,
                Some(SetPosition(mac_address, position, pica_cmd_rsp_tx)) => {
                    self.set_position(mac_address, position, pica_cmd_rsp_tx)
                }
                Some(CreateAnchor(mac_address, position, pica_cmd_rsp_tx)) => {
                    self.create_anchor(mac_address, position, pica_cmd_rsp_tx)
                }
                Some(DestroyAnchor(mac_address, pica_cmd_rsp_tx)) => {
                    self.destroy_anchor(mac_address, pica_cmd_rsp_tx)
                }
                Some(GetState(state_tx)) => self.get_state(state_tx),
                Some(InitUciDevice(mac_address, position, pica_cmd_rsp_tx)) => {
                    self.init_uci_device(mac_address, position, pica_cmd_rsp_tx);
                }
                None => (),
            };
        }
    }

    // TODO: Assign a reserved range of mac addresses for UCI devices
    // to protect against conflicts  with user defined Anchor addresses
    // b/246000641
    fn init_uci_device(
        &mut self,
        mac_address: MacAddress,
        position: Position,
        pica_cmd_rsp_tx: oneshot::Sender<PicaCommandStatus>,
    ) {
        println!("[_] Init device");
        println!("  mac_address: {}", mac_address);
        println!("  position={:?}", position);

        let status = match self
            .get_device_mut_by_mac(mac_address)
            .ok_or(PicaCommandError::DeviceNotFound(mac_address))
        {
            Ok(uci_device) => {
                uci_device.mac_address = mac_address;
                uci_device.position = position;
                PicaCommandStatus::Ok
            }
            Err(err) => {
                println!("{}", err);
                PicaCommandStatus::Error(err)
            }
        };

        pica_cmd_rsp_tx.send(status).unwrap_or_else(|err| {
            println!("{}", PicaCommandError::SendStatusFailed(err.to_string()))
        });
    }

    fn set_position(
        &mut self,
        mac_address: MacAddress,
        position: Position,
        pica_cmd_rsp_tx: oneshot::Sender<PicaCommandStatus>,
    ) {
        let mut status = if let Some(uci_device) = self.get_device_mut_by_mac(mac_address) {
            uci_device.position = position;
            PicaCommandStatus::Ok
        } else if let Some(anchor) = self.anchors.get_mut(&mac_address) {
            anchor.position = position;
            PicaCommandStatus::Ok
        } else {
            let err = PicaCommandError::DeviceNotFound(mac_address);
            println!("{}", err);
            PicaCommandStatus::Error(err)
        };

        if status == PicaCommandStatus::Ok {
            status = match self.update_position(mac_address, position) {
                Ok(_) => PicaCommandStatus::Ok,
                Err(err) => {
                    println!("{}", err);
                    PicaCommandStatus::Error(err)
                }
            };
        }

        pica_cmd_rsp_tx.send(status).unwrap_or_else(|err| {
            println!("{}", PicaCommandError::SendStatusFailed(err.to_string()))
        });
    }

    fn update_position(
        &self,
        mac_address: MacAddress,
        position: Position,
    ) -> Result<(), PicaCommandError> {
        let category = match self.get_category(&mac_address) {
            Some(category) => category,
            None => {
                return Err(PicaCommandError::DeviceNotFound(mac_address));
            }
        };
        self.send_event(PicaEvent::DeviceUpdated {
            device: web::Device::new(category, mac_address, position),
        });

        let devices = self.devices.values().map(|d| (d.mac_address, d.position));
        let anchors = self.anchors.values().map(|b| (b.mac_address, b.position));

        let update_neighbors = |category, device_mac_address, device_position| {
            if mac_address != device_mac_address {
                let local = position.compute_range_azimuth_elevation(&device_position);
                let remote = device_position.compute_range_azimuth_elevation(&position);

                assert!(local.0 == remote.0);

                self.send_event(PicaEvent::NeighborUpdated {
                    source_device: web::Device::new(category, mac_address, position),
                    destination_device: web::Device::new(
                        category,
                        device_mac_address,
                        device_position,
                    ),
                    distance: local.0,
                    azimuth: local.1,
                    elevation: local.2,
                });

                self.send_event(PicaEvent::NeighborUpdated {
                    source_device: web::Device::new(category, device_mac_address, device_position),
                    destination_device: web::Device::new(category, mac_address, position),
                    distance: remote.0,
                    azimuth: remote.1,
                    elevation: remote.2,
                });
            }
        };

        devices.for_each(|device| update_neighbors(Category::Uci, device.0, device.1));
        anchors.for_each(|anchor| update_neighbors(Category::Anchor, anchor.0, anchor.1));
        Ok(())
    }

    #[allow(clippy::map_entry)]
    fn create_anchor(
        &mut self,
        mac_address: MacAddress,
        position: Position,
        pica_cmd_rsp_tx: oneshot::Sender<PicaCommandStatus>,
    ) {
        println!("Create anchor: {} {}", mac_address, position);
        let status = if self.get_category(&mac_address).is_some() {
            let err = PicaCommandError::AddAnchorFailed(mac_address);
            println!("{}", err);
            PicaCommandStatus::Error(err)
        } else {
            self.send_event(PicaEvent::DeviceAdded {
                device: web::Device::new(Category::Anchor, mac_address, position),
            });
            assert!(self
                .anchors
                .insert(
                    mac_address,
                    Anchor {
                        mac_address,
                        position,
                    },
                )
                .is_none());
            PicaCommandStatus::Ok
        };

        pica_cmd_rsp_tx.send(status).unwrap_or_else(|err| {
            println!("{}", PicaCommandError::SendStatusFailed(err.to_string()))
        })
    }

    fn destroy_anchor(
        &mut self,
        mac_address: MacAddress,
        pica_cmd_rsp_tx: oneshot::Sender<PicaCommandStatus>,
    ) {
        println!("[_] Destroy anchor");
        println!("  mac_address: {}", mac_address);

        let status = if self.anchors.remove(&mac_address).is_none() {
            let err = PicaCommandError::DeviceNotFound(mac_address);
            println!("{}", err);
            PicaCommandStatus::Error(err)
        } else {
            self.send_event(PicaEvent::DeviceRemoved {
                device: web::Device::new(Category::Anchor, mac_address, Position::default()),
            });
            PicaCommandStatus::Ok
        };
        pica_cmd_rsp_tx.send(status).unwrap_or_else(|err| {
            println!("{}", PicaCommandError::SendStatusFailed(err.to_string()))
        })
    }

    fn get_state(&self, state_tx: oneshot::Sender<Vec<web::Device>>) {
        println!("[_] Get State");
        let web_devices: Vec<web::Device> = self
            .anchors
            .iter()
            .map(|(_, anchor)| web::Device::from(*anchor))
            .chain(self.devices.iter().map(|(_, uci_device)| {
                web::Device::new(Category::Uci, uci_device.mac_address, uci_device.position)
            }))
            .collect();

        state_tx.send(web_devices).unwrap();
    }
}
