const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
pub(super) const BLOCK_LEN: usize = 128;
const DESCRIPTOR_OFFSETS: [usize; 4] = [54, 72, 90, 108];
const DESCRIPTOR_LEN: usize = 18;
const DISPLAY_NAME_TAG: u8 = 0xFC;
const SERIAL_NUMBER_TAG: u8 = 0xFF;

#[derive(Debug, Eq, PartialEq)]
pub(super) struct Edid {
    pub(super) manufacturer: String,
    pub(super) product_code: u16,
    pub(super) serial_number: u32,
    pub(super) name: Option<String>,
    pub(super) serial_text: Option<String>,
}

impl Edid {
    pub(super) fn display_name(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("{} {:04X}", self.manufacturer, self.product_code))
    }

    pub(super) fn serial(&self) -> Option<String> {
        self.serial_text
            .clone()
            .or_else(|| (self.serial_number != 0).then(|| self.serial_number.to_string()))
    }
}

/// Parses the base EDID block. The checksum is not enforced because some
/// monitors ship with a bad one while still reporting usable identifiers.
pub(super) fn parse(bytes: &[u8]) -> Option<Edid> {
    let block = bytes.get(..BLOCK_LEN)?;
    if block[..HEADER.len()] != HEADER {
        return None;
    }

    let manufacturer_bits = u16::from_be_bytes([block[8], block[9]]);
    let manufacturer = [10, 5, 0]
        .iter()
        .map(|shift| {
            let letter = ((manufacturer_bits >> shift) & 0x1F) as u8;
            if (1..=26).contains(&letter) {
                char::from(b'A' + letter - 1)
            } else {
                '?'
            }
        })
        .collect();

    let mut name = None;
    let mut serial_text = None;
    for offset in DESCRIPTOR_OFFSETS {
        let descriptor = &block[offset..offset + DESCRIPTOR_LEN];
        if descriptor[..3] != [0, 0, 0] {
            continue;
        }
        match descriptor[3] {
            DISPLAY_NAME_TAG => name = descriptor_text(descriptor),
            SERIAL_NUMBER_TAG => serial_text = descriptor_text(descriptor),
            _ => {}
        }
    }

    Some(Edid {
        manufacturer,
        product_code: u16::from_le_bytes([block[10], block[11]]),
        serial_number: u32::from_le_bytes([block[12], block[13], block[14], block[15]]),
        name,
        serial_text,
    })
}

fn descriptor_text(descriptor: &[u8]) -> Option<String> {
    let text = &descriptor[5..];
    let end = text
        .iter()
        .position(|&byte| byte == b'\n' || byte == 0)
        .unwrap_or(text.len());
    let text = String::from_utf8_lossy(&text[..end]).trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::{BLOCK_LEN, HEADER, parse};

    fn sample_edid(name: &str, serial: &str) -> Vec<u8> {
        let mut block = vec![0u8; BLOCK_LEN];
        block[..8].copy_from_slice(&HEADER);
        // "DEL" packed as 5-bit letters.
        block[8] = 0x10;
        block[9] = 0xAC;
        block[10..12].copy_from_slice(&0xA0F1u16.to_le_bytes());
        block[12..16].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        write_descriptor(&mut block, 90, 0xFF, serial);
        write_descriptor(&mut block, 108, 0xFC, name);
        block
    }

    fn write_descriptor(block: &mut [u8], offset: usize, tag: u8, text: &str) {
        block[offset + 3] = tag;
        let text_area = &mut block[offset + 5..offset + 18];
        text_area.fill(b' ');
        text_area[..text.len()].copy_from_slice(text.as_bytes());
        if text.len() < text_area.len() {
            text_area[text.len()] = b'\n';
        }
    }

    #[test]
    fn parses_identity_and_text_descriptors() {
        let edid = parse(&sample_edid("DELL U2520D", "4P9K923")).expect("valid EDID");
        assert_eq!(edid.manufacturer, "DEL");
        assert_eq!(edid.product_code, 0xA0F1);
        assert_eq!(edid.serial_number, 0x1234_5678);
        assert_eq!(edid.display_name(), "DELL U2520D");
        assert_eq!(edid.serial().as_deref(), Some("4P9K923"));
    }

    #[test]
    fn falls_back_to_numeric_identity() {
        let mut bytes = sample_edid("X", "Y");
        bytes[90..BLOCK_LEN].fill(0x01);
        let edid = parse(&bytes).expect("valid EDID");
        assert_eq!(edid.display_name(), "DEL A0F1");
        assert_eq!(edid.serial().as_deref(), Some("305419896"));
    }

    #[test]
    fn rejects_missing_header_and_short_input() {
        let mut bytes = sample_edid("X", "Y");
        assert!(parse(&bytes[..64]).is_none());
        bytes[0] = 0xFF;
        assert!(parse(&bytes).is_none());
    }
}
