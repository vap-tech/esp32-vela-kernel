//! Userspace INA226 power monitor driver for M5Stack Tab5 (`0x41`, `5 mOhm` shunt).
//!
//! On M5Stack Tab5 the INA226 sensor sits in the removable 2S NP-F550 battery
//! rail (nominal 7.4 V, operating range ~6.0 V..8.4 V) rather than on the 5 V
//! USB-C input:
//! - When powered from USB-C with no battery attached (`vbus_mv < 5000`), the
//!   battery rail reads a low floating/open-circuit voltage and ~0 mA shunt
//!   current (`supply=usb_only`, `battery=absent`).
//! - When a 2S battery is attached (`vbus_mv >= 5000`), positive shunt current
//!   (`current_ma > 10`) indicates net battery discharge (`supply=battery_discharging`,
//!   `battery=discharging`), negative shunt current (`current_ma < -10`) indicates
//!   battery charging from USB-C (`supply=usb_charging`, `battery=charging`), and
//!   near-zero shunt current (`|current_ma| <= 10`) indicates the board is running
//!   on USB-C with an idle/full battery (`supply=usb_idle`, `battery=idle`).

use crate::sys;

pub const DEFAULT_ADDR: u8 = 0x41;
pub const TAB5_SHUNT_MOHM: u32 = 5;

pub const REG_CONFIG: u8 = 0x00;
pub const REG_SHUNT_V: u8 = 0x01;
pub const REG_BUS_V: u8 = 0x02;
pub const REG_POWER: u8 = 0x03;
pub const REG_CURRENT: u8 = 0x04;
pub const REG_CALIB: u8 = 0x05;
pub const REG_MFG_ID: u8 = 0xfe;
pub const REG_DIE_ID: u8 = 0xff;

pub const MFG_ID_TI: u16 = 0x5449; // "TI"
pub const DIE_ID_INA226_MASK: u16 = 0xfff0;
pub const DIE_ID_INA226: u16 = 0x2260;

/// 16-sample averaging, 1.1 ms VBUS/VSHUNT conversion time, continuous shunt + bus mode.
pub const CONFIG_AVG16_CONT: u16 = 0x4527;

/// Exact calibration for `R_shunt = 5 mOhm` (`0.005 Ohm`) and `Current_LSB = 0.5 mA` (`500 uA`):
/// `CAL = 0.00512 / (0.0005 * 0.005) = 2048` (`0x0800`), giving `Power_LSB = 25 * 0.5 mA = 12.5 mW`.
pub const CALIB_5MOHM_500UA: u16 = 2048;

/// Minimum bus voltage (in mV) for a connected 2S Li-ion (NP-F550) battery rail.
pub const BATTERY_2S_MIN_MV: u32 = 5000;

/// Current deadband (in mA) around zero for an idle battery when the board runs from USB-C.
pub const CURRENT_DEADBAND_MA: i32 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupplyState {
    UsbOnly,
    UsbCharging,
    UsbIdle,
    BatteryDischarging,
}

impl SupplyState {
    pub const fn as_bytes(self) -> &'static [u8] {
        match self {
            Self::UsbOnly => b"usb_only",
            Self::UsbCharging => b"usb_charging",
            Self::UsbIdle => b"usb_idle",
            Self::BatteryDischarging => b"battery_discharging",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatteryState {
    Absent,
    Charging,
    Discharging,
    Idle,
}

impl BatteryState {
    pub const fn as_bytes(self) -> &'static [u8] {
        match self {
            Self::Absent => b"absent",
            Self::Charging => b"charging",
            Self::Discharging => b"discharging",
            Self::Idle => b"idle",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    pub mfg_id: u16,
    pub die_id: u16,
    pub vbus_mv: u32,
    pub vshunt_uv: i32,
    /// Signed battery current in mA (`+` = battery discharging, `-` = battery charging).
    pub current_ma: i32,
    pub power_mw: u32,
    pub supply: SupplyState,
    pub battery: BatteryState,
}

impl Reading {
    pub const fn is_valid_id(mfg_id: u16, die_id: u16) -> bool {
        mfg_id == MFG_ID_TI && (die_id & DIE_ID_INA226_MASK) == DIE_ID_INA226
    }

    pub const fn from_raw(
        mfg_id: u16,
        die_id: u16,
        raw_shunt: i16,
        raw_bus: u16,
        raw_power: u16,
        raw_current: i16,
    ) -> Option<Self> {
        if !Self::is_valid_id(mfg_id, die_id) {
            return None;
        }
        let bus_lsb = (raw_bus & 0x7fff) as u32;
        // 1 LSB = 1.25 mV = 5/4 mV (rounded to nearest mV)
        let vbus_mv = (bus_lsb * 5 + 2) / 4;
        // 1 LSB = 2.5 uV = 5/2 uV
        let vshunt_uv = (raw_shunt as i32 * 5) / 2;
        // With R_shunt = 5 mOhm and CAL = 2048 (Current_LSB = 0.5 mA):
        // 1 LSB of shunt voltage (2.5 uV / 5 mOhm) is also 0.5 mA.
        let half_ma = if raw_current != 0 {
            raw_current as i32
        } else {
            raw_shunt as i32
        };
        let current_ma = half_ma / 2;
        // Power_LSB = 25 * Current_LSB = 12.5 mW = 25/2 mW
        let power_mw = if current_ma == 0 {
            0
        } else if raw_power != 0 {
            (raw_power as u32 * 25 + 1) / 2
        } else {
            (vbus_mv * current_ma.unsigned_abs() + 500) / 1000
        };
        let (supply, battery) = if vbus_mv < BATTERY_2S_MIN_MV {
            (SupplyState::UsbOnly, BatteryState::Absent)
        } else if current_ma < -CURRENT_DEADBAND_MA {
            (SupplyState::UsbCharging, BatteryState::Charging)
        } else if current_ma > CURRENT_DEADBAND_MA {
            (SupplyState::BatteryDischarging, BatteryState::Discharging)
        } else {
            (SupplyState::UsbIdle, BatteryState::Idle)
        };
        Some(Self {
            mfg_id,
            die_id,
            vbus_mv,
            vshunt_uv,
            current_ma,
            power_mw,
            supply,
            battery,
        })
    }
}

pub fn read_reg(i2c_handle: u32, addr: u8, reg: u8) -> Result<u16, i32> {
    let mut rx = [0u8; 2];
    sys::i2c_transfer(i2c_handle, addr, &[reg], &mut rx)?;
    Ok(u16::from_be_bytes(rx))
}

pub fn write_reg(i2c_handle: u32, addr: u8, reg: u8, value: u16) -> Result<(), i32> {
    let [msb, lsb] = value.to_be_bytes();
    let mut empty = [];
    sys::i2c_transfer(i2c_handle, addr, &[reg, msb, lsb], &mut empty)
}

pub fn sample_tab5(i2c_handle: u32) -> Result<Reading, i32> {
    let mfg_id = read_reg(i2c_handle, DEFAULT_ADDR, REG_MFG_ID)?;
    let die_id = read_reg(i2c_handle, DEFAULT_ADDR, REG_DIE_ID)?;
    if !Reading::is_valid_id(mfg_id, die_id) {
        return Err(-19); // -ENODEV
    }
    let cfg = read_reg(i2c_handle, DEFAULT_ADDR, REG_CONFIG)?;
    let cal = read_reg(i2c_handle, DEFAULT_ADDR, REG_CALIB)?;
    if cal != CALIB_5MOHM_500UA {
        write_reg(i2c_handle, DEFAULT_ADDR, REG_CALIB, CALIB_5MOHM_500UA)?;
    }
    if cfg != CONFIG_AVG16_CONT {
        write_reg(i2c_handle, DEFAULT_ADDR, REG_CONFIG, CONFIG_AVG16_CONT)?;
    }
    let raw_shunt = read_reg(i2c_handle, DEFAULT_ADDR, REG_SHUNT_V)? as i16;
    let raw_bus = read_reg(i2c_handle, DEFAULT_ADDR, REG_BUS_V)?;
    let raw_power = read_reg(i2c_handle, DEFAULT_ADDR, REG_POWER)?;
    let raw_current = read_reg(i2c_handle, DEFAULT_ADDR, REG_CURRENT)? as i16;
    Reading::from_raw(mfg_id, die_id, raw_shunt, raw_bus, raw_power, raw_current).ok_or(-19)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_ina226_ids() {
        assert!(Reading::from_raw(0x0000, 0x2260, 0, 0, 0, 0).is_none());
        assert!(Reading::from_raw(0x5449, 0x2190, 0, 0, 0, 0).is_none());
    }

    #[test]
    fn classifies_usb_only_when_battery_rail_is_below_2s_threshold() {
        // 800 * 1.25 mV = 1000 mV floating rail, 0 uV shunt, 1 LSB (0.5 mA) noise
        let r = Reading::from_raw(0x5449, 0x2260, 0, 800, 0, 1).unwrap();
        assert_eq!(r.vbus_mv, 1000);
        assert_eq!(r.vshunt_uv, 0);
        assert_eq!(r.current_ma, 0);
        assert_eq!(r.power_mw, 0);
        assert_eq!(r.supply, SupplyState::UsbOnly);
        assert_eq!(r.battery, BatteryState::Absent);
    }

    #[test]
    fn decodes_battery_discharging_charging_and_usb_idle_states() {
        // 2S battery at 7.400 V (5920 * 1.25 mV = 7400 mV), discharging at 400 mA
        // (raw_shunt = +800 -> +2000 uV across 5 mOhm -> +400 mA, raw_power = 237 -> 2963 mW)
        let dis = Reading::from_raw(0x5449, 0x2260, 800, 5920, 237, 800).unwrap();
        assert_eq!(dis.vbus_mv, 7400);
        assert_eq!(dis.vshunt_uv, 2000);
        assert_eq!(dis.current_ma, 400);
        assert_eq!(dis.power_mw, 2963);
        assert_eq!(dis.supply, SupplyState::BatteryDischarging);
        assert_eq!(dis.battery, BatteryState::Discharging);

        // 2S battery at 8.100 V (6480 * 1.25 mV = 8100 mV), charging at -500 mA
        // (raw_shunt = -1000 -> -2500 uV across 5 mOhm -> -500 mA, fallback power when raw_power=0)
        let chg = Reading::from_raw(0x5449, 0x2260, -1000, 6480, 0, 0).unwrap();
        assert_eq!(chg.vbus_mv, 8100);
        assert_eq!(chg.vshunt_uv, -2500);
        assert_eq!(chg.current_ma, -500);
        assert_eq!(chg.power_mw, 4050);
        assert_eq!(chg.supply, SupplyState::UsbCharging);
        assert_eq!(chg.battery, BatteryState::Charging);

        // 2S battery at 8.350 V (6680 * 1.25 mV = 8350 mV), idle (+2 mA within deadband)
        let idle = Reading::from_raw(0x5449, 0x2260, 4, 6680, 0, 4).unwrap();
        assert_eq!(idle.vbus_mv, 8350);
        assert_eq!(idle.current_ma, 2);
        assert_eq!(idle.supply, SupplyState::UsbIdle);
        assert_eq!(idle.battery, BatteryState::Idle);
    }
}
