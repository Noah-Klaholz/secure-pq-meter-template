//! A Basic Modbus client for the Umg605Pro device.
//!
//! See [Modbus register map] for the list of registers that can be read from the device.
//!
//! For more information about the Umg605Pro device, see the [official documentation](https://www.janitza.com/en/products/umg-605-pro/downloads).
//!
//! [Modbus register map]: https://assets.janitza.com/ce18jq9ih0x6/b83ae2356a42a682591109/ef2bc2b24a6b7c77de4dbda20e43cebf/janitza-mal-umg605pro-en.pdf

use std::borrow::Cow;
use std::net::SocketAddr;
use std::time::Duration;

use tokio_modbus::Slave;
use tokio_modbus::client::Reader;

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
    pub async fn read_holding_registers(
        &mut self,
        addr: u16,
        count: u16,
    ) -> Result<Vec<u16>, ReadError> {
        tokio::time::timeout(
            self.timeout,
            self.client.read_holding_registers(addr, count),
        )
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

    /// Decodes the three float32 values of a per-phase quantity whose L1 value is at `addr`.
    pub fn phases_at(&self, addr: u16) -> Result<Phases, ReadError> {
        let mut values = [0.0; PHASE_COUNT];
        for (phase, value) in values.iter_mut().enumerate() {
            *value = self.f32_at(addr + reg::PHASE_STRIDE * phase as u16)?;
        }
        Ok(values)
    }
}

/// Number of phases a three-phase meter measures.
pub const PHASE_COUNT: usize = 3;

/// One value per phase, ordered L1, L2, L3.
pub type Phases = [f32; PHASE_COUNT];

/// Addresses of the registers read together by [`Umg605ProClient::snapshot`].
///
/// The three values of a per-phase quantity sit next to each other, L1 first, so the
/// address of L2 and L3 is the address of L1 plus one or two [`PHASE_STRIDE`].
pub mod reg {
    pub const SYSTIME: u16 = 4;

    /// Registers between the value of one phase and the same value of the next phase.
    pub const PHASE_STRIDE: u16 = 2;

    pub const VOLTAGE_L1: u16 = 19000;
    pub const CURRENT_L1: u16 = 19012;
    pub const REAL_POWER_L1: u16 = 19020;
    pub const REAL_POWER_SUM3: u16 = 19026;
    pub const APPARENT_POWER_L1: u16 = 19028;
    pub const APPARENT_POWER_SUM3: u16 = 19034;
    pub const REACTIVE_POWER_L1: u16 = 19036;
    pub const REACTIVE_POWER_SUM3: u16 = 19042;
    pub const COS_PHI_L1: u16 = 19044;
    pub const FREQUENCY: u16 = 19050;
    pub const REAL_ENERGY_CONSUMED_L1: u16 = 19062;
    pub const THD_VOLTAGE_L1: u16 = 19110;
    pub const THD_CURRENT_L1: u16 = 19116;
    /// Last measured value of the block, and therefore the end of it.
    pub const THD_CURRENT_L3: u16 = 19120;

    /// First register of the block covering every measured value of a `Snapshot`.
    pub const MEASUREMENT_BLOCK_START: u16 = VOLTAGE_L1;

    /// Registers from [`MEASUREMENT_BLOCK_START`] up to and including the second register
    /// of [`THD_CURRENT_L3`]. Modbus allows up to 125 registers per read, so the complete
    /// three-phase snapshot fits in one transaction.
    pub const MEASUREMENT_BLOCK_LEN: u16 = THD_CURRENT_L3 + 2 - MEASUREMENT_BLOCK_START;

    /// Reading more than this in one request is outside the Modbus TCP spec. Extending the
    /// block past the limit has to fail the build rather than the meter.
    const MODBUS_MAX_REGISTERS_PER_READ: u16 = 125;
    const _: () = assert!(MEASUREMENT_BLOCK_LEN <= MODBUS_MAX_REGISTERS_PER_READ);
}

/// One consistent set of readings taken from the meter.
///
/// Quantities the meter cannot determine come back as NaN rather than as an error: a phase
/// with nothing connected to it carries no current, and the harmonic distortion of a current
/// that is not there is undefined. Callers have to treat every field as possibly non-finite.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Snapshot {
    pub systime: i32,
    pub frequency: f32,
    pub voltage: Phases,
    pub current: Phases,
    pub real_power: Phases,
    /// The meter's own sum P1+P2+P3, negative when the installation exports power.
    pub real_power_sum3: f32,
    pub apparent_power: Phases,
    pub apparent_power_sum3: f32,
    pub reactive_power: Phases,
    pub reactive_power_sum3: f32,
    pub cos_phi: Phases,
    pub real_energy_consumed: Phases,
    pub thd_voltage: Phases,
    pub thd_current: Phases,
}

impl Umg605ProClient {
    /// Reads every value of a [`Snapshot`] in two Modbus transactions.
    ///
    /// All measured values lie in one contiguous range, so the complete three-phase
    /// snapshot arrives in a single read and the values are consistent with each other.
    /// Only the clock, which sits at the very start of the register map, costs a second
    /// read.
    pub async fn snapshot(&mut self) -> Result<Snapshot, ReadError> {
        let clock = self.read_block(reg::SYSTIME, 2).await?;
        let measurements = self
            .read_block(reg::MEASUREMENT_BLOCK_START, reg::MEASUREMENT_BLOCK_LEN)
            .await?;

        Ok(Snapshot {
            systime: clock.i32_at(reg::SYSTIME)?,
            frequency: measurements.f32_at(reg::FREQUENCY)?,
            voltage: measurements.phases_at(reg::VOLTAGE_L1)?,
            current: measurements.phases_at(reg::CURRENT_L1)?,
            real_power: measurements.phases_at(reg::REAL_POWER_L1)?,
            real_power_sum3: measurements.f32_at(reg::REAL_POWER_SUM3)?,
            apparent_power: measurements.phases_at(reg::APPARENT_POWER_L1)?,
            apparent_power_sum3: measurements.f32_at(reg::APPARENT_POWER_SUM3)?,
            reactive_power: measurements.phases_at(reg::REACTIVE_POWER_L1)?,
            reactive_power_sum3: measurements.f32_at(reg::REACTIVE_POWER_SUM3)?,
            cos_phi: measurements.phases_at(reg::COS_PHI_L1)?,
            real_energy_consumed: measurements.phases_at(reg::REAL_ENERGY_CONSUMED_L1)?,
            thd_voltage: measurements.phases_at(reg::THD_VOLTAGE_L1)?,
            thd_current: measurements.phases_at(reg::THD_CURRENT_L1)?,
        })
    }
}

// Register reading functions for the Umg605Pro device. Each costs one round trip, so
// prefer `snapshot` when more than a single value is needed.
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

    /// The measured three-phase real power P1+P2+P3.
    pub async fn real_power_sum3(&mut self) -> Result<f32, ReadError> {
        self.read_f32(reg::REAL_POWER_SUM3).await
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

    pub async fn thd_voltage_l1(&mut self) -> Result<f32, ReadError> {
        self.read_f32(reg::THD_VOLTAGE_L1).await
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
    fn decodes_all_three_phases_of_a_quantity() {
        let mut registers = Vec::new();
        for value in [230.0_f32, 231.0, 232.0] {
            let (hi, lo) = encode_f32(value);
            registers.push(hi);
            registers.push(lo);
        }
        let block = RegisterBlock::new(reg::VOLTAGE_L1, registers);

        assert_eq!(
            block.phases_at(reg::VOLTAGE_L1).unwrap(),
            [230.0, 231.0, 232.0]
        );
    }

    #[test]
    fn phases_report_unavailable_values_as_nan() {
        // An unconnected phase makes the meter report NaN, which must survive decoding
        // rather than turning into an error or a zero.
        let mut registers = Vec::new();
        for value in [1.85_f32, f32::NAN, f32::NAN] {
            let (hi, lo) = encode_f32(value);
            registers.push(hi);
            registers.push(lo);
        }
        let block = RegisterBlock::new(reg::THD_VOLTAGE_L1, registers);

        let thd = block.phases_at(reg::THD_VOLTAGE_L1).unwrap();
        assert_eq!(thd[0], 1.85);
        assert!(thd[1].is_nan() && thd[2].is_nan());
    }

    #[test]
    fn phases_outside_the_block_are_rejected() {
        // Only L1 and L2 fit, so reading the quantity as three phases must fail.
        let block = RegisterBlock::new(reg::VOLTAGE_L1, vec![0; 4]);
        assert!(matches!(
            block.phases_at(reg::VOLTAGE_L1),
            Err(ReadError::DecodeError(_))
        ));
    }

    #[test]
    fn measurement_block_covers_every_snapshot_value_within_the_modbus_limit() {
        // The block has to reach from the first voltage to the last THD register.
        assert_eq!(reg::MEASUREMENT_BLOCK_START, reg::VOLTAGE_L1);
        assert_eq!(reg::MEASUREMENT_BLOCK_LEN, 122);

        let end = reg::MEASUREMENT_BLOCK_START + reg::MEASUREMENT_BLOCK_LEN;
        for addr in [
            reg::VOLTAGE_L1,
            reg::CURRENT_L1,
            reg::REAL_POWER_L1,
            reg::REAL_POWER_SUM3,
            reg::APPARENT_POWER_L1,
            reg::APPARENT_POWER_SUM3,
            reg::REACTIVE_POWER_L1,
            reg::REACTIVE_POWER_SUM3,
            reg::COS_PHI_L1,
            reg::FREQUENCY,
            reg::REAL_ENERGY_CONSUMED_L1,
            reg::THD_VOLTAGE_L1,
            reg::THD_CURRENT_L1,
        ] {
            let last_phase = addr + reg::PHASE_STRIDE * (PHASE_COUNT as u16 - 1);
            assert!(
                addr >= reg::MEASUREMENT_BLOCK_START && last_phase + 2 <= end,
                "{addr} is not covered by the measurement block"
            );
        }
    }
}
