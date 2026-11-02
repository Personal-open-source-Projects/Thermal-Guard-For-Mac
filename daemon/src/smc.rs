//! Direct AppleSMC access over IOKit.
//!
//! We talk to the SMC ourselves (no helper libraries) so the app has no runtime
//! dependency beyond the system frameworks that ship with macOS.
//!
//! Reading the SMC requires root. Writing fan keys requires root *and* SMC
//! support; on MacBookAir9,1 the fan is not addressable, which we detect by
//! writing a target and reading it back instead of assuming success.

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};

type KernReturn = c_int;
type IoObject = u32;
type IoConnect = u32;
type CFMutableDictionary = *mut c_void;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CFMutableDictionary;
    fn IOServiceGetMatchingService(main_port: u32, matching: CFMutableDictionary) -> IoObject;
    fn IOServiceOpen(
        service: IoObject,
        owning_task: u32,
        type_: u32,
        connect: *mut IoConnect,
    ) -> KernReturn;
    fn IOServiceClose(connect: IoConnect) -> KernReturn;
    fn IOObjectRelease(object: IoObject) -> KernReturn;
    fn IOConnectCallStructMethod(
        connection: IoConnect,
        selector: u32,
        input: *const c_void,
        input_cnt: usize,
        output: *mut c_void,
        output_cnt: *mut usize,
    ) -> KernReturn;
}

extern "C" {
    static mach_task_self_: u32;
}

const KERNEL_INDEX_SMC: u32 = 2;
const SMC_CMD_READ_BYTES: u8 = 5;
const SMC_CMD_WRITE_BYTES: u8 = 6;
const SMC_CMD_READ_KEYINFO: u8 = 9;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Vers {
    major: u8,
    minor: u8,
    build: u8,
    reserved: u8,
    release: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PLimitData {
    version: u16,
    length: u16,
    cpu_plimit: u32,
    gpu_plimit: u32,
    mem_plimit: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct KeyInfo {
    data_size: u32,
    data_type: u32,
    data_attributes: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct KeyData {
    key: u32,
    vers: Vers,
    p_limit_data: PLimitData,
    key_info: KeyInfo,
    result: u8,
    status: u8,
    data8: u8,
    data32: u32,
    bytes: [u8; 32],
}

fn fourcc(key: &str) -> Option<u32> {
    let b = key.as_bytes();
    if b.len() != 4 {
        return None;
    }
    Some(((b[0] as u32) << 24) | ((b[1] as u32) << 16) | ((b[2] as u32) << 8) | (b[3] as u32))
}

fn u32_to_fourcc(v: u32) -> String {
    let mut s = String::new();
    for shift in [24u32, 16, 8, 0] {
        let byte = ((v >> shift) & 0xff) as u8;
        s.push(if byte.is_ascii_graphic() || byte == b' ' {
            byte as char
        } else {
            '?'
        });
    }
    s
}

pub struct Smc {
    conn: IoConnect,
}

impl Smc {
    pub fn open() -> Result<Smc, String> {
        unsafe {
            let name = CString::new("AppleSMC").unwrap();
            let matching = IOServiceMatching(name.as_ptr());
            if matching.is_null() {
                return Err("IOServiceMatching(AppleSMC) failed".into());
            }
            let service = IOServiceGetMatchingService(0, matching);
            if service == 0 {
                return Err("AppleSMC service not found".into());
            }
            let mut conn: IoConnect = 0;
            let kr = IOServiceOpen(service, mach_task_self_, 0, &mut conn);
            IOObjectRelease(service);
            if kr != 0 || conn == 0 {
                return Err(format!(
                    "IOServiceOpen(AppleSMC) failed (0x{:x}) - needs root",
                    kr
                ));
            }
            Ok(Smc { conn })
        }
    }

    fn call(&self, input: &mut KeyData) -> Result<KeyData, String> {
        let in_size = std::mem::size_of::<KeyData>();
        let mut out = KeyData::default();
        let mut out_size = std::mem::size_of::<KeyData>();
        let kr = unsafe {
            IOConnectCallStructMethod(
                self.conn,
                KERNEL_INDEX_SMC,
                input as *const KeyData as *const c_void,
                in_size,
                &mut out as *mut KeyData as *mut c_void,
                &mut out_size,
            )
        };
        if kr != 0 {
            return Err(format!("SMC call failed (0x{:x})", kr));
        }
        if out.result != 0 {
            return Err(format!("SMC result {}", out.result));
        }
        Ok(out)
    }

    fn key_info(&self, key: &str) -> Result<KeyInfo, String> {
        let k = fourcc(key).ok_or_else(|| format!("bad key {}", key))?;
        let mut input = KeyData::default();
        input.key = k;
        input.data8 = SMC_CMD_READ_KEYINFO;
        let out = self.call(&mut input)?;
        Ok(out.key_info)
    }

    /// Returns the SMC data type tag (e.g. "sp78") and raw bytes.
    pub fn read_raw(&self, key: &str) -> Result<(String, Vec<u8>), String> {
        let info = self.key_info(key)?;
        let k = fourcc(key).ok_or_else(|| format!("bad key {}", key))?;
        let mut input = KeyData::default();
        input.key = k;
        input.key_info.data_size = info.data_size;
        input.data8 = SMC_CMD_READ_BYTES;
        let out = self.call(&mut input)?;
        let len = (info.data_size as usize).min(32);
        Ok((u32_to_fourcc(info.data_type), out.bytes[..len].to_vec()))
    }

    pub fn write_raw(&self, key: &str, type_tag: &str, bytes: &[u8]) -> Result<(), String> {
        let info = self.key_info(key)?;
        let k = fourcc(key).ok_or_else(|| format!("bad key {}", key))?;
        let _ = type_tag;
        let mut input = KeyData::default();
        input.key = k;
        input.key_info.data_size = info.data_size;
        input.data8 = SMC_CMD_WRITE_BYTES;
        let n = (bytes.len()).min(32).min(info.data_size as usize);
        input.bytes[..n].copy_from_slice(&bytes[..n]);
        self.call(&mut input)?;
        Ok(())
    }

    pub fn read_f64(&self, key: &str) -> Option<f64> {
        let (tag, raw) = self.read_raw(key).ok()?;
        decode(&tag, &raw)
    }
}

impl Drop for Smc {
    fn drop(&mut self) {
        unsafe {
            IOServiceClose(self.conn);
        }
    }
}

/// Decode the handful of SMC data types that carry temperature and fan values.
pub fn decode(tag: &str, raw: &[u8]) -> Option<f64> {
    match tag {
        "sp78" => {
            if raw.len() < 2 {
                return None;
            }
            let v = (((raw[0] as u16) << 8) | raw[1] as u16) as i16;
            Some(v as f64 / 256.0)
        }
        "sp87" | "sp5a" | "sp4b" => {
            if raw.len() < 2 {
                return None;
            }
            let v = (((raw[0] as u16) << 8) | raw[1] as u16) as i16;
            Some(v as f64 / 256.0)
        }
        "fpe2" => {
            if raw.len() < 2 {
                return None;
            }
            let v = ((raw[0] as u16) << 8) | raw[1] as u16;
            Some(v as f64 / 4.0)
        }
        "fp2e" => {
            if raw.len() < 2 {
                return None;
            }
            let v = ((raw[0] as u16) << 8) | raw[1] as u16;
            Some(v as f64 / 16384.0)
        }
        "flt " => {
            if raw.len() < 4 {
                return None;
            }
            Some(f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as f64)
        }
        "ui8 " => raw.first().map(|v| *v as f64),
        "ui16" => {
            if raw.len() < 2 {
                return None;
            }
            Some((((raw[0] as u16) << 8) | raw[1] as u16) as f64)
        }
        "ui32" => {
            if raw.len() < 4 {
                return None;
            }
            Some(
                (((raw[0] as u32) << 24)
                    | ((raw[1] as u32) << 16)
                    | ((raw[2] as u32) << 8)
                    | raw[3] as u32) as f64,
            )
        }
        "si8 " => raw.first().map(|v| *v as i8 as f64),
        _ => None,
    }
}

pub fn encode_fpe2(v: f64) -> Vec<u8> {
    let raw = (v * 4.0).round().clamp(0.0, 65535.0) as u16;
    vec![(raw >> 8) as u8, (raw & 0xff) as u8]
}

pub fn encode_flt(v: f64) -> Vec<u8> {
    (v as f32).to_le_bytes().to_vec()
}

/// Candidate CPU temperature sensors, in preference order. Different Intel
/// Macs expose different subsets; the first one that answers wins.
pub const CPU_TEMP_KEYS: &[&str] = &[
    "TC0P", "TC0D", "TC0E", "TC0F", "TC0H", "TCXC", "TCAP", "Tp09", "Tp0T", "Tp05", "Tp0b",
];
pub const BATTERY_TEMP_KEYS: &[&str] = &["TB0T", "TB1T", "TB2T"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_fixed_point_temperature() {
        // sp78: 78/256 degrees per LSB. 0x4A80 = 74.5 C
        assert!((decode("sp78", &[0x4A, 0x80]).unwrap() - 74.5).abs() < 0.01);
        // negative values stay negative
        assert!((decode("sp78", &[0xFF, 0x00]).unwrap() + 1.0).abs() < 0.01);
    }

    #[test]
    fn decodes_fan_rpm() {
        // fpe2: quarter-RPM units. 0x1770 = 6000/4 => 1500 RPM
        assert_eq!(decode("fpe2", &[0x17, 0x70]).unwrap(), 1500.0);
    }

    #[test]
    fn decodes_float_and_int_types() {
        assert_eq!(decode("flt ", &2000.0f32.to_le_bytes()).unwrap(), 2000.0);
        assert_eq!(decode("ui8 ", &[42]).unwrap(), 42.0);
        assert_eq!(decode("ui16", &[0x01, 0x2C]).unwrap(), 300.0);
    }

    #[test]
    fn unknown_or_short_data_is_rejected_not_guessed() {
        assert!(decode("sp78", &[0x01]).is_none());
        assert!(decode("zzzz", &[1, 2, 3]).is_none());
    }

    #[test]
    fn encodes_fan_target_round_trip() {
        let bytes = encode_fpe2(3000.0);
        assert_eq!(decode("fpe2", &bytes).unwrap(), 3000.0);
    }

    #[test]
    fn fourcc_only_accepts_four_bytes() {
        assert!(fourcc("F0Ac").is_some());
        assert!(fourcc("F0A").is_none());
    }
}
