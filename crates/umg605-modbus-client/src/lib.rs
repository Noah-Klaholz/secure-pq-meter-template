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

    /// Reads an i32 value spanning the two holding registers starting at `addr`.
    pub async fn read_i32(&mut self, addr: u16) -> Result<i32, ReadError> {
        let regs = self.read_holding_registers(addr, 2).await?;
        let [hi, lo] = regs[..] else {
            return Err(ReadError::DecodeError(Cow::Owned(format!(
                "expected 2 registers at {addr}, got {}",
                regs.len()
            ))));
        };
        Ok((((hi as u32) << 16) | (lo as u32)) as i32)
    }

    /// Reads `count` consecutive holding registers as one Modbus transaction.
    ///
    /// Values are then decoded from the returned block by address. Reading a range in one
    /// go costs one round trip instead of one per value, which is what keeps a fast poll
    /// interval affordable.
    pub async fn read_block(&mut self, start: u16, count: u16) -> Result<RegisterBlock, ReadError> {
        let registers = self.read_holding_registers(start, count).await?;
        if registers.len() != usize::from(count) {
            return Err(ReadError::DecodeError(Cow::Owned(format!(
                "expected {count} registers at {start}, got {}",
                registers.len()
            ))));
        }
        Ok(RegisterBlock { start, registers })
    }
}

/// A range of holding registers fetched in a single read.
pub struct RegisterBlock {
    start: u16,
    registers: Vec<u16>,
}

impl RegisterBlock {
    pub fn new(start: u16, registers: Vec<u16>) -> Self {
        Self { start, registers }
    }

    /// The two registers holding the 32-bit value at `addr`.
    fn pair(&self, addr: u16) -> Result<(u16, u16), ReadError> {
        let offset = addr
            .checked_sub(self.start)
            .map(usize::from)
            .filter(|offset| offset + 1 < self.registers.len())
            .ok_or_else(|| {
                ReadError::DecodeError(Cow::Owned(format!(
                    "address {addr} is outside the block at {} of {} registers",
                    self.start,
                    self.registers.len()
                )))
            })?;
        Ok((self.registers[offset], self.registers[offset + 1]))
    }

    /// Decodes the float32 value at `addr`.
    pub fn f32_at(&self, addr: u16) -> Result<f32, ReadError> {
        let (hi, lo) = self.pair(addr)?;
        Ok(f32::from_bits(((hi as u32) << 16) | (lo as u32)))
    }

    /// Decodes the int32 value at `addr`.
    pub fn i32_at(&self, addr: u16) -> Result<i32, ReadError> {
        let (hi, lo) = self.pair(addr)?;
        Ok((((hi as u32) << 16) | (lo as u32)) as i32)
    }
}

/// Addresses of the registers read together by [`Umg605ProClient::snapshot`].
pub mod reg {
    pub const SYSTIME: u16 = 4;
    pub const VOLTAGE_L1: u16 = 19000;
    pub const CURRENT_L1: u16 = 19012;
    pub const REAL_POWER_L1: u16 = 19020;
    pub const APPARENT_POWER_L1: u16 = 19028;
    pub const REACTIVE_POWER_L1: u16 = 19036;
    pub const COS_PHI_L1: u16 = 19044;
    pub const FREQUENCY: u16 = 19050;
    pub const REAL_ENERGY_CONSUMED_L1: u16 = 19062;
    pub const THD_CURRENT_L1: u16 = 19116;

    /// Registers spanning [`VOLTAGE_L1`] up to and including [`REAL_ENERGY_CONSUMED_L1`].
    /// Modbus allows up to 125 per read, so this fits in one transaction.
    pub const MEASUREMENT_BLOCK_LEN: u16 = REAL_ENERGY_CONSUMED_L1 + 2 - VOLTAGE_L1;
}

/// One consistent set of readings taken from the meter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Snapshot {
    pub systime: i32,
    pub frequency: f32,
    pub voltage_l1: f32,
    pub current_l1: f32,
    pub real_power_l1: f32,
    pub apparent_power_l1: f32,
    pub reactive_power_l1: f32,
    pub cos_phi_l1: f32,
    pub real_energy_consumed_l1: f32,
    pub thd_current_l1: f32,
}

impl Umg605ProClient {
    /// Reads every value of a [`Snapshot`] in three Modbus transactions.
    ///
    /// The measured values sit in one contiguous range, so they come back in a single
    /// read; the clock and the THD register live elsewhere and cost one read each.
    pub async fn snapshot(&mut self) -> Result<Snapshot, ReadError> {
        let clock = self.read_block(reg::SYSTIME, 2).await?;
        let measurements = self
            .read_block(reg::VOLTAGE_L1, reg::MEASUREMENT_BLOCK_LEN)
            .await?;
        let thd = self.read_block(reg::THD_CURRENT_L1, 2).await?;

        Ok(Snapshot {
            systime: clock.i32_at(reg::SYSTIME)?,
            frequency: measurements.f32_at(reg::FREQUENCY)?,
            voltage_l1: measurements.f32_at(reg::VOLTAGE_L1)?,
            current_l1: measurements.f32_at(reg::CURRENT_L1)?,
            real_power_l1: measurements.f32_at(reg::REAL_POWER_L1)?,
            apparent_power_l1: measurements.f32_at(reg::APPARENT_POWER_L1)?,
            reactive_power_l1: measurements.f32_at(reg::REACTIVE_POWER_L1)?,
            cos_phi_l1: measurements.f32_at(reg::COS_PHI_L1)?,
            real_energy_consumed_l1: measurements.f32_at(reg::REAL_ENERGY_CONSUMED_L1)?,
            thd_current_l1: thd.f32_at(reg::THD_CURRENT_L1)?,
        })
    }
}

// Register reading functions for the Umg605Pro device.
impl Umg605ProClient {

    pub async fn voltage_l1(&mut self) -> Result<f32, ReadError> {
        self.read_f32(reg::VOLTAGE_L1).await
    }

    pub async fn current_l1(&mut self) -> Result<f32, ReadError> {
        self.read_f32(reg::CURRENT_L1).await
    }

    pub async fn real_power_l1(&mut self) -> Result<f32, ReadError> {
        self.read_f32(reg::REAL_POWER_L1).await
    }

    /// Alias for real_power_l1
    pub async fn power_l1_n(&mut self) -> Result<f32, ReadError> {
        self.real_power_l1().await
    }

    pub async fn apparent_power_l1(&mut self) -> Result<f32, ReadError> {
        self.read_f32(reg::APPARENT_POWER_L1).await
    }

    pub async fn reactive_power_l1(&mut self) -> Result<f32, ReadError> {
        self.read_f32(reg::REACTIVE_POWER_L1).await
    }

    pub async fn cos_phi_l1(&mut self) -> Result<f32, ReadError> {
        self.read_f32(reg::COS_PHI_L1).await
    }

    pub async fn frequency(&mut self) -> Result<f32, ReadError> {
        self.read_f32(reg::FREQUENCY).await
    }

    pub async fn real_energy_consumed_l1(&mut self) -> Result<f32, ReadError> {
        self.read_f32(reg::REAL_ENERGY_CONSUMED_L1).await
    }

    pub async fn thd_current_l1(&mut self) -> Result<f32, ReadError> {
        self.read_f32(reg::THD_CURRENT_L1).await
    }

    pub async fn systime(&mut self) -> Result<i32, ReadError> {
        self.read_i32(reg::SYSTIME).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_f32(val: f32) -> (u16, u16) {
        let bits = val.to_bits();
        ((bits >> 16) as u16, (bits & 0xFFFF) as u16)
    }

    fn encode_i32(val: i32) -> (u16, u16) {
        let bits = val as u32;
        ((bits >> 16) as u16, (bits & 0xFFFF) as u16)
    }

    #[test]
    fn decodes_f32_and_i32_from_register_block() {
        let (f_hi, f_lo) = encode_f32(230.5);
        let (i_hi, i_lo) = encode_i32(1_700_000_000);
        let block = RegisterBlock::new(100, vec![f_hi, f_lo, i_hi, i_lo]);

        assert_eq!(block.f32_at(100).unwrap(), 230.5);
        assert_eq!(block.i32_at(102).unwrap(), 1_700_000_000);
    }

    #[test]
    fn decodes_negative_and_zero_values() {
        let (f_hi, f_lo) = encode_f32(-50.25);
        let (i_hi, i_lo) = encode_i32(-42);
        let (z_hi, z_lo) = encode_f32(0.0);
        let block = RegisterBlock::new(0, vec![f_hi, f_lo, i_hi, i_lo, z_hi, z_lo]);

        assert_eq!(block.f32_at(0).unwrap(), -50.25);
        assert_eq!(block.i32_at(2).unwrap(), -42);
        assert_eq!(block.f32_at(4).unwrap(), 0.0);
    }

    #[test]
    fn register_block_bounds_and_underflow_checks() {
        let (f_hi, f_lo) = encode_f32(12.34);
        let block = RegisterBlock::new(100, vec![f_hi, f_lo, 3, 4]);

        // Address before start
        assert!(matches!(block.f32_at(98), Err(ReadError::DecodeError(_))));
        assert!(matches!(block.f32_at(99), Err(ReadError::DecodeError(_))));

        // Valid addresses
        assert_eq!(block.f32_at(100).unwrap(), 12.34);
        assert!(block.f32_at(102).is_ok());

        // Partial register at the end (103 is valid offset, but 104 is out of bounds)
        assert!(matches!(block.f32_at(103), Err(ReadError::DecodeError(_))));

        // Address past the end
        assert!(matches!(block.f32_at(104), Err(ReadError::DecodeError(_))));
        assert!(matches!(block.f32_at(200), Err(ReadError::DecodeError(_))));

        // Underflow check: addr 0 when start is 100
        assert!(matches!(block.f32_at(0), Err(ReadError::DecodeError(_))));
    }

    #[test]
    fn measurement_block_length_fits_modbus_limit() {
        assert_eq!(
            reg::MEASUREMENT_BLOCK_LEN,
            reg::REAL_ENERGY_CONSUMED_L1 + 2 - reg::VOLTAGE_L1
        );
        // Modbus TCP allows at most 125 registers per read request
        assert!(reg::MEASUREMENT_BLOCK_LEN <= 125);
    }
}

