//! A Basic Modbus client for the Umg605Pro device.
//! 
//! See [Modbus register map] for the list of registers that can be read from the device.
//! 
//! For more information about the Umg605Pro device, see the [official documentation](https://www.janitza.com/en/products/umg-605-pro/downloads).
//! 
//! [Modbus register map]: https://assets.janitza.com/ce18jq9ih0x6/b83ae2356a42a682591109/ef2bc2b24a6b7c77de4dbda20e43cebf/janitza-mal-umg605pro-en.pdf

use std::borrow::Cow;
use std::net::{SocketAddr};
use std::time::{Duration};

use tokio_modbus::client::Reader;
use tokio_modbus::Slave;

/// Default Modbus TCP port
pub const DEFAULT_MODBUS_PORT: u16 = 502;

/// TCP Modbus client for the Umg605Pro device.
pub struct Umg605ProClient {
    client: tokio_modbus::client::Context,
    timeout: Duration,
}

/// Errors that can occur when connecting to the Umg605Pro device.
#[derive(thiserror::Error, Debug)]
pub enum ConnectError {
    #[error("connection to {0} timed out after {1:?}")]
    Timeout(SocketAddr, Duration),
    #[error("failed to connect: {0}")]
    Connect(std::io::Error),
}

impl Umg605ProClient {
    /// Creates a new instance of the Umg605ProClient over TCP Modbus.
    /// 
    /// ### Parameters
    /// * `socket_addr` is the IP address and port of the Umg605Pro device.
    /// * `unit` is the Modbus unit id configured on the meter. For TCP Modbus, this can usually be set to 1.
    /// * `timeout` is the timeout for connecting and for each register read.
    pub async fn connect_tcp(
        socket_addr: SocketAddr,
        unit: Slave,
        timeout: Duration,
    ) -> Result<Self, ConnectError> {
        let modbus_context = tokio::time::timeout(
            timeout,
            tokio_modbus::client::tcp::connect_slave(socket_addr, unit),
        )
        .await
            .map_err(|_| ConnectError::Timeout(socket_addr, timeout))?
            .map_err(ConnectError::Connect)?;

        Ok(Umg605ProClient {
            client: modbus_context,
            timeout,
        })
    }
}


#[derive(thiserror::Error, Debug)]
pub enum ReadError {
    #[error("read timed out after {0:?}")]
    Timeout(Duration),
    #[error("transport error: {0}")]
    Transport(#[from] std::io::Error),
    #[error("protocol error: {0}")]
    Protocol(#[from] tokio_modbus::ProtocolError),
    #[error("modbus exception: {0}")]
    ModbusException(#[from] tokio_modbus::ExceptionCode),
    #[error("decode error: {0}")]
    DecodeError(Cow<'static, str>),
}

impl Umg605ProClient {

    /// Reads `count` holding registers, returning a vector of `u16` values.
    pub async fn read_holding_registers(&mut self, addr: u16, count: u16) -> Result<Vec<u16>, ReadError> {
        tokio::time::timeout(self.timeout, self.client.read_holding_registers(addr, count))
            .await
            .map_err(|_| ReadError::Timeout(self.timeout))?
            .map_err(|e| match e {
                tokio_modbus::Error::Protocol(protocol_error) => ReadError::Protocol(protocol_error),
                tokio_modbus::Error::Transport(error) => ReadError::Transport(error),
            })?
            .map_err(ReadError::ModbusException)
    }


    /// Reads a float32 value spanning the two holding registers starting at `addr`.
    pub async fn read_f32(&mut self, addr: u16) -> Result<f32, ReadError> {
        let regs = self.read_holding_registers(addr, 2).await?;
        let [hi, lo] = regs[..] else {
            return Err(ReadError::DecodeError(Cow::Owned(format!(
                "expected 2 registers at {addr}, got {}",
                regs.len()
            ))));
        };
        Ok(f32::from_bits(((hi as u32) << 16) | (lo as u32)))
    }
}



// Register reading functions for the Umg605Pro device.
impl Umg605ProClient {

    // --- L1 Variants ---

    /// Fetches the voltage of phase L1 in volts.
    pub async fn voltage_l1(&mut self) -> Result<f32, ReadError> { self.read_f32(19000).await }

    /// Fetches the current of phase L1 in amperes.
    pub async fn current_l1(&mut self) -> Result<f32, ReadError> { self.read_f32(19012).await }

    /// Fetches the active power of phase L1 to neutral in watts.
    pub async fn power_l1_n(&mut self) -> Result<f32, ReadError> { self.read_f32(19020).await }

    /// Fetches the apparent power of phase L1 in VA.
    pub async fn apparent_power_l1(&mut self) -> Result<f32, ReadError> { self.read_f32(19028).await }

    /// Fetches the reactive power of phase L1 in var.
    pub async fn reactive_power_l1(&mut self) -> Result<f32, ReadError> { self.read_f32(19036).await }

    /// Fetches the CosPhi (power factor) of phase L1.
    pub async fn cosphi_l1(&mut self) -> Result<f32, ReadError> { self.read_f32(19044).await }

    /// Fetches the THD (Total Harmonic Distortion) Voltage L1-N in %.
    pub async fn thd_voltage_l1(&mut self) -> Result<f32, ReadError> { self.read_f32(19110).await }

    /// Fetches the THD (Total Harmonic Distortion) Current L1 in %.
    pub async fn thd_current_l1(&mut self) -> Result<f32, ReadError> { self.read_f32(19116).await }

    // --- Global Variants ---

    /// Fetches the measured frequency in Hz.
    pub async fn frequency(&mut self) -> Result<f32, ReadError> { self.read_f32(19050).await }

    /// Fetches the vector sum of Current (I1 + I2 + I3) in amperes.
    pub async fn current_sum(&mut self) -> Result<f32, ReadError> { self.read_f32(19018).await }

    /// Fetches the sum of Real power (P1 + P2 + P3) in watts.
    pub async fn real_power_sum(&mut self) -> Result<f32, ReadError> { self.read_f32(19026).await }

    /// Fetches the sum of Apparent power (S1 + S2 + S3) in VA.
    pub async fn apparent_power_sum(&mut self) -> Result<f32, ReadError> { self.read_f32(19034).await }

    /// Fetches the sum of Reactive power (Q1 + Q2 + Q3) in var.
    pub async fn reactive_power_sum(&mut self) -> Result<f32, ReadError> { self.read_f32(19042).await }
}
