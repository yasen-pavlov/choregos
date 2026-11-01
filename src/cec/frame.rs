use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Frame {
    pub from: u8,
    pub to: u8,
    pub opcode: Option<u8>,
    pub params: Vec<u8>,
    pub ack: Option<bool>,
    pub t_us: Option<u64>,
}

impl Frame {
    pub fn parse(payload: &[u8]) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct Envelope {
            #[serde(rename = "HdmiReceived")]
            frame: Received,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct Received {
            from: u8,
            to: u8,
            data: String,
        }
        let received = serde_json::from_slice::<Envelope>(payload)
            .map_err(|_| "invalid HdmiReceived JSON")?
            .frame;
        Self::from_hex(received.from, received.to, &received.data)
    }

    pub fn from_hex(from: u8, to: u8, data: &str) -> Result<Self, String> {
        if from > 15 || to > 15 {
            return Err("logical address outside 0–15".into());
        }
        let bytes = hex::decode(data).map_err(|_| "invalid CEC hex")?;
        if bytes.len() > 15 {
            return Err("CEC payload exceeds 15 bytes".into());
        }
        Ok(Self {
            from,
            to,
            opcode: bytes.first().copied(),
            params: bytes.get(1..).unwrap_or_default().to_vec(),
            ack: None,
            t_us: None,
        })
    }

    pub fn data(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        let mut data =
            String::with_capacity((self.params.len() + usize::from(self.opcode.is_some())) * 2);
        for &byte in self.opcode.iter().chain(&self.params) {
            data.push(char::from(HEX[usize::from(byte >> 4)]));
            data.push(char::from(HEX[usize::from(byte & 15)]));
        }
        data
    }

    pub fn to_hdmisend(&self) -> serde_json::Value {
        serde_json::json!({"To": self.to, "Data": self.data()})
    }

    pub fn physical(&self) -> Option<u16> {
        self.params
            .get(..2)
            .map(|p| u16::from_be_bytes([p[0], p[1]]))
    }
}

pub fn physical_address(value: u16) -> String {
    format!(
        "{:X}.{:X}.{:X}.{:X}",
        value >> 12,
        (value >> 8) & 15,
        (value >> 4) & 15,
        value & 15
    )
}

pub fn parse_physical(value: &str) -> Option<u16> {
    let mut result = 0;
    let mut count = 0;
    for part in value.split('.') {
        if part.len() != 1 {
            return None;
        }
        result = (result << 4) | u16::from_str_radix(part, 16).ok()?;
        count += 1;
    }
    (count == 4).then_some(result)
}

pub fn decode_osd(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_matches(|c: char| c == '\0' || c.is_whitespace())
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_and_boundaries() {
        let f = Frame::parse(br#"{"HdmiReceived":{"From":4,"To":15,"Data":"822100"}}"#).unwrap();
        assert_eq!(f.physical(), Some(0x2100));
        assert_eq!(
            f.to_hdmisend(),
            serde_json::json!({"To":15,"Data":"822100"})
        );
        assert_eq!(parse_physical(&physical_address(0x2a10)), Some(0x2a10));
        assert_eq!(parse_physical("2.1.0"), None);
        assert!(Frame::from_hex(16, 0, "04").is_err());
        assert!(Frame::from_hex(4, 0, "0").is_err());
        assert_eq!(Frame::from_hex(4, 0, "").unwrap().opcode, None);
        assert_eq!(decode_osd(b" Apple TV\0"), "Apple TV");
    }
}
